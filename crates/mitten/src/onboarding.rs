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
use crate::config::{self, Config};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

/// Common OpenCode Go models; `config::Api::for_model` picks each one's endpoint format.
pub const MODELS: &[&str] = &[
    "minimax-m3",
    "minimax-m2.7",
    "qwen3.8-max",
    "qwen3.8-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "kimi-k3",
    "kimi-k2.7-code",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
];
const CUSTOM: &str = "Custom…";
const ENABLED: &str = "Enabled";
const DISABLED: &str = "Disabled";
const STEPS: [&str; 6] = [
    "Model",
    "Discord",
    "Web search",
    "MCP",
    "Claude Code",
    "Advanced",
];
const NO: &str = "No";
const STDIO: &str = "Local command (stdio)";
const HTTP: &str = "Remote URL (HTTP)";
const KEEP: &str = "Keep";
const REMOVE: &str = "Remove";
const ASK: &str = "Ask every call";
const ASK_ME: &str = "Ask me every time";
const AUTO: &str = "Auto (LLM review)";
const SAME_MODEL: &str = "Same as main model";
const NEVER_ASK: &str = "Never ask";
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
            Ok(Config {
                path: path.to_owned(),
                ..config
            })
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
    label: String,
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
const SEARCH: usize = 7;
const SEARCH_URL: usize = 8;
const SEARCH_USER: usize = 9;
const SEARCH_PASSWORD: usize = 10;
const LOG: usize = 11;
const DATABASE: usize = 12;
const MCP_ADD: usize = 13;
const MCP_NAME: usize = 14;
const MCP_COMMAND: usize = 15;
const MCP_ARGS: usize = 16;
const MCP_ENV: usize = 17;
const MCP_URL: usize = 18;
const MCP_TOKEN: usize = 19;
const MCP_APPROVE: usize = 20;
const APPROVAL: usize = 21;
const REVIEWER: usize = 22;
const CLAUDE: usize = 23;
const CLAUDE_DIRS: usize = 24;
const CLAUDE_MODE: usize = 25;
const CLAUDE_TOOLS: usize = 26;
/// First of the rows for already configured MCP servers.
const MCP_FIRST: usize = 27;

/// Focus value for the Save button under the fields.
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
    /// `tools.searxng.results`, not on the form; kept so saving doesn't reset a hand-edited value.
    search_results: usize,
    /// `tools.claude_code` timeout and command, not on the form; kept like `search_results`.
    claude_extra: (u64, String),
    /// Configured MCP servers, one Keep / Remove row each from `MCP_FIRST` on.
    mcp_names: Vec<String>,
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
        let searxng = current.and_then(|c| c.searxng.as_ref());
        let search_auth = searxng.and_then(|s| s.auth.as_ref());
        let model = current.map_or(MODELS[0], |c| c.model.as_str());
        let preset = MODELS.contains(&model);
        let log = current.map_or("warn", |c| c.log_level.as_str());
        let database = match current {
            Some(c) => c.database_path.clone(),
            None => default_database_path()?,
        };
        let field = |step, label: &str, kind, value: String| Field {
            step,
            label: label.to_owned(),
            kind,
            value,
        };

        let mut model_options = options(MODELS);
        model_options.push(CUSTOM.to_owned());
        let mut fields = vec![
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
                "Web search",
                Kind::Select(options(&[DISABLED, ENABLED])),
                if searxng.is_some() { ENABLED } else { DISABLED }.to_owned(),
            ),
            field(
                2,
                "SearXNG URL",
                Kind::Text,
                searxng.map(|s| s.url.clone()).unwrap_or_default(),
            ),
            field(
                2,
                "Username",
                Kind::Text,
                search_auth
                    .map(|(user, _)| user.clone())
                    .unwrap_or_default(),
            ),
            field(
                2,
                "Password",
                Kind::Secret,
                search_auth
                    .map(|(_, password)| password.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                5,
                "Log level",
                Kind::Select(options_with(&["error", "warn", "info", "debug"], log)),
                log.to_owned(),
            ),
            field(
                5,
                "Database",
                Kind::Text,
                database.to_string_lossy().into_owned(),
            ),
            field(
                3,
                "Add server",
                Kind::Select(options(&[NO, STDIO, HTTP])),
                NO.to_owned(),
            ),
            field(3, "Name", Kind::Text, String::new()),
            field(3, "Command", Kind::Text, String::new()),
            field(3, "Arguments", Kind::Text, String::new()),
            field(3, "Env (K=V, …)", Kind::Secret, String::new()),
            field(3, "URL", Kind::Text, String::new()),
            field(3, "Token", Kind::Secret, String::new()),
            field(
                3,
                "Approval",
                Kind::Select(options(&[ASK, NEVER_ASK])),
                ASK.to_owned(),
            ),
        ];
        let auto = current.is_some_and(|c| matches!(c.approval, config::Approval::Auto { .. }));
        fields.push(field(
            5,
            "Approval",
            Kind::Select(options(&[ASK_ME, AUTO])),
            if auto { AUTO } else { ASK_ME }.to_owned(),
        ));
        let reviewer = match current.map(|c| (&c.approval, &c.model)) {
            Some((config::Approval::Auto { model }, main)) if model != main => model.as_str(),
            _ => SAME_MODEL,
        };
        let mut reviewers = vec![SAME_MODEL];
        reviewers.extend(MODELS);
        fields.push(field(
            5,
            "Reviewer model",
            Kind::Select(options_with(&reviewers, reviewer)),
            reviewer.to_owned(),
        ));
        let claude = current.and_then(|c| c.claude_code.as_ref());
        fields.push(field(
            4,
            "Claude Code",
            Kind::Select(options(&[DISABLED, ENABLED])),
            if claude.is_some() { ENABLED } else { DISABLED }.to_owned(),
        ));
        fields.push(field(
            4,
            "Directories",
            Kind::Text,
            claude.map_or_else(
                || "~/repos".to_owned(),
                |c| {
                    c.dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                },
            ),
        ));
        let mode = claude.map_or(config::CLAUDE_CODE_MODES[0], |c| c.permission_mode.as_str());
        fields.push(field(
            4,
            "Permissions",
            Kind::Select(options(config::CLAUDE_CODE_MODES)),
            mode.to_owned(),
        ));
        fields.push(field(
            4,
            "Allowed tools",
            Kind::Text,
            claude.map_or_else(String::new, |c| c.allowed_tools.join(", ")),
        ));
        let mcp_names: Vec<String> = current
            .map(|c| c.mcp.keys().cloned().collect())
            .unwrap_or_default();
        fields.extend(mcp_names.iter().map(|name| {
            field(
                3,
                name,
                Kind::Select(options(&[KEEP, REMOVE])),
                KEEP.to_owned(),
            )
        }));
        Ok(Self {
            fields,
            step: 0,
            focus: KEY,
            menu: None,
            reveal: false,
            status: Line::default(),
            force_save: false,
            search_results: searxng.map_or(5, |s| s.results),
            claude_extra: current
                .and_then(|c| c.claude_code.as_ref())
                .map_or((1800, "claude".to_owned()), |c| {
                    (c.timeout.as_secs(), c.command.clone())
                }),
            mcp_names,
        })
    }

    fn shown(&self, i: usize) -> bool {
        let field = &self.fields[i];
        field.step == self.step
            && match i {
                CUSTOM_MODEL => self.fields[MODEL].value == CUSTOM,
                TOKEN | USERS => self.fields[DISCORD].value == ENABLED,
                SEARCH_URL | SEARCH_USER | SEARCH_PASSWORD => self.fields[SEARCH].value == ENABLED,
                MCP_NAME | MCP_APPROVE => self.fields[MCP_ADD].value != NO,
                MCP_COMMAND | MCP_ARGS | MCP_ENV => self.fields[MCP_ADD].value == STDIO,
                MCP_URL | MCP_TOKEN => self.fields[MCP_ADD].value == HTTP,
                REVIEWER => self.fields[APPROVAL].value == AUTO,
                CLAUDE_DIRS | CLAUDE_MODE | CLAUDE_TOOLS => self.fields[CLAUDE].value == ENABLED,
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
                // Steps are checked together on save, so any step can be visited in any order.
                KeyCode::Left if self.step > 0 => self.go_to_step(self.step - 1),
                KeyCode::Right if self.step + 1 < STEPS.len() => self.go_to_step(self.step + 1),
                KeyCode::Up | KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Down | KeyCode::Tab => self.move_focus(1),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.reveal = !self.reveal;
                }
                KeyCode::Enter if self.focus == BUTTON => {
                    if let Some(config) = self.save(terminal, handle, path)? {
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
        // Steps can be skipped with ←→, so check them all and show the first one with a problem.
        if let Some((step, err)) =
            (0..STEPS.len()).find_map(|step| self.check_step(step).err().map(|err| (step, err)))
        {
            self.go_to_step(step);
            self.status = Line::from(format!("✗ {err}")).red();
            return Ok(None);
        }
        let answers = match self.answers() {
            Ok(answers) => answers,
            Err(err) => {
                self.status = Line::from(format!("✗ {err}")).red();
                return Ok(None);
            }
        };
        let (remove, add) = match self.mcp() {
            Ok(changes) => changes,
            Err(err) => {
                self.status = Line::from(format!("✗ {err}")).red();
                return Ok(None);
            }
        };
        let old = std::fs::read_to_string(path).ok();
        let text = merge_mcp(&render_toml(&answers), old.as_deref(), &remove, add);
        let text = keep_timezone(&text, old.as_deref());
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
        config::write(path, &text)?;
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

    /// Validates the fields of `step` only.
    fn check_step(&self, step: usize) -> std::result::Result<(), String> {
        match step {
            0 => {
                self.required(KEY)?;
                self.model()?;
            }
            1 => {
                self.discord()?;
            }
            2 => {
                self.search()?;
            }
            3 => {
                self.mcp()?;
            }
            4 => {
                self.claude_code()?;
            }
            _ => {
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

    fn search(&self) -> std::result::Result<Option<SearchAnswers>, String> {
        if self.fields[SEARCH].value != ENABLED {
            return Ok(None);
        }
        let url = self.required(SEARCH_URL)?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("SearXNG URL must start with http:// or https://".to_owned());
        }
        let auth = match (self.value(SEARCH_USER), self.value(SEARCH_PASSWORD)) {
            (user, password) if user.is_empty() && password.is_empty() => None,
            (user, password) if !user.is_empty() && !password.is_empty() => Some((user, password)),
            _ => return Err("fill in both Username and Password, or leave both empty".to_owned()),
        };
        Ok(Some(SearchAnswers {
            url,
            auth,
            results: self.search_results,
        }))
    }

    fn claude_code(&self) -> std::result::Result<Option<ClaudeAnswers>, String> {
        if self.fields[CLAUDE].value != ENABLED {
            return Ok(None);
        }
        let list = |i| -> Vec<String> {
            self.value(i)
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let dirs = list(CLAUDE_DIRS);
        if dirs.is_empty() {
            return Err("list at least one directory Claude Code may work in".to_owned());
        }
        let (timeout_secs, command) = self.claude_extra.clone();
        Ok(Some(ClaudeAnswers {
            dirs,
            permission_mode: self.value(CLAUDE_MODE),
            allowed_tools: list(CLAUDE_TOOLS),
            timeout_secs,
            command,
        }))
    }

    /// MCP servers to remove, and the one to add as `(name, table)`.
    fn mcp(&self) -> std::result::Result<McpChanges, String> {
        let remove: Vec<String> = self
            .mcp_names
            .iter()
            .enumerate()
            .filter(|(i, _)| self.fields[MCP_FIRST + i].value == REMOVE)
            .map(|(_, name)| name.clone())
            .collect();
        let kind = self.fields[MCP_ADD].value.as_str();
        if kind == NO {
            return Ok((remove, None));
        }
        let name = self.required(MCP_NAME)?;
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("server Name may use only letters, digits, _ and -".to_owned());
        }
        if self.mcp_names.contains(&name) && !remove.contains(&name) {
            return Err(format!("`{name}` exists; set it to Remove to replace it"));
        }
        let mut table = Table::new();
        if kind == STDIO {
            table["command"] = toml_edit::value(self.required(MCP_COMMAND)?);
            let args: Array = self.value(MCP_ARGS).split_whitespace().collect();
            if !args.is_empty() {
                table["args"] = toml_edit::value(args);
            }
            let mut env = InlineTable::new();
            for pair in self.value(MCP_ENV).split(',').map(str::trim) {
                if pair.is_empty() {
                    continue;
                }
                let Some((key, value)) = pair.split_once('=') else {
                    return Err("Env entries need KEY=VALUE, separated by commas".to_owned());
                };
                env.insert(key.trim(), value.trim().into());
            }
            if !env.is_empty() {
                table["env"] = toml_edit::value(env);
            }
        } else {
            let url = self.required(MCP_URL)?;
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("URL must start with http:// or https://".to_owned());
            }
            table["url"] = toml_edit::value(url);
            let token = self.value(MCP_TOKEN);
            if !token.is_empty() {
                table["token"] = toml_edit::value(token);
            }
        }
        if self.fields[MCP_APPROVE].value == NEVER_ASK {
            table["approve"] = toml_edit::value(false);
        }
        Ok((remove, Some((name, table))))
    }

    fn answers(&self) -> std::result::Result<Answers, String> {
        Ok(Answers {
            api_key: self.required(KEY)?,
            model: self.model()?,
            discord: self.discord()?,
            search: self.search()?,
            claude_code: self.claude_code()?,
            log_level: self.required(LOG)?,
            auto_approval: (self.fields[APPROVAL].value == AUTO).then(|| {
                let reviewer = self.value(REVIEWER);
                (reviewer != SAME_MODEL).then_some(reviewer)
            }),
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
                let label = "[ Save ]";
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
            "←→ step · ↑↓ move · Enter open/next · type to edit · ^R show secrets · Esc back · ^C quit"
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

/// Comma-separated Discord IDs; `what` names them in errors ("user", "channel").
fn parse_ids(text: &str, what: &str) -> std::result::Result<Vec<u64>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map_err(|_| format!("`{part}` is not a Discord {what} ID (digits only)"))
        })
        .collect()
}

fn parse_user_ids(text: &str) -> std::result::Result<Vec<u64>, String> {
    let ids = parse_ids(text, "user")?;
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

struct SearchAnswers {
    url: String,
    auth: Option<(String, String)>,
    results: usize,
}

struct ClaudeAnswers {
    dirs: Vec<String>,
    permission_mode: String,
    allowed_tools: Vec<String>,
    timeout_secs: u64,
    command: String,
}

struct Answers {
    model: String,
    api_key: String,
    discord: Option<DiscordAnswers>,
    search: Option<SearchAnswers>,
    claude_code: Option<ClaudeAnswers>,
    log_level: String,
    /// `Some` for auto approval, holding the reviewer model if it isn't the main one.
    auto_approval: Option<Option<String>>,
    database: PathBuf,
}

/// MCP servers to remove, and one to add as `(name, table)`.
type McpChanges = (Vec<String>, Option<(String, Table)>);

/// `text` with the `[mcp.servers]` from the `old` config file carried over, minus `remove`, plus `add`.
/// Old entries are copied as written, so their comments and secrets survive.
fn merge_mcp(
    text: &str,
    old: Option<&str>,
    remove: &[String],
    add: Option<(String, Table)>,
) -> String {
    let Ok(mut new) = text.parse::<DocumentMut>() else {
        return text.to_owned();
    };
    let mut servers = old
        .and_then(|old| old.parse::<DocumentMut>().ok())
        .and_then(|old| old.get("mcp")?.get("servers")?.as_table().cloned())
        .unwrap_or_default();
    for name in remove {
        servers.remove(name);
    }
    if let Some((name, table)) = add {
        servers.insert(&name, Item::Table(table));
    }
    if servers.is_empty() {
        return new.to_string();
    }
    servers.set_implicit(true);
    let mut mcp = Table::new();
    mcp.set_implicit(true);
    mcp.insert("servers", Item::Table(servers));
    new["mcp"] = Item::Table(mcp);
    new.to_string()
}

/// `text` with the top-level `timezone` from the `old` config file, which the form doesn't edit.
fn keep_timezone(text: &str, old: Option<&str>) -> String {
    let zone = old
        .and_then(|old| old.parse::<DocumentMut>().ok())
        .and_then(|old| old.get("timezone").cloned());
    match (text.parse::<DocumentMut>(), zone) {
        (Ok(mut new), Some(zone)) => {
            new.insert("timezone", zone);
            new.to_string()
        }
        _ => text.to_owned(),
    }
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
         [log]\n\
         level = {log}\n\
         \n\
         [database]\n\
         path = {db}\n",
        model = quote(&answers.model),
        key = quote(&answers.api_key),
        log = quote(&answers.log_level),
        db = quote(&answers.database.to_string_lossy()),
    );
    if let Some(reviewer) = &answers.auto_approval {
        text.push_str("\n[approval]\nmode = \"auto\"\n");
        if let Some(model) = reviewer {
            text.push_str(&format!("model = {}\n", quote(model)));
        }
    }
    if let Some(claude) = &answers.claude_code {
        let array = |items: &[String]| {
            toml::Value::Array(items.iter().cloned().map(toml::Value::String).collect()).to_string()
        };
        text.push_str(&format!(
            "\n[tools.claude_code]\ndirs = {}\npermission_mode = {}\nallowed_tools = {}\n\
             timeout_secs = {}\ncommand = {}\n",
            array(&claude.dirs),
            quote(&claude.permission_mode),
            array(&claude.allowed_tools),
            claude.timeout_secs,
            quote(&claude.command),
        ));
    }
    if let Some(search) = &answers.search {
        text.push_str(&format!(
            "\n[tools.searxng]\nurl = {}\nresults = {}\n",
            quote(&search.url),
            search.results
        ));
        if let Some((user, password)) = &search.auth {
            text.push_str(&format!(
                "username = {}\npassword = {}\n",
                quote(user),
                quote(password)
            ));
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_step_validates_new_server() {
        let config = Config::parse(
            "[opencode-go]\napi_key = \"k\"\n[mcp.servers.linear]\nurl = \"https://a.example\"\n",
        )
        .expect("valid");
        let mut form = Form::new(Some(&config)).expect("form");
        form.fields[MCP_ADD].value = HTTP.to_owned();
        form.fields[MCP_NAME].value = "linear".to_owned();
        form.fields[MCP_URL].value = "https://mcp.linear.app/mcp".to_owned();
        assert!(
            form.mcp().is_err(),
            "existing name is refused unless removed"
        );
        form.fields[MCP_FIRST].value = REMOVE.to_owned();
        let (remove, add) = form.mcp().expect("replaces it");
        assert_eq!(remove, ["linear"]);
        assert_eq!(add.expect("added").0, "linear");

        form.fields[MCP_ADD].value = STDIO.to_owned();
        form.fields[MCP_NAME].value = "gh".to_owned();
        form.fields[MCP_COMMAND].value = "npx".to_owned();
        form.fields[MCP_ARGS].value = "-y  server-github".to_owned();
        form.fields[MCP_ENV].value = "TOKEN=abc, MODE=ro".to_owned();
        let (_, add) = form.mcp().expect("valid");
        let merged = merge_mcp("[opencode-go]\napi_key = \"k\"\n", None, &[], add);
        let gh = &Config::parse(&merged).expect("valid").mcp["gh"];
        assert_eq!(gh.args, ["-y", "server-github"]);
        assert_eq!(gh.env["MODE"].as_str(), "ro");
        form.fields[MCP_ENV].value = "oops".to_owned();
        assert!(form.mcp().is_err());
    }

    #[test]
    fn keep_timezone_carries_it_over() {
        let new = "[opencode-go]\napi_key = \"k\"\n";
        let kept = keep_timezone(
            new,
            Some("timezone = \"Asia/Taipei\"\n[log]\nlevel = \"warn\"\n"),
        );
        let config = Config::parse(&kept).expect("valid");
        assert_eq!(config.timezone, chrono_tz::Tz::Asia__Taipei);
        assert_eq!(keep_timezone(new, None), new);
    }

    #[test]
    fn merge_mcp_keeps_removes_and_adds_servers() {
        let new = "[opencode-go]\napi_key = \"k\"\n";
        let old = "[opencode-go]\napi_key = \"old\"\n\n\
                   [mcp.servers.fs]\ncommand = \"npx\"\n\n\
                   [mcp.servers.gone]\nurl = \"https://gone.example\"\n";
        let mut linear = Table::new();
        linear["url"] = toml_edit::value("https://mcp.linear.app/mcp");
        linear["token"] = toml_edit::value("lin_api_x");
        let merged = merge_mcp(
            new,
            Some(old),
            &["gone".to_owned()],
            Some(("linear".to_owned(), linear)),
        );
        let config = Config::parse(&merged).expect("valid");
        assert_eq!(config.api_key.as_str(), "k");
        assert_eq!(config.mcp["fs"].command.as_deref(), Some("npx"));
        assert!(!config.mcp.contains_key("gone"));
        assert_eq!(
            config.mcp["linear"].token.as_ref().map(|t| t.as_str()),
            Some("lin_api_x")
        );
        assert_eq!(merge_mcp(new, None, &[], None), new);
    }

    #[test]
    fn rendered_config_parses_back() {
        let text = render_toml(&Answers {
            model: "qwen3.8-max".to_owned(),
            api_key: "sk-\"quoted\"".to_owned(),
            discord: Some(DiscordAnswers {
                token: "tok".to_owned(),
                allowed_users: vec![1, 22],
            }),
            claude_code: Some(ClaudeAnswers {
                dirs: vec!["~/repos".to_owned(), "/srv/app".to_owned()],
                permission_mode: "plan".to_owned(),
                allowed_tools: vec!["Bash(cargo test:*)".to_owned()],
                timeout_secs: 600,
                command: "claude".to_owned(),
            }),
            search: Some(SearchAnswers {
                url: "https://search.example.com".to_owned(),
                auth: Some(("me".to_owned(), "p\"w".to_owned())),
                results: 8,
            }),
            log_level: "info".to_owned(),
            auto_approval: Some(Some("glm-5.3-flash".to_owned())),
            database: PathBuf::from("/tmp/m.db"),
        });
        let config = Config::parse(&text).expect("valid");
        assert_eq!(config.model, "qwen3.8-max");
        let claude = config.claude_code.clone().expect("claude code");
        assert_eq!(
            claude.dirs,
            [PathBuf::from("~/repos"), PathBuf::from("/srv/app")]
        );
        assert_eq!(claude.permission_mode, "plan");
        assert_eq!(claude.allowed_tools, ["Bash(cargo test:*)"]);
        assert_eq!(claude.timeout.as_secs(), 600);
        assert_eq!(
            config.approval,
            config::Approval::Auto {
                model: "glm-5.3-flash".to_owned()
            }
        );
        assert_eq!(config.api_key.as_str(), "sk-\"quoted\"");
        assert_eq!(config.discord.map(|d| d.allowed_users), Some(vec![1, 22]));
        let searxng = config.searxng.expect("search section");
        assert_eq!(searxng.url, "https://search.example.com");
        assert_eq!(searxng.results, 8);
        let (user, password) = searxng.auth.expect("basic auth");
        assert_eq!((user.as_str(), password.as_str()), ("me", "p\"w"));
    }

    #[test]
    fn steps_validate_and_hide_conditional_fields() {
        let mut form = Form::new(None).expect("form");
        assert_eq!(
            form.check_step(form.step).err().as_deref(),
            Some("API key is required")
        );
        form.fields[KEY].value = "k".to_owned();
        assert!(!form.visible().contains(&CUSTOM_MODEL));
        form.fields[MODEL].value = CUSTOM.to_owned();
        assert!(form.visible().contains(&CUSTOM_MODEL));
        assert!(
            form.check_step(form.step).is_err(),
            "custom model needs an ID"
        );
        form.fields[CUSTOM_MODEL].value = "qwen-next".to_owned();
        form.check_step(form.step).expect("step 1 valid");

        form.step = 1;
        assert!(!form.visible().contains(&TOKEN));
        form.fields[DISCORD].value = ENABLED.to_owned();
        assert!(form.visible().contains(&TOKEN));
        form.fields[TOKEN].value = "t".to_owned();
        form.fields[USERS].value = "12, x".to_owned();
        assert!(form.check_step(form.step).is_err());
        form.fields[USERS].value = "12, 34".to_owned();

        form.step = 2;
        assert!(!form.visible().contains(&SEARCH_URL));
        form.fields[SEARCH].value = ENABLED.to_owned();
        assert!(form.visible().contains(&SEARCH_PASSWORD));
        form.fields[SEARCH_URL].value = "search.example.com".to_owned();
        assert!(form.check_step(form.step).is_err(), "URL needs a scheme");
        form.fields[SEARCH_URL].value = "https://search.example.com".to_owned();
        form.fields[SEARCH_USER].value = "me".to_owned();
        assert!(
            form.check_step(form.step).is_err(),
            "username without password"
        );
        form.fields[SEARCH_PASSWORD].value = "pw".to_owned();
        form.check_step(form.step).expect("search step valid");

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
