//! `mitten chat`: a full-screen conversation with the agent.

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};
use unicode_width::UnicodeWidthChar;

use crate::agent::{Agent, Io};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);
/// Command output lines shown on screen; the model still gets all of it.
const OUTPUT_PREVIEW_LINES: usize = 6;
const MAX_INPUT_LINES: u16 = 6;

/// Runs the chat screen until the user quits.
pub async fn run(agent: Agent) -> Result<()> {
    let cwd = std::env::current_dir()?.display().to_string();
    let cwd = match std::env::home_dir() {
        Some(home) => cwd.replacen(&home.display().to_string(), "~", 1),
        None => cwd,
    };
    let mut app = App::new(agent.describe(), cwd, agent.is_resumed());

    let (requests, inbox) = mpsc::unbounded_channel();
    let (updates_tx, mut updates) = mpsc::unbounded_channel();
    tokio::spawn(serve(agent, inbox, updates_tx));
    // ponytail: the reader thread outlives the screen; fine since the process exits right after.
    let (events_tx, mut events) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if events_tx.send(event).is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableBracketedPaste)?;
    let result = app
        .run(&mut terminal, &mut events, &mut updates, &requests)
        .await;
    execute!(std::io::stdout(), DisableBracketedPaste)?;
    ratatui::restore();
    result
}

/// What the screen asks the agent task to do.
enum Request {
    Prompt(String),
    Reset,
}

/// What the agent task reports back to the screen.
#[derive(Debug)]
enum Update {
    Say(String),
    Note(String),
    Confirm(String, oneshot::Sender<bool>),
    Ran { output: String, ok: bool },
    Done(Result<(), String>),
}

/// Owns the agent so turns run while the screen keeps drawing.
async fn serve(
    mut agent: Agent,
    mut requests: mpsc::UnboundedReceiver<Request>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let mut io = ChannelIo(updates.clone());
    while let Some(request) = requests.recv().await {
        let result = match request {
            Request::Prompt(prompt) => agent.run_turn(&prompt, Vec::new(), &mut io).await,
            Request::Reset => agent.reset().await,
        };
        let done = Update::Done(result.map_err(|err| format!("{err:#}")));
        if updates.send(done).is_err() {
            break;
        }
    }
}

struct ChannelIo(mpsc::UnboundedSender<Update>);

impl ChannelIo {
    fn send(&self, update: Update) -> Result<()> {
        self.0
            .send(update)
            .map_err(|_| anyhow!("chat screen closed"))
    }
}

impl Io for ChannelIo {
    async fn say(&mut self, text: &str) -> Result<()> {
        self.send(Update::Say(text.to_owned()))
    }

    async fn note(&mut self, text: &str) -> Result<()> {
        self.send(Update::Note(text.to_owned()))
    }

    async fn confirm(&mut self, command: &str) -> Result<bool> {
        let (answer, reply) = oneshot::channel();
        self.send(Update::Confirm(command.to_owned(), answer))?;
        Ok(reply.await.unwrap_or(false))
    }

    async fn ran(&mut self, output: &str, ok: bool) -> Result<()> {
        self.send(Update::Ran {
            output: output.to_owned(),
            ok,
        })
    }
}

enum Entry {
    User(String),
    Assistant(String),
    Command { command: String, state: Run },
    Error(String),
    Notice(String),
}

enum Run {
    Asking,
    Denied,
    Running,
    Done { output: String, ok: bool },
}

struct App {
    model: String,
    cwd: String,
    entries: Vec<Entry>,
    input: String,
    /// When the current turn started; `None` while idle.
    busy: Option<Instant>,
    /// Answer channel for the command waiting on approval.
    confirm: Option<oneshot::Sender<bool>>,
    /// Lines scrolled up from the bottom of the transcript.
    scroll: usize,
}

impl App {
    fn new(model: String, cwd: String, resumed: bool) -> Self {
        let mut entries = Vec::new();
        if resumed {
            entries.push(Entry::Notice(
                "Resumed the earlier conversation. /new starts fresh.".to_owned(),
            ));
        }
        Self {
            model,
            cwd,
            entries,
            input: String::new(),
            busy: None,
            confirm: None,
            scroll: 0,
        }
    }

    async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        events: &mut mpsc::UnboundedReceiver<Event>,
        updates: &mut mpsc::UnboundedReceiver<Update>,
        requests: &mpsc::UnboundedSender<Request>,
    ) -> Result<()> {
        let mut tick = tokio::time::interval(TICK);
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            tokio::select! {
                Some(event) = events.recv() => {
                    if self.on_event(event, requests) {
                        return Ok(());
                    }
                }
                update = updates.recv() => match update {
                    Some(update) => self.on_update(update),
                    None => bail!("the agent stopped unexpectedly"),
                },
                _ = tick.tick() => {}
            }
        }
    }

    /// Handles terminal input; returns true to quit.
    fn on_event(&mut self, event: Event, requests: &mpsc::UnboundedSender<Request>) -> bool {
        match event {
            Event::Paste(text) if self.confirm.is_none() => {
                self.input
                    .push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                false
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key, requests),
            _ => false,
        }
    }

    fn on_key(&mut self, key: KeyEvent, requests: &mpsc::UnboundedSender<Request>) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('d') if ctrl && self.input.is_empty() => return true,
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up => self.scroll += 1,
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            _ if self.confirm.is_some() => match key.code {
                KeyCode::Char('y' | 'Y') => self.answer(true),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.answer(false),
                _ => {}
            },
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.input.push('\n'),
            KeyCode::Enter => return self.submit(requests),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char(c) if !ctrl => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            _ => {}
        }
        false
    }

    /// Sends the typed prompt or runs a slash command; returns true to quit.
    fn submit(&mut self, requests: &mpsc::UnboundedSender<Request>) -> bool {
        if self.busy.is_some() {
            return false;
        }
        let prompt = self.input.trim().to_owned();
        let request = match prompt.as_str() {
            "" => return false,
            "/exit" | "/quit" => return true,
            "/new" => {
                self.entries.clear();
                self.entries
                    .push(Entry::Notice("Started a new conversation.".to_owned()));
                Request::Reset
            }
            _ => {
                self.entries.push(Entry::User(prompt.clone()));
                Request::Prompt(prompt)
            }
        };
        self.input.clear();
        self.scroll = 0;
        if requests.send(request).is_ok() {
            self.busy = Some(Instant::now());
        }
        false
    }

    fn answer(&mut self, run: bool) {
        if let Some(reply) = self.confirm.take() {
            let _ = reply.send(run);
        }
        if let Some(Entry::Command { state, .. }) = self.entries.last_mut() {
            *state = if run { Run::Running } else { Run::Denied };
        }
    }

    fn on_update(&mut self, update: Update) {
        match update {
            Update::Say(text) => self.entries.push(Entry::Assistant(text)),
            Update::Note(text) => self.entries.push(Entry::Notice(text)),
            Update::Confirm(command, reply) => {
                self.entries.push(Entry::Command {
                    command,
                    state: Run::Asking,
                });
                self.confirm = Some(reply);
            }
            Update::Ran { output, ok } => {
                if let Some(Entry::Command { state, .. }) = self.entries.last_mut() {
                    *state = Run::Done { output, ok };
                }
            }
            Update::Done(result) => {
                self.busy = None;
                if let Err(err) = result {
                    self.entries.push(Entry::Error(err));
                }
            }
        }
        self.scroll = 0;
    }

    fn spinner(&self) -> &'static str {
        let elapsed = self.busy.map_or(0, |start| start.elapsed().as_millis());
        SPINNER[(elapsed / TICK.as_millis()) as usize % SPINNER.len()]
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let (rows, cursor_x) = wrap_input(&self.input, frame.area().width.saturating_sub(4));
        let input_height = (rows.len() as u16).clamp(1, MAX_INPUT_LINES);
        let [header, body, input_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(input_height + 2),
        ])
        .areas(frame.area());

        frame.render_widget(
            Line::from(vec![
                " mitten ".black().on_cyan().bold(),
                Span::raw(" "),
                Span::raw(self.model.as_str()).dark_gray(),
            ]),
            header,
        );
        frame.render_widget(
            Line::from(format!("{} ", self.cwd))
                .dark_gray()
                .right_aligned(),
            header,
        );

        let [body] = Layout::horizontal([Constraint::Fill(1)])
            .horizontal_margin(1)
            .areas(body);
        let bottom = wrapped(self.transcript_lines())
            .line_count(body.width)
            .saturating_sub(usize::from(body.height));
        self.scroll = self.scroll.min(bottom);
        let top = u16::try_from(bottom - self.scroll).unwrap_or(u16::MAX);
        frame.render_widget(wrapped(self.transcript_lines()).scroll((top, 0)), body);

        let (color, status) = if self.confirm.is_some() {
            (Color::Yellow, " run this command? y yes · n no ".to_owned())
        } else if let Some(start) = self.busy {
            let secs = start.elapsed().as_secs();
            (
                Color::DarkGray,
                format!(" {} working… {secs}s ", self.spinner()),
            )
        } else {
            (Color::Cyan, String::new())
        };
        let mut hints =
            "enter send · alt+enter newline · /new · ↑↓ pgup pgdn scroll · ctrl+c quit ".to_owned();
        if self.scroll > 0 {
            hints = format!("↓ {} more lines · {hints}", self.scroll);
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .title(Line::from(status).fg(color).bold())
            .title_bottom(Line::from(hints).dark_gray().right_aligned());
        // Keep the end of long input, where the cursor is, in view.
        let skip = rows.len() - usize::from(input_height);
        let lines: Vec<Line> = if self.input.is_empty() && self.busy.is_none() {
            vec![Line::from("Ask Mitten anything...").dark_gray()]
        } else {
            rows.into_iter().skip(skip).map(Line::from).collect()
        };
        frame.render_widget(
            Paragraph::new(lines).block(block.padding(Padding::horizontal(1))),
            input_area,
        );
        // The real cursor, not a drawn one, so IME composition shows up where you type.
        frame.set_cursor_position((input_area.x + 2 + cursor_x, input_area.y + input_height));
    }

    fn transcript_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        if self.entries.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from("Hi, I'm Mitten.").bold());
            lines.push(
                Line::from("Ask me to look into or change things on this machine.").dark_gray(),
            );
            lines.push(Line::from("I only run commands after you approve them.").dark_gray());
            return lines;
        }
        for entry in &self.entries {
            lines.push(Line::default());
            match entry {
                Entry::User(text) => prefixed(
                    &mut lines,
                    "❯ ",
                    text,
                    |line| Line::from(line).bold(),
                    Style::new().cyan().bold(),
                ),
                Entry::Assistant(text) => assistant_lines(&mut lines, text),
                Entry::Command { command, state } => self.command_lines(&mut lines, command, state),
                Entry::Error(text) => prefixed(
                    &mut lines,
                    "✗ ",
                    text,
                    |line| Line::from(line).red(),
                    Style::new().red().bold(),
                ),
                Entry::Notice(text) => lines.push(Line::from(text.as_str()).dark_gray().italic()),
            }
        }
        lines
    }

    fn command_lines<'a>(&self, lines: &mut Vec<Line<'a>>, command: &'a str, state: &'a Run) {
        let color = match state {
            Run::Asking => Color::Yellow,
            Run::Running => Color::Blue,
            Run::Denied => Color::DarkGray,
            Run::Done { ok: true, .. } => Color::Green,
            Run::Done { ok: false, .. } => Color::Red,
        };
        let edge = Style::new().fg(color);
        for (i, line) in command.lines().enumerate() {
            let (corner, sigil) = if i == 0 {
                ("╭─ ", "$ ")
            } else {
                ("│  ", "  ")
            };
            lines.push(Line::from(vec![
                Span::styled(corner, edge),
                Span::styled(sigil, edge),
                Span::raw(line).bold(),
            ]));
        }
        if let Run::Done { output, .. } = state {
            let output = output.trim_end();
            let total = output.lines().count();
            for line in output.lines().take(OUTPUT_PREVIEW_LINES) {
                lines.push(Line::from(vec![
                    Span::styled("│  ", edge),
                    Span::raw(line).dark_gray(),
                ]));
            }
            if total > OUTPUT_PREVIEW_LINES {
                lines.push(Line::from(vec![
                    Span::styled("│  ", edge),
                    format!("… {} more lines", total - OUTPUT_PREVIEW_LINES)
                        .dark_gray()
                        .italic(),
                ]));
            }
        }
        let status = match state {
            Run::Asking => "run this? y yes · n no".yellow().bold(),
            Run::Running => format!("{} running…", self.spinner()).blue(),
            Run::Denied => "denied".dark_gray(),
            Run::Done { ok: true, .. } => "✓ done".green(),
            Run::Done { ok: false, .. } => "✗ failed".red(),
        };
        lines.push(Line::from(vec![Span::styled("╰─ ", edge), status]));
    }
}

/// Splits input into rows of at most `width` cells (wide CJK characters count as two),
/// returning the rows and the cursor column after the last character.
fn wrap_input(text: &str, width: u16) -> (Vec<String>, u16) {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut col = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if c == '\n' || col + w > width {
            rows.push(std::mem::take(&mut row));
            col = 0;
        }
        if c != '\n' {
            row.push(c);
            col += w;
        }
    }
    if col >= width {
        rows.push(std::mem::take(&mut row));
        col = 0;
    }
    rows.push(row);
    (rows, col as u16)
}

fn wrapped(lines: Vec<Line<'_>>) -> Paragraph<'_> {
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

/// Pushes `text` with `mark` on its first line and matching indent on the rest.
fn prefixed<'a>(
    lines: &mut Vec<Line<'a>>,
    mark: &'static str,
    text: &'a str,
    style_line: impl Fn(&'a str) -> Line<'a>,
    mark_style: Style,
) {
    for (i, line) in text.lines().enumerate() {
        let lead = if i == 0 { mark } else { "  " };
        let mut styled = style_line(line);
        styled.spans.insert(0, Span::styled(lead, mark_style));
        lines.push(styled);
    }
}

/// Model text with light markdown: headings bold, fenced code highlighted.
// ponytail: no inline markdown (bold, links, tables); pull in a renderer if replies need it.
fn assistant_lines<'a>(lines: &mut Vec<Line<'a>>, text: &'a str) {
    let mut in_code = false;
    for (i, line) in text.lines().enumerate() {
        let lead = if i == 0 {
            Span::styled("● ", Style::new().magenta())
        } else {
            Span::raw("  ")
        };
        let body = if line.trim_start().starts_with("```") {
            in_code = !in_code;
            Span::raw(line).dark_gray()
        } else if in_code {
            Span::raw(line).yellow()
        } else if line.starts_with('#') {
            Span::raw(line.trim_start_matches('#').trim_start()).bold()
        } else {
            Span::raw(line)
        };
        lines.push(Line::from(vec![lead, body]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(app: &mut App) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        format!("{}", terminal.backend())
    }

    #[test]
    fn wrap_input_counts_wide_characters() {
        assert_eq!(wrap_input("", 4), (vec![String::new()], 0));
        assert_eq!(
            wrap_input("磁碟空間", 5),
            (vec!["磁碟".to_owned(), "空間".to_owned()], 4)
        );
        assert_eq!(
            wrap_input("abcd", 4),
            (vec!["abcd".to_owned(), String::new()], 0)
        );
        assert_eq!(
            wrap_input("a\nb", 4),
            (vec!["a".to_owned(), "b".to_owned()], 1)
        );
    }

    #[test]
    fn approval_flows_from_prompt_to_result() {
        let mut app = App::new("opencode-go / m".to_owned(), "~/x".to_owned(), false);
        assert!(screen(&mut app).contains("Hi, I'm Mitten."));

        let (requests, mut inbox) = mpsc::unbounded_channel();
        app.input = "disk?".to_owned();
        assert!(!app.submit(&requests));
        assert!(matches!(inbox.try_recv(), Ok(Request::Prompt(p)) if p == "disk?"));

        let (answer, mut reply) = oneshot::channel();
        app.on_update(Update::Confirm("df -h /".to_owned(), answer));
        assert!(screen(&mut app).contains("run this? y yes"));
        app.on_key(KeyEvent::from(KeyCode::Char('y')), &requests);
        assert_eq!(reply.try_recv(), Ok(true));

        app.on_update(Update::Ran {
            output: "Filesystem Size\n/dev/disk3 460G".to_owned(),
            ok: true,
        });
        app.on_update(Update::Say("120G free.".to_owned()));
        app.on_update(Update::Done(Ok(())));
        let shown = screen(&mut app);
        for want in [
            "❯ disk?",
            "$ df -h /",
            "/dev/disk3 460G",
            "✓ done",
            "● 120G free.",
        ] {
            assert!(shown.contains(want), "missing {want:?}:\n{shown}");
        }
        assert!(app.busy.is_none());
    }
}
