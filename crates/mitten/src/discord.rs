//! Discord gateway: one agent per DM channel, fed by an allow-listed set of users.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serenity::all::{ChannelId, Client, Context, EventHandler, GatewayIntents, Http, Message};
use serenity::async_trait;
use tokio::sync::mpsc;

use crate::agent::{Agent, Io};
use crate::config::Config;
use crate::db::Db;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Discord rejects messages over 2000 characters.
const MAX_MESSAGE_CHARS: usize = 1_900;

/// Connects to Discord and serves DMs until the connection fails.
pub async fn serve(config: Config) -> Result<()> {
    let Some(discord) = config.discord.clone() else {
        bail!("`mitten serve` needs a [discord] section in the config");
    };
    let db = Db::open(&config.database_path)?;
    let handler = Handler {
        config,
        db,
        allowed_users: discord.allowed_users,
        channels: Mutex::new(HashMap::new()),
    };
    // ponytail: DMs only; add GUILD_MESSAGES + MESSAGE_CONTENT and mention filtering for server channels.
    let mut client = Client::builder(discord.token.as_str(), GatewayIntents::DIRECT_MESSAGES)
        .event_handler(handler)
        .await
        .context("failed to build Discord client")?;
    client.start().await.context("Discord connection failed")
}

struct Handler {
    config: Config,
    db: Db,
    allowed_users: Vec<u64>,
    /// Inbox of the task that owns each DM channel's conversation.
    channels: Mutex<HashMap<ChannelId, mpsc::UnboundedSender<String>>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot
            || msg.guild_id.is_some()
            || !self.allowed_users.contains(&msg.author.id.get())
        {
            return;
        }
        let Ok(mut channels) = self.channels.lock() else {
            tracing::error!("channel map lock poisoned");
            return;
        };
        let inbox = channels.entry(msg.channel_id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(run_channel(
                self.config.clone(),
                self.db.clone(),
                ctx.http.clone(),
                msg.channel_id,
                rx,
            ));
            tx
        });
        if inbox.send(msg.content).is_err() {
            // The channel task died; drop it so the next message starts a fresh one.
            channels.remove(&msg.channel_id);
        }
    }
}

#[tracing::instrument(skip_all, fields(channel = %channel))]
async fn run_channel(
    config: Config,
    db: Db,
    http: Arc<Http>,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<String>,
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
    };
    while let Some(prompt) = io.inbox.recv().await {
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
    inbox: mpsc::UnboundedReceiver<String>,
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
        self.say(&format!("```sh\n{command}\n```\nReply `y` to run it."))
            .await?;
        let answer = tokio::time::timeout(CONFIRM_TIMEOUT, self.inbox.recv()).await;
        Ok(matches!(answer, Ok(Some(reply)) if reply.trim().eq_ignore_ascii_case("y")))
    }
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
}
