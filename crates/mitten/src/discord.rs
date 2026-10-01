//! Discord gateway: one agent per DM or thread. A message in a server text channel opens a new
//! thread for its conversation. Memory is shared by every conversation. Only allow-listed
//! users are heard, and they approve settings changes with buttons.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use base64::Engine as _;
use rig_core::message::{DocumentSourceKind, Image, ImageMediaType, MimeType as _};
use serenity::all::{
    Attachment, AutoArchiveDuration, ButtonStyle, ChannelId, ChannelType, Client,
    ComponentInteraction, Context, CreateActionRow, CreateAllowedMentions, CreateButton,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateThread,
    EditMessage, EventHandler, GatewayIntents, Http, Interaction, Message, MessageId, Ready,
};
use serenity::async_trait;
use tokio::sync::mpsc;

use crate::agent::{Agent, Io};
use crate::config::Config;
use crate::cron;
use crate::db::{Db, Job};
use crate::mcp::Mcp;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a turn waits for another message before starting, so a burst is answered once.
const MERGE_QUIET: Duration = Duration::from_millis(500);
/// Longest a turn waits to merge, however fast messages keep coming.
const MERGE_CAP: Duration = Duration::from_secs(1);
/// Discord rejects messages over 2000 characters.
const MAX_MESSAGE_CHARS: usize = 1_900;
/// Reaction added to each message the bot picks up, swapped for `DONE` or `FAILED` once handled.
const RECEIVED: char = '👀';
const DONE: char = '✅';
const FAILED: char = '❌';
const RUN: &str = "mitten:run";
const DENY: &str = "mitten:deny";
/// Longest status line kept; statuses are one-line summaries.
const MAX_STATUS_CHARS: usize = 200;
/// How often the scheduler looks for due jobs.
const CRON_TICK: Duration = Duration::from_secs(30);
/// Discord caps thread names at 100 characters.
const MAX_THREAD_NAME_CHARS: usize = 80;
/// Anthropic rejects images over 5 MB; larger attachments are left out.
const MAX_IMAGE_BYTES: u32 = 5 * 1024 * 1024;
/// Largest text attachment read, like the `message.txt` Discord makes from a long paste.
const MAX_TEXT_BYTES: u32 = 200 * 1024;

/// What a channel's agent task receives.
#[derive(Debug)]
enum Input {
    /// A user's message, and where it lives (a thread's first message sits in the parent channel)
    /// so its reaction can be updated once handled.
    Text {
        content: String,
        /// Image and text attachments, downloaded when the turn starts.
        attachments: Vec<Attachment>,
        origin: ChannelId,
        message: MessageId,
    },
    /// A Run or Deny click on the approval message `message`.
    Decision { message: MessageId, run: bool },
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
    let mcp = Arc::new(Mcp::connect(&config.mcp).await);
    let handler = Handler {
        mcp: Arc::clone(&mcp),
        config: config.clone(),
        db: db.clone(),
        allowed_users: discord.allowed_users,
        channels: Mutex::new(HashMap::new()),
    };
    let mut client = Client::builder(discord.token.as_str(), intents)
        .event_handler(handler)
        .await
        .context("failed to build Discord client")?;
    tokio::spawn(run_scheduler(config, db, mcp, Arc::clone(&client.http)));
    client.start().await.context("Discord connection failed")
}

/// Starts every due job, forever. A job missed while mitten was down runs once on startup.
async fn run_scheduler(config: Config, db: Db, mcp: Arc<Mcp>, http: Arc<Http>) {
    let mut tick = tokio::time::interval(CRON_TICK);
    loop {
        tick.tick().await;
        if let Err(err) = start_due_jobs(&config, &db, &mcp, &http).await {
            tracing::warn!("scheduler tick failed: {err:#}");
        }
    }
}

/// Moves each due job to its next run (or deletes a one-off) before starting it, so a slow run
/// can't start twice.
// ponytail: scans every job each tick; query by next_run if job counts grow large.
async fn start_due_jobs(config: &Config, db: &Db, mcp: &Arc<Mcp>, http: &Arc<Http>) -> Result<()> {
    let now = chrono::Local::now();
    for job in db.jobs().await? {
        if job.next_run > now.timestamp() {
            break; // Sorted soonest first.
        }
        match job.schedule.as_deref().map(|s| cron::next_run(s, now)) {
            Some(Ok(next)) => db.set_next_run(job.id, next).await?,
            Some(Err(err)) => {
                tracing::warn!(job = job.id, "dropping job: {err}");
                db.remove_job(job.id).await?;
                continue;
            }
            None => {
                db.remove_job(job.id).await?;
            }
        }
        tokio::spawn(run_job(
            config.clone(),
            db.clone(),
            Arc::clone(mcp),
            Arc::clone(http),
            job,
        ));
    }
    Ok(())
}

/// Runs one job in its own fresh conversation and posts the result where the job was made.
#[tracing::instrument(skip_all, fields(job = job.id))]
async fn run_job(config: Config, db: Db, mcp: Arc<Mcp>, http: Arc<Http>, job: Job) {
    let Some(channel) = job
        .target
        .strip_prefix("discord:")
        .and_then(|id| id.parse().ok())
        .map(ChannelId::new)
    else {
        tracing::warn!(target = job.target, "job target is not a Discord channel");
        return;
    };
    // No one can answer an approval here: with the sender gone, `confirm` sees a closed inbox
    // and denies. Auto approval still reviews and runs.
    let (_, inbox) = mpsc::unbounded_channel();
    let mut io = DiscordIo {
        http,
        web: reqwest::Client::new(),
        channel,
        inbox,
        approval: None,
        status: None,
    };
    let first_line = job.prompt.lines().next().unwrap_or_default();
    let run = async {
        io.note(&format!("⏰ job #{}: {first_line}", job.id))
            .await?;
        let mut agent = Agent::new(config, db, mcp, &format!("cron:{}", job.id)).await?;
        // The last run's transcript stays on disk until the next one starts.
        agent.reset().await?;
        agent
            .run_turn(&cron::prompt(&job), Vec::new(), &mut io)
            .await
    };
    if let Err(err) = run.await {
        tracing::warn!("job failed: {err:#}");
        if let Err(err) = io
            .say(&format!("error: job #{} failed: {err:#}", job.id))
            .await
        {
            tracing::error!("failed to report job failure: {err:#}");
        }
    }
}

struct Handler {
    /// MCP servers, shared by every channel's agent.
    mcp: Arc<Mcp>,
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
                Arc::clone(&self.mcp),
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
                .content("You are not allowed to approve changes.")
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
        // Acknowledge right away so the user knows the bot has it before the model answers.
        if let Err(err) = msg.react(&ctx.http, RECEIVED).await {
            tracing::warn!("failed to react to message: {err:#}");
        }
        let channel = match route(&ctx, &msg).await {
            Ok(channel) => channel,
            Err(err) => {
                tracing::warn!("failed to open a thread, answering in place: {err:#}");
                msg.channel_id
            }
        };
        let attachments = msg
            .attachments
            .into_iter()
            .filter(|a| image_type(a).is_some() || is_text(a))
            .collect();
        let input = Input::Text {
            content: msg.content,
            attachments,
            origin: msg.channel_id,
            message: msg.id,
        };
        self.deliver(&ctx.http, channel, input);
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

/// Where `msg` is answered: a new thread off a message in a server text channel, otherwise
/// the channel itself.
async fn route(ctx: &Context, msg: &Message) -> Result<ChannelId> {
    let here = msg.channel_id;
    if msg.guild_id.is_none() {
        return Ok(here);
    }
    // ponytail: one channel lookup per server message; enable serenity's cache if volume grows.
    let Some(channel) = here.to_channel(&ctx.http).await?.guild() else {
        return Ok(here);
    };
    Ok(match channel.kind {
        ChannelType::Text | ChannelType::News => {
            let thread = CreateThread::new(thread_name(&msg.content))
                .auto_archive_duration(AutoArchiveDuration::OneHour);
            let thread = here
                .create_thread_from_message(&ctx.http, msg.id, thread)
                .await?;
            thread.id
        }
        _ => here,
    })
}

/// The message's first line, shortened to fit a thread name.
fn thread_name(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut name: String = line.chars().take(MAX_THREAD_NAME_CHARS).collect();
    if name.len() < line.len() {
        name.push('…');
    }
    if name.is_empty() {
        name.push_str("mitten");
    }
    name
}

#[tracing::instrument(skip_all, fields(channel = %channel))]
async fn run_channel(
    config: Config,
    db: Db,
    mcp: Arc<Mcp>,
    http: Arc<Http>,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<Input>,
) {
    let key = format!("discord:{channel}");
    let mut agent = match Agent::new(config, db, mcp, &key).await {
        Ok(agent) => agent,
        Err(err) => {
            tracing::error!("failed to start agent: {err:#}");
            return;
        }
    };
    let mut io = DiscordIo {
        http,
        web: reqwest::Client::new(),
        channel,
        inbox,
        approval: None,
        status: None,
    };
    // A `/new` that ended a merged burst, handled next on its own.
    let mut next = None;
    loop {
        let input = match next.take() {
            Some(input) => input,
            None => match io.inbox.recv().await {
                Some(input) => input,
                None => break,
            },
        };
        let Input::Text {
            mut content,
            mut attachments,
            origin,
            message,
        } = input
        else {
            continue; // A click on an approval that already timed out.
        };
        let mut messages = vec![(origin, message)];
        if content.trim() != "/new" {
            next =
                merge_waiting(&mut io.inbox, &mut content, &mut attachments, &mut messages).await;
        }
        let prompt = content.trim();
        if prompt.is_empty() && attachments.is_empty() {
            io.finish_all(&messages, true).await;
            continue;
        }
        if prompt == "/new" {
            let (reply, ok) = match agent.reset().await {
                Ok(()) => ("Started a new conversation.".to_owned(), true),
                Err(err) => (format!("error: {err:#}"), false),
            };
            if let Err(err) = io.say(&reply).await {
                tracing::error!("failed to reply: {err:#}");
            }
            io.finish_all(&messages, ok).await;
            continue;
        }
        io.status = None;
        let typing = io.channel.start_typing(&io.http);
        let result = match download(&io.web, &attachments).await {
            Ok((text, images)) => {
                let prompt = format!("{prompt}{text}");
                agent.run_turn(prompt.trim(), images, &mut io).await
            }
            Err(err) => Err(err),
        };
        drop(typing);
        if let Err(err) = &result {
            tracing::warn!("turn failed: {err:#}");
            if let Err(err) = io.say(&format!("error: {err:#}")).await {
                tracing::error!("failed to report error: {err:#}");
            }
        }
        io.finish_all(&messages, result.is_ok()).await;
    }
}

/// Folds text messages into one prompt until `inbox` stays quiet for `MERGE_QUIET` or `MERGE_CAP`
/// passes, so a burst,
/// or messages sent while a turn ran, is answered once. Stops at a `/new`, which is returned to
/// run after this prompt.
async fn merge_waiting(
    inbox: &mut mpsc::UnboundedReceiver<Input>,
    content: &mut String,
    attachments: &mut Vec<Attachment>,
    messages: &mut Vec<(ChannelId, MessageId)>,
) -> Option<Input> {
    let cap = tokio::time::Instant::now() + MERGE_CAP;
    loop {
        let quiet = tokio::time::Instant::now() + MERGE_QUIET;
        let Ok(Some(input)) = tokio::time::timeout_at(quiet.min(cap), inbox.recv()).await else {
            return None;
        };
        let Input::Text {
            content: more,
            attachments: more_attachments,
            origin,
            message,
        } = input
        else {
            continue;
        };
        if more.trim() == "/new" {
            return Some(Input::Text {
                content: more,
                attachments: more_attachments,
                origin,
                message,
            });
        }
        content.push('\n');
        content.push_str(&more);
        attachments.extend(more_attachments);
        messages.push((origin, message));
    }
}

struct DiscordIo {
    http: Arc<Http>,
    /// Downloads image attachments.
    web: reqwest::Client,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<Input>,
    /// The last approved command's message and its command block, where the output goes.
    approval: Option<(MessageId, String)>,
    /// The status message notes are being collected into, and its text so far.
    status: Option<(MessageId, String)>,
}

impl DiscordIo {
    /// Sends a message that can't ping anyone, whatever the model or a search result put in it.
    async fn send(&self, message: CreateMessage) -> Result<MessageId> {
        let message = message.allowed_mentions(CreateAllowedMentions::new());
        Ok(self
            .channel
            .send_message(&self.http, message)
            .await
            .context("failed to send Discord message")?
            .id)
    }

    /// Swaps the bot's 👀 on `message` in `origin` for ✅ or ❌; failures are only logged.
    async fn finish(&self, origin: ChannelId, message: MessageId, ok: bool) {
        let swap = async {
            origin
                .delete_reaction(&self.http, message, None, RECEIVED)
                .await?;
            let done = if ok { DONE } else { FAILED };
            origin.create_reaction(&self.http, message, done).await
        };
        if let Err(err) = swap.await {
            tracing::warn!("failed to update reaction: {err:#}");
        }
    }

    async fn finish_all(&self, messages: &[(ChannelId, MessageId)], ok: bool) {
        for &(origin, message) in messages {
            self.finish(origin, message, ok).await;
        }
    }

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
        // Notes after this reply start a new status message below it.
        self.status = None;
        for chunk in chunks(text, MAX_MESSAGE_CHARS) {
            self.send(CreateMessage::new().content(chunk)).await?;
        }
        Ok(())
    }

    /// Shows `text` as small gray subtext, merging back-to-back notes into one edited message.
    async fn note(&mut self, text: &str) -> Result<()> {
        let first_line = text.lines().next().unwrap_or_default();
        let line: String = format!("-# {first_line}")
            .chars()
            .take(MAX_STATUS_CHARS)
            .collect();
        if let Some((message, body)) = &mut self.status
            && body.chars().count() + line.chars().count() < MAX_MESSAGE_CHARS
        {
            body.push('\n');
            body.push_str(&line);
            let edit = EditMessage::new().content(body.as_str());
            if let Err(err) = self.channel.edit_message(&self.http, *message, edit).await {
                tracing::warn!("failed to update status message: {err:#}");
            }
            return Ok(());
        }
        let message = self.send(CreateMessage::new().content(&line)).await?;
        self.status = Some((message, line));
        Ok(())
    }

    async fn confirm(&mut self, command: &str) -> Result<bool> {
        let block = code_block("", command, MAX_MESSAGE_CHARS / 2);
        let buttons = CreateActionRow::Buttons(vec![
            CreateButton::new(RUN)
                .label("Run")
                .style(ButtonStyle::Success),
            CreateButton::new(DENY)
                .label("Deny")
                .style(ButtonStyle::Danger),
        ]);
        self.status = None;
        let message = self
            .send(
                CreateMessage::new()
                    .content(&block)
                    .components(vec![buttons]),
            )
            .await
            .context("failed to send approval request")?;
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
                Ok(Some(Input::Text {
                    content,
                    origin,
                    message: reply,
                    ..
                })) => {
                    let run = content.trim().eq_ignore_ascii_case("y");
                    self.finish(origin, reply, true).await;
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

/// The attachment's image format, if it is an image the model can read.
fn image_type(attachment: &Attachment) -> Option<ImageMediaType> {
    let mime = attachment
        .content_type
        .as_deref()?
        .split(';')
        .next()?
        .trim();
    ImageMediaType::from_mime_type(mime)
}

/// Whether the attachment is text to read into the prompt.
fn is_text(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|mime| mime.starts_with("text/") || mime.starts_with("application/json"))
}

/// Downloads `attachments` for the model: text files as prompt text to append, images as images.
/// Files over `MAX_TEXT_BYTES` or `MAX_IMAGE_BYTES` are left out.
async fn download(
    web: &reqwest::Client,
    attachments: &[Attachment],
) -> Result<(String, Vec<Image>)> {
    let mut text = String::new();
    let mut images = Vec::new();
    for attachment in attachments {
        let media_type = image_type(attachment);
        let limit = if media_type.is_some() {
            MAX_IMAGE_BYTES
        } else {
            MAX_TEXT_BYTES
        };
        if attachment.size > limit {
            tracing::warn!(
                "skipping {}: {} bytes",
                attachment.filename,
                attachment.size
            );
            continue;
        }
        let bytes = web
            .get(&attachment.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("failed to download {}", attachment.filename))?
            .bytes()
            .await
            .with_context(|| format!("failed to download {}", attachment.filename))?;
        if media_type.is_some() {
            images.push(Image {
                data: DocumentSourceKind::Base64(
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ),
                media_type,
                ..Image::default()
            });
        } else {
            text.push_str(&format!(
                "\n\n[{}]\n{}",
                attachment.filename,
                String::from_utf8_lossy(&bytes)
            ));
        }
    }
    Ok((text, images))
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

    #[tokio::test]
    async fn merge_waiting_joins_burst_and_stops_at_new() {
        let text = |content: &str, id: u64| Input::Text {
            content: content.to_owned(),
            attachments: Vec::new(),
            origin: ChannelId::new(1),
            message: MessageId::new(id),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        for input in [
            text("b", 2),
            Input::Decision {
                message: MessageId::new(9),
                run: true,
            },
            text("c", 3),
            text(" /new ", 4),
            text("d", 5),
        ] {
            tx.send(input).unwrap();
        }
        drop(tx);
        let mut content = "a".to_owned();
        let mut attachments = Vec::new();
        let mut messages = vec![(ChannelId::new(1), MessageId::new(1))];
        let next = merge_waiting(&mut rx, &mut content, &mut attachments, &mut messages).await;
        assert_eq!(content, "a\nb\nc");
        assert_eq!(messages.len(), 3);
        assert!(matches!(next, Some(Input::Text { message, .. }) if message == MessageId::new(4)));
        assert!(matches!(rx.try_recv(), Ok(Input::Text { content, .. }) if content == "d"));
    }

    #[tokio::test]
    async fn merge_waiting_stops_at_cap() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for id in 2.. {
                let input = Input::Text {
                    content: "more".to_owned(),
                    attachments: Vec::new(),
                    origin: ChannelId::new(1),
                    message: MessageId::new(id),
                };
                if tx.send(input).is_err() {
                    break;
                }
                tokio::time::sleep(MERGE_QUIET / 2).await;
            }
        });
        let start = tokio::time::Instant::now();
        let mut content = String::new();
        let mut messages = Vec::new();
        merge_waiting(&mut rx, &mut content, &mut Vec::new(), &mut messages).await;
        assert!(start.elapsed() < MERGE_CAP + MERGE_QUIET / 2);
        assert!(messages.len() >= 2);
    }

    #[test]
    fn thread_name_uses_first_line_within_limit() {
        assert_eq!(thread_name("\n  check disk  \nmore"), "check disk");
        assert_eq!(thread_name("   "), "mitten");
        let long = thread_name(&"磁".repeat(200));
        assert_eq!(long.chars().count(), MAX_THREAD_NAME_CHARS + 1);
        assert!(long.ends_with('…'));
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
