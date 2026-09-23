//! `mitten configure`: a step-by-step, full-screen setup with dropdowns.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListState, Paragraph};
use tokio::runtime::Handle;

use crate::agent;
use crate::config::Config;

/// OpenCode Go models served in Anthropic format; the others need `/chat/completions`.
const MODELS: &[&str] = &[
    "minimax-m3",
    "minimax-m2.7",
    "qwen3.8-max",
    "qwen3.8-flash",
    "qwen3.7-max",
    "qwen3.7-plus",
];
const CUSTOM: &str = "Custom…";
const ENABLED: &str = "Enabled";
const DISABLED: &str = "Disabled";
const STEPS: [&str; 3] = ["Model", "Discord", "Advanced"];
const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// Shows the setup, tests the connection, and saves the config to `path`.
pub async fn run(path: &Path) -> Result<Config> {
    let current = path.exists().then(|| Config::load(path).ok()).flatten();
    let mut form = Form::new(current.as_ref())?;
    let handle = Handle::current();
    let target = path.to_owned();
    let saved = tokio::task::spawn_blocking(move || {
        ratatui::run(|terminal| form.run(terminal, &handle, &target))
    })
    .await
    .context("configure screen panicked")??;
    match saved {
        Some(config) => {
            println!("✓ saved {}", path.display());
            Ok(config)
        }
        None => bail!("not saved"),
    }
}

enum Kind {
    Text,
    Secret,
    Select(Vec<String>),
}

struct Field {
    step: usize,
    label: &'static str,
    kind: Kind,
    value: String,
}

// Field indices.
// 0 is Provider, which has only OpenCode Go for now.
const KEY: usize = 1;
const MODEL: usize = 2;
const CUSTOM_MODEL: usize = 3;
const DISCORD: usize = 4;
const TOKEN: usize = 5;
const USERS: usize = 6;
const TIMEOUT: usize = 7;
const LOG: usize = 8;
const DATABASE: usize = 9;

/// Focus value for the Next / Save button under the fields.
const BUTTON: usize = usize::MAX;

struct Form {
    fields: Vec<Field>,
    step: usize,
    focus: usize,
    /// Open dropdown for the focused select field.
    menu: Option<ListState>,
    reveal: bool,
    status: Line<'static>,
    /// Set after a failed connection test; the next save skips the test.
    force_save: bool,
}

fn options(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// `values` plus `current` when it isn't one of them, so a hand-edited value survives.
fn options_with(values: &[&str], current: &str) -> Vec<String> {
    let mut all = options(values);
    if !all.iter().any(|v| v == current) {
        all.push(current.to_owned());
    }
    all
}

impl Form {
    fn new(current: Option<&Config>) -> Result<Self> {
        let discord = current.and_then(|c| c.discord.as_ref());
        let model = current.map_or(MODELS[0], |c| c.model.as_str());
        let preset = MODELS.contains(&model);
        let timeout = current
            .map_or(120, |c| c.bash_timeout.as_secs())
            .to_string();
        let log = current.map_or("warn", |c| c.log_level.as_str());
        let database = match current {
            Some(c) => c.database_path.clone(),
            None => default_database_path()?,
        };
        let field = |step, label, kind, value: String| Field {
            step,
            label,
            kind,
            value,
        };

        let mut model_options = options(MODELS);
        model_options.push(CUSTOM.to_owned());
        let fields = vec![
            field(
                0,
                "Provider",
                Kind::Select(options(&["OpenCode Go"])),
                "OpenCode Go".to_owned(),
            ),
            field(
                0,
                "API key",
                Kind::Secret,
                current
                    .map(|c| c.api_key.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                0,
                "Model",
                Kind::Select(model_options),
                if preset { model } else { CUSTOM }.to_owned(),
            ),
            field(
                0,
                "Model ID",
                Kind::Text,
                if preset {
                    String::new()
                } else {
                    model.to_owned()
                },
            ),
            field(
                1,
                "Discord bot",
                Kind::Select(options(&[DISABLED, ENABLED])),
                if discord.is_some() { ENABLED } else { DISABLED }.to_owned(),
            ),
            field(
                1,
                "Bot token",
                Kind::Secret,
                discord
                    .map(|d| d.token.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                1,
                "Allowed user IDs",
                Kind::Text,
                discord
                    .map(|d| {
                        d.allowed_users
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default(),
            ),
            field(
                2,
                "Bash timeout (s)",
                Kind::Select(options_with(&["30", "60", "120", "300", "600"], &timeout)),
                timeout,
            ),
            field(
                2,
                "Log level",
                Kind::Select(options_with(&["error", "warn", "info", "debug"], log)),
                log.to_owned(),
            ),
            field(
                2,
                "Database",
                Kind::Text,
                database.to_string_lossy().into_owned(),
            ),
        ];
        Ok(Self {
            fields,
            step: 0,
            focus: KEY,
            menu: None,
            reveal: false,
            status: Line::default(),
            force_save: false,
        })
    }

    fn shown(&self, i: usize) -> bool {
        let field = &self.fields[i];
        field.step == self.step
            && match i {
                CUSTOM_MODEL => self.fields[MODEL].value == CUSTOM,
                TOKEN | USERS => self.fields[DISCORD].value == ENABLED,
                _ => true,
            }
    }

    /// Focusable rows of the current step: its visible fields, then the button.
    fn visible(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|&i| self.shown(i))
            .chain([BUTTON])
            .collect()
    }

    fn move_focus(&mut self, step: isize) {
        let rows = self.visible();
        let at = rows.iter().position(|&i| i == self.focus).unwrap_or(0);
        let next = (at as isize + step).rem_euclid(rows.len() as isize);
        self.focus = rows[next as usize];
    }

    fn go_to_step(&mut self, step: usize) {
        self.step = step;
        self.focus = self.visible()[0];
        self.status = Line::default();
    }

    fn open_menu(&mut self) {
        if let Some(Kind::Select(values)) = self.fields.get(self.focus).map(|f| &f.kind) {
            let selected = values
                .iter()
                .position(|v| *v == self.fields[self.focus].value);
            self.menu = Some(ListState::default().with_selected(selected.or(Some(0))));
        }
    }

    /// Runs the event loop; `Some` once saved, `None` if the user quit.
    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        handle: &Handle,
        path: &Path,
    ) -> Result<Option<Config>> {
        loop {
            terminal.draw(|frame| self.draw(frame))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(None);
            }
            if let Some(menu) = &mut self.menu {
                let Kind::Select(values) = &self.fields[self.focus].kind else {
                    self.menu = None;
                    continue;
                };
                match key.code {
                    KeyCode::Up => menu.select_previous(),
                    KeyCode::Down => menu.select_next(),
                    KeyCode::Enter => {
                        let chosen = menu.selected().unwrap_or(0).min(values.len() - 1);
                        self.fields[self.focus].value = values[chosen].clone();
                        self.menu = None;
                        self.edited();
                    }
                    KeyCode::Esc => self.menu = None,
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Esc if self.step == 0 => return Ok(None),
                KeyCode::Esc => self.go_to_step(self.step - 1),
                KeyCode::Up | KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Down | KeyCode::Tab => self.move_focus(1),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.reveal = !self.reveal;
                }
                KeyCode::Enter if self.focus == BUTTON => {
                    if let Err(err) = self.check_step() {
                        self.status = Line::from(format!("✗ {err}")).red();
                    } else if self.step + 1 < STEPS.len() {
                        self.go_to_step(self.step + 1);
                    } else if let Some(config) = self.save(terminal, handle, path)? {
                        return Ok(Some(config));
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ')
                    if matches!(self.fields[self.focus].kind, Kind::Select(_)) =>
                {
                    self.open_menu();
                }
                KeyCode::Enter => self.move_focus(1),
                KeyCode::Backspace if self.focus != BUTTON => {
                    self.fields[self.focus].value.pop();
                    self.edited();
                }
                KeyCode::Char(c)
                    if self.focus != BUTTON
                        && !matches!(self.fields[self.focus].kind, Kind::Select(_)) =>
                {
                    self.fields[self.focus].value.push(c);
                    self.edited();
                }
                _ => {}
            }
        }
    }

    /// Any edit means a previous failed connection test no longer applies.
    fn edited(&mut self) {
        self.force_save = false;
        self.status = Line::default();
    }

    fn save(
        &mut self,
        terminal: &mut DefaultTerminal,
        handle: &Handle,
        path: &Path,
    ) -> Result<Option<Config>> {
        let answers = match self.answers() {
            Ok(answers) => answers,
            Err(err) => {
                self.status = Line::from(format!("✗ {err}")).red();
                return Ok(None);
            }
        };
        let text = render_toml(&answers);
        let config = Config::parse(&text).context("form produced an invalid config")?;

        if !self.force_save {
            self.status = Line::from("testing connection…").yellow();
            terminal.draw(|frame| self.draw(frame))?;
            let result = handle.block_on(async {
                tokio::time::timeout(PING_TIMEOUT, agent::ping(&config))
                    .await
                    .map_err(|_| anyhow::anyhow!("no reply in {PING_TIMEOUT:?}"))?
            });
            if let Err(err) = result {
                self.status = Line::from(format!("✗ {err:#}  (Enter again saves anyway)")).red();
                self.force_save = true;
                return Ok(None);
            }
        }
        save(path, &text)?;
        Ok(Some(config))
    }

    fn value(&self, i: usize) -> String {
        self.fields[i].value.trim().to_owned()
    }

    fn required(&self, i: usize) -> std::result::Result<String, String> {
        let value = self.value(i);
        if value.is_empty() {
            return Err(format!("{} is required", self.fields[i].label));
        }
        Ok(value)
    }

    /// Validates the fields of the current step only.
    fn check_step(&self) -> std::result::Result<(), String> {
        match self.step {
            0 => {
                self.required(KEY)?;
                self.model()?;
            }
            1 => {
                self.discord()?;
            }
            _ => {
                self.timeout()?;
                self.required(DATABASE)?;
            }
        }
        Ok(())
    }

    fn model(&self) -> std::result::Result<String, String> {
        if self.fields[MODEL].value == CUSTOM {
            self.required(CUSTOM_MODEL)
        } else {
            self.required(MODEL)
        }
    }

    fn discord(&self) -> std::result::Result<Option<DiscordAnswers>, String> {
        if self.fields[DISCORD].value != ENABLED {
            return Ok(None);
        }
        Ok(Some(DiscordAnswers {
            token: self.required(TOKEN)?,
            allowed_users: parse_user_ids(&self.value(USERS))?,
        }))
    }

    fn timeout(&self) -> std::result::Result<u64, String> {
        self.value(TIMEOUT)
            .parse()
            .map_err(|_| "Bash timeout must be a whole number of seconds".to_owned())
    }

    fn answers(&self) -> std::result::Result<Answers, String> {
        Ok(Answers {
            api_key: self.required(KEY)?,
            model: self.model()?,
            discord: self.discord()?,
            bash_timeout_secs: self.timeout()?,
            log_level: self.required(LOG)?,
            database: PathBuf::from(self.required(DATABASE)?),
        })
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let rows = self.visible();
        let [steps, body, status, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(rows.len() as u16 + 3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        let tabs: Vec<Span> = STEPS
            .iter()
            .enumerate()
            .flat_map(|(i, name)| {
                let label = format!(" {} {name} ", i + 1);
                let span = if i == self.step {
                    label.black().on_cyan().bold()
                } else {
                    label.dark_gray()
                };
                [span, Span::raw(" ")]
            })
            .collect();
        frame.render_widget(Line::from(tabs), steps);

        let mut lines: Vec<Line> = Vec::new();
        let mut menu_anchor = None;
        for (row, &i) in rows.iter().enumerate() {
            let focused = i == self.focus;
            if i == BUTTON {
                let label = if self.step + 1 < STEPS.len() {
                    "[ Next › ]"
                } else {
                    "[ Save ]"
                };
                lines.push(Line::default());
                let button = if focused {
                    label.black().on_cyan().bold()
                } else {
                    label.cyan()
                };
                lines.push(Line::from(vec![Span::raw("  "), button]));
                continue;
            }
            let field = &self.fields[i];
            let shown = match &field.kind {
                Kind::Secret if !self.reveal && !field.value.is_empty() => {
                    "•".repeat(field.value.chars().count().min(24))
                }
                Kind::Select(_) => format!("{} ▾", field.value),
                _ => field.value.clone(),
            };
            let cursor = if focused && !matches!(field.kind, Kind::Select(_)) {
                "▏"
            } else {
                ""
            };
            let label_style = if focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            if focused {
                menu_anchor = Some(row as u16);
            }
            lines.push(Line::from(vec![
                Span::raw(if focused { "› " } else { "  " }),
                Span::styled(format!("{:<18}", field.label), label_style),
                Span::raw(shown),
                Span::raw(cursor),
            ]));
        }
        let title = format!(" mitten configure · {} ", STEPS[self.step]);
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            body,
        );
        frame.render_widget(Paragraph::new(self.status.clone()), status);
        let hint = if self.menu.is_some() {
            "↑↓ choose · Enter select · Esc close"
        } else {
            "↑↓ move · Enter open/next · type to edit · ^R show secrets · Esc back · ^C quit"
        };
        frame.render_widget(Paragraph::new(hint).dark_gray(), help);

        if let (Some(menu), Some(anchor), Some(Kind::Select(values))) = (
            &mut self.menu,
            menu_anchor,
            self.fields.get(self.focus).map(|f| &f.kind),
        ) {
            let area = dropdown_area(body, anchor, values.len() as u16);
            let list = List::new(values.iter().map(String::as_str))
                .block(Block::bordered())
                .highlight_style(Style::new().black().on_cyan())
                .highlight_symbol("› ");
            frame.render_widget(Clear, area);
            frame.render_stateful_widget(list, area, menu);
        }
    }
}

/// Places the dropdown just under field row `row`, aligned with the value column.
fn dropdown_area(body: Rect, row: u16, items: u16) -> Rect {
    let x = body.x + 1 + 2 + 18;
    let y = body.y + 1 + row + 1;
    let [area] = Layout::horizontal([Constraint::Length(28)])
        .flex(Flex::Start)
        .areas(Rect::new(
            x,
            y,
            body.width.saturating_sub(x - body.x),
            items + 2,
        ));
    area
}

struct DiscordAnswers {
    token: String,
    allowed_users: Vec<u64>,
}

fn parse_user_ids(text: &str) -> std::result::Result<Vec<u64>, String> {
    let ids = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map_err(|_| format!("`{part}` is not a Discord user ID (digits only)"))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err("enter at least one user ID".to_owned());
    }
    Ok(ids)
}

fn default_database_path() -> Result<PathBuf> {
    Ok(std::env::home_dir()
        .context("cannot find home directory")?
        .join(".local/share/mitten/mitten.db"))
}

struct Answers {
    model: String,
    api_key: String,
    discord: Option<DiscordAnswers>,
    bash_timeout_secs: u64,
    log_level: String,
    database: PathBuf,
}

/// Quotes and escapes a TOML string.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn render_toml(answers: &Answers) -> String {
    let mut text = format!(
        "# Written by `mitten configure`; run it again to change these settings.\n\
         \n\
         [model]\n\
         name = {model}\n\
         \n\
         [opencode-go]\n\
         api_key = {key}\n\
         \n\
         [tools.bash]\n\
         timeout_secs = {timeout}\n\
         \n\
         [log]\n\
         level = {log}\n\
         \n\
         [database]\n\
         path = {db}\n",
        model = quote(&answers.model),
        key = quote(&answers.api_key),
        timeout = answers.bash_timeout_secs,
        log = quote(&answers.log_level),
        db = quote(&answers.database.to_string_lossy()),
    );
    if let Some(discord) = &answers.discord {
        let users = discord
            .allowed_users
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            "\n[discord]\ntoken = {}\nallowed_users = [{users}]\n",
            quote(&discord.token)
        ));
    }
    text
}

/// Writes the config readable only by the user, keeping the previous file as `.bak`.
fn save(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)
            .with_context(|| format!("failed to back up to {}", backup.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    // `mode` only applies on creation; tighten an existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_parses_back() {
        let text = render_toml(&Answers {
            model: "qwen3.8-max".to_owned(),
            api_key: "sk-\"quoted\"".to_owned(),
            discord: Some(DiscordAnswers {
                token: "tok".to_owned(),
                allowed_users: vec![1, 22],
            }),
            bash_timeout_secs: 60,
            log_level: "info".to_owned(),
            database: PathBuf::from("/tmp/m.db"),
        });
        let config = Config::parse(&text).expect("valid");
        assert_eq!(config.model, "qwen3.8-max");
        assert_eq!(config.api_key.as_str(), "sk-\"quoted\"");
        assert_eq!(config.bash_timeout.as_secs(), 60);
        assert_eq!(config.discord.map(|d| d.allowed_users), Some(vec![1, 22]));
    }

    #[test]
    fn steps_validate_and_hide_conditional_fields() {
        let mut form = Form::new(None).expect("form");
        assert_eq!(
            form.check_step().err().as_deref(),
            Some("API key is required")
        );
        form.fields[KEY].value = "k".to_owned();
        assert!(!form.visible().contains(&CUSTOM_MODEL));
        form.fields[MODEL].value = CUSTOM.to_owned();
        assert!(form.visible().contains(&CUSTOM_MODEL));
        assert!(form.check_step().is_err(), "custom model needs an ID");
        form.fields[CUSTOM_MODEL].value = "qwen-next".to_owned();
        form.check_step().expect("step 1 valid");

        form.step = 1;
        assert!(!form.visible().contains(&TOKEN));
        form.fields[DISCORD].value = ENABLED.to_owned();
        assert!(form.visible().contains(&TOKEN));
        form.fields[TOKEN].value = "t".to_owned();
        form.fields[USERS].value = "12, x".to_owned();
        assert!(form.check_step().is_err());
        form.fields[USERS].value = "12, 34".to_owned();

        let answers = form.answers().expect("all steps valid");
        assert_eq!(answers.model, "qwen-next");
        assert_eq!(answers.discord.map(|d| d.allowed_users), Some(vec![12, 34]));
    }

    #[test]
    fn draws_step_and_opens_dropdown() {
        let mut form = Form::new(None).expect("form");
        form.fields[KEY].value = "sk-secret".to_owned();
        form.focus = MODEL;
        form.open_menu();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| form.draw(frame)).expect("draw");
        let screen = format!("{}", terminal.backend());
        assert!(screen.contains("1 Model"));
        assert!(
            screen.contains("qwen3.8-max"),
            "dropdown lists models:\n{screen}"
        );
        assert!(!screen.contains("sk-secret"));
    }

    #[test]
    fn user_ids_are_validated() {
        assert_eq!(parse_user_ids(" 1, 2 ,"), Ok(vec![1, 2]));
        assert!(parse_user_ids("abc").is_err());
        assert!(parse_user_ids(" , ").is_err());
    }
}
