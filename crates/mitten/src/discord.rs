//! Discord gateway: one agent per channel (DMs and every server channel the bot can read),
//! fed by an allow-listed set of users who approve commands with buttons.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serenity::all::{
    ButtonStyle, ChannelId, Client, ComponentInteraction, Context, CreateActionRow, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, EditMessage,
    EventHandler, GatewayIntents, Http, Interaction, Message, MessageId, Ready,
};
use serenity::async_trait;
use tokio::sync::mpsc;

use crate::agent::{Agent, Io};
use crate::config::Config;
use crate::db::Db;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Discord rejects messages over 2000 characters.
const MAX_MESSAGE_CHARS: usize = 1_900;
const RUN: &str = "mitten:run";
const DENY: &str = "mitten:deny";

/// What a channel's agent task receives.
#[derive(Debug)]
enum Input {
    Text(String),
    /// A Run or Deny click on the approval message `message`.
    Decision {
        message: MessageId,
        run: bool,
    },
}

/// Connects to Discord and serves DMs until the connection fails.
pub async fn serve(config: Config) -> Result<()> {
    let Some(discord) = config.discord.clone() else {
        bail!("`mitten serve` needs a [discord] section in the config");
    };
    let db = Db::open(&config.database_path)?;
    // MESSAGE_CONTENT is privileged: enable it for the bot in the Developer Portal, or connecting fails.
    let intents = GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let handler = Handler {
        config,
        db,
        allowed_users: discord.allowed_users,
        channels: Mutex::new(HashMap::new()),
    };
    let mut client = Client::builder(discord.token.as_str(), intents)
        .event_handler(handler)
        .await
        .context("failed to build Discord client")?;
    client.start().await.context("Discord connection failed")
}

struct Handler {
    config: Config,
    db: Db,
    /// The only people whose messages and clicks the bot acts on, in DMs and in every server channel.
    allowed_users: Vec<u64>,
    /// Inbox of the task that owns each channel's conversation.
    channels: Mutex<HashMap<ChannelId, mpsc::UnboundedSender<Input>>>,
}

impl Handler {
    /// Hands `input` to the channel's agent task, starting one for text if none is running.
    fn deliver(&self, http: &Arc<Http>, channel: ChannelId, input: Input) {
        let Ok(mut channels) = self.channels.lock() else {
            tracing::error!("channel map lock poisoned");
            return;
        };
        if !channels.contains_key(&channel) && matches!(input, Input::Decision { .. }) {
            return; // A click on an old approval; nothing is waiting for it.
        }
        let inbox = channels.entry(channel).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(run_channel(
                self.config.clone(),
                self.db.clone(),
                Arc::clone(http),
                channel,
                rx,
            ));
            tx
        });
        if inbox.send(input).is_err() {
            // The channel task died; drop it so the next message starts a fresh one.
            channels.remove(&channel);
        }
    }

    async fn on_click(&self, ctx: &Context, click: &ComponentInteraction) -> Result<()> {
        let run = match click.data.custom_id.as_str() {
            RUN => true,
            DENY => false,
            _ => return Ok(()),
        };
        if !self.allowed_users.contains(&click.user.id.get()) {
            let reply = CreateInteractionResponseMessage::new()
                .content("You are not allowed to approve commands.")
                .ephemeral(true);
            return Ok(click
                .create_response(&ctx.http, CreateInteractionResponse::Message(reply))
                .await?);
        }
        // Answer within Discord's 3-second window: drop the buttons and show the decision.
        let status = if run {
            "▶️ running…"
        } else {
            "✖️ denied"
        };
        let update = CreateInteractionResponseMessage::new()
            .content(format!("{}\n{status}", click.message.content))
            .components(Vec::new());
        click
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(update))
            .await?;
        self.deliver(
            &ctx.http,
            click.channel_id,
            Input::Decision {
                message: click.message.id,
                run,
            },
        );
        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        // No mention needed: every message from an allowed user goes to that channel's agent.
        if msg.author.bot || !self.allowed_users.contains(&msg.author.id.get()) {
            return;
        }
        self.deliver(&ctx.http, msg.channel_id, Input::Text(msg.content));
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        // Printed, not logged, so `mitten serve` shows it at the default log level.
        eprintln!("connected to Discord as {}", ready.user.name);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        if let Interaction::Component(click) = interaction
            && let Err(err) = self.on_click(&ctx, &click).await
        {
            tracing::warn!("failed to handle button click: {err:#}");
        }
    }
}

#[tracing::instrument(skip_all, fields(channel = %channel))]
async fn run_channel(
    config: Config,
    db: Db,
    http: Arc<Http>,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<Input>,
) {
    let mut agent = match Agent::new(config, db, &format!("discord:{channel}")).await {
        Ok(agent) => agent,
        Err(err) => {
            tracing::error!("failed to start agent: {err:#}");
            return;
        }
    };
    let mut io = DiscordIo {
        http,
        channel,
        inbox,
        approval: None,
    };
    while let Some(input) = io.inbox.recv().await {
        let Input::Text(prompt) = input else {
            continue; // A click on an approval that already timed out.
        };
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/new" {
            let reply = match agent.reset().await {
                Ok(()) => "Started a new conversation.".to_owned(),
                Err(err) => format!("error: {err:#}"),
            };
            if let Err(err) = io.say(&reply).await {
                tracing::error!("failed to reply: {err:#}");
            }
            continue;
        }
        let _typing = io.channel.start_typing(&io.http);
        if let Err(err) = agent.run_turn(prompt, &mut io).await {
            tracing::warn!("turn failed: {err:#}");
            if let Err(err) = io.say(&format!("error: {err:#}")).await {
                tracing::error!("failed to report error: {err:#}");
            }
        }
    }
}

struct DiscordIo {
    http: Arc<Http>,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<Input>,
    /// The last approved command's message and its command block, where the output goes.
    approval: Option<(MessageId, String)>,
}

impl DiscordIo {
    /// Replaces the approval message's text and removes its buttons; failures are only logged.
    async fn settle(&self, message: MessageId, content: String) {
        let edit = EditMessage::new().content(content).components(Vec::new());
        if let Err(err) = self.channel.edit_message(&self.http, message, edit).await {
            tracing::warn!("failed to update approval message: {err:#}");
        }
    }
}

impl Io for DiscordIo {
    async fn say(&mut self, text: &str) -> Result<()> {
        for chunk in chunks(text, MAX_MESSAGE_CHARS) {
            self.channel
                .say(&self.http, chunk)
                .await
                .context("failed to send Discord message")?;
        }
        Ok(())
    }

    async fn confirm(&mut self, command: &str) -> Result<bool> {
        let block = code_block("sh", command, MAX_MESSAGE_CHARS / 2);
        let buttons = CreateActionRow::Buttons(vec![
            CreateButton::new(RUN)
                .label("Run")
                .style(ButtonStyle::Success),
            CreateButton::new(DENY)
                .label("Deny")
                .style(ButtonStyle::Danger),
        ]);
        let message = self
            .channel
            .send_message(
                &self.http,
                CreateMessage::new()
                    .content(&block)
                    .components(vec![buttons]),
            )
            .await
            .context("failed to send approval request")?
            .id;
        let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
        let run = loop {
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Err(_) => {
                    self.settle(message, format!("{block}\n⌛ timed out, not run"))
                        .await;
                    return Ok(false);
                }
                Ok(None) => return Ok(false),
                // The click handler already updated the message.
                Ok(Some(Input::Decision { message: id, run })) if id == message => break run,
                Ok(Some(Input::Decision { .. })) => {}
                // Typing an answer still works; anything but `y` is a no.
                Ok(Some(Input::Text(reply))) => {
                    let run = reply.trim().eq_ignore_ascii_case("y");
                    let status = if run {
                        "▶️ running…"
                    } else {
                        "✖️ denied"
                    };
                    self.settle(message, format!("{block}\n{status}")).await;
                    break run;
                }
            }
        };
        if run {
            self.approval = Some((message, block));
        }
        Ok(run)
    }

    async fn ran(&mut self, output: &str, ok: bool) -> Result<()> {
        let Some((message, block)) = self.approval.take() else {
            return Ok(());
        };
        let status = if ok { "✅ done" } else { "❌ failed" };
        let mut content = format!("{block}\n{status}");
        let room = MAX_MESSAGE_CHARS.saturating_sub(content.chars().count() + 16);
        let output = output.trim();
        if !output.is_empty() && room > 100 {
            content.push('\n');
            content.push_str(&code_block("", &tail(output, room), room));
        }
        self.settle(message, content).await;
        Ok(())
    }
}

/// Wraps `text` in a code fence, cut to `max` characters, so it can't break out of the fence.
fn code_block(lang: &str, text: &str, max: usize) -> String {
    let text = text.replace("```", "`\u{200b}``");
    let cut: String = text.chars().take(max).collect();
    let more = if cut.len() < text.len() { "\n…" } else { "" };
    format!("```{lang}\n{cut}{more}\n```")
}

/// The last `max` characters of `text`, marked when cut, since errors usually come last.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - max + 2).collect();
    format!("…\n{kept}")
}

/// Splits `text` into pieces of at most `max` characters.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(max)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_limit_and_keep_all_text() {
        let text = "é".repeat(2 * MAX_MESSAGE_CHARS + 3);
        let parts = chunks(&text, MAX_MESSAGE_CHARS);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_MESSAGE_CHARS));
        assert_eq!(parts.concat(), text);
        assert!(chunks("", MAX_MESSAGE_CHARS).is_empty());
    }

    #[test]
    fn code_block_and_tail_stay_within_limits() {
        assert_eq!(code_block("sh", "ls", 10), "```sh\nls\n```");
        let fenced = code_block("", "a```b", 100);
        assert_eq!(fenced.matches("```").count(), 2, "{fenced}");
        assert!(code_block("", &"x".repeat(50), 10).contains("xxxxxxxxxx\n…"));
        assert_eq!(tail("short", 10), "short");
        let cut = tail(&"é".repeat(50), 10);
        assert!(cut.starts_with("…\n") && cut.chars().count() == 10, "{cut}");
    }
}
