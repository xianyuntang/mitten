//! `mitten configure`: a step-by-step, full-screen setup with dropdowns.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListState, Padding, Paragraph, Wrap};

use crate::config::{self, Config};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

/// Common OpenCode Go models; `config::Api::for_model` picks each one's endpoint format.
pub const MODELS: &[&str] = &[
    "minimax-m3",
    "qwen3.8-max",
    "qwen3.8-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "kimi-k3",
    "kimi-k2.7-code",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
    "longcat-2.0",
    "claude-haiku-5-5",
    "gpt-6-luna",
    "gpt-5.6-luna",
    "grok-4.7",
    "muse-spark-1.3-contributor",
    "gemini-4-argon-contributor",
];
const CUSTOM: &str = "Custom…";
const ENABLED: &str = "Enabled";
const DISABLED: &str = "Disabled";
const SEARXNG: &str = "SearXNG";
const BRAVE: &str = "Brave Search API";
const STEPS: [&str; 5] = ["Model", "Discord", "Web search", "MCP", "Advanced"];
const NO: &str = "No";
const YES: &str = "Yes";
const EVERY_MESSAGE: &str = "Every message";
const MENTIONED: &str = "Only when @mentioned";
const STDIO: &str = "Local command (stdio)";
const HTTP: &str = "Remote URL (HTTP)";
const KEEP: &str = "Keep";
const REMOVE: &str = "Remove";
const ASK: &str = "Ask every call";
const ASK_ME: &str = "Ask me every time";
const AUTO: &str = "Auto (LLM review)";
const SAME_MODEL: &str = "Same as main model";
const NEVER_ASK: &str = "Never ask";

/// Shows the setup and saves the config to `path`.
pub async fn run(path: &Path) -> Result<Config> {
    let current = path.exists().then(|| Config::load(path).ok()).flatten();
    let mut form = Form::new(current.as_ref())?;
    let target = path.to_owned();
    let saved =
        tokio::task::spawn_blocking(move || ratatui::run(|terminal| form.run(terminal, &target)))
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
const TIMEZONE: usize = 23;
const BRAVE_KEY: usize = 24;
const GUILD_ADD: usize = 25;
const GUILD_ID: usize = 26;
const GUILD_USERS: usize = 27;
const GUILD_REPLY: usize = 28;
/// First of the rows for already configured MCP servers, followed by the configured Discord
/// servers' rows.
const MCP_FIRST: usize = 29;

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
    /// `tools.searxng.results` or `tools.brave.results`, not on the form; kept so saving doesn't
    /// reset a hand-edited value.
    search_results: usize,
    /// Configured MCP servers, one Keep / Remove row each from `MCP_FIRST` on.
    mcp_names: Vec<String>,
    /// Configured Discord servers, one Keep / Remove row each after the MCP rows.
    guilds: Vec<(u64, config::GuildRule)>,
    /// Set once a save was tried, so empty required fields and unfinished steps show as errors.
    attempted: bool,
    /// `NO_COLOR` is set: draw without colors, relying on the ✓ ✗ ● ▌ marks.
    plain: bool,
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
        let search = current.and_then(|c| c.search.as_ref());
        let (searxng, brave) = match search {
            Some(config::Search::Searxng(searxng)) => (Some(searxng), None),
            Some(config::Search::Brave(brave)) => (None, Some(brave)),
            None => (None, None),
        };
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
                    .map(|d| join_ids(&d.allowed_users))
                    .unwrap_or_default(),
            ),
            field(
                2,
                "Web search",
                Kind::Select(options(&[DISABLED, SEARXNG, BRAVE])),
                match search {
                    Some(config::Search::Searxng(_)) => SEARXNG,
                    Some(config::Search::Brave(_)) => BRAVE,
                    None => DISABLED,
                }
                .to_owned(),
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
                4,
                "Log level",
                Kind::Select(options_with(&["error", "warn", "info", "debug"], log)),
                log.to_owned(),
            ),
            field(
                4,
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
            4,
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
            4,
            "Reviewer model",
            Kind::Select(options_with(&reviewers, reviewer)),
            reviewer.to_owned(),
        ));
        let zone = current.map_or_else(config::system_timezone, |c| c.timezone);
        fields.push(field(4, "Time zone", Kind::Text, zone.name().to_owned()));
        fields.push(field(
            2,
            "Brave API key",
            Kind::Secret,
            brave
                .map(|b| b.api_key.as_str().to_owned())
                .unwrap_or_default(),
        ));
        fields.push(field(
            1,
            "Add server",
            Kind::Select(options(&[NO, YES])),
            NO.to_owned(),
        ));
        fields.push(field(1, "Server ID", Kind::Text, String::new()));
        fields.push(field(1, "Server user IDs", Kind::Text, String::new()));
        fields.push(field(
            1,
            "Reply to",
            Kind::Select(options(&[EVERY_MESSAGE, MENTIONED])),
            EVERY_MESSAGE.to_owned(),
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
        let guilds: Vec<(u64, config::GuildRule)> = discord
            .map(|d| d.guilds.clone().into_iter().collect())
            .unwrap_or_default();
        fields.extend(guilds.iter().map(|(id, rule)| {
            let reply = if rule.mention_only {
                MENTIONED
            } else {
                EVERY_MESSAGE
            };
            let keep = format!(
                "{KEEP} · {} · {}",
                match &rule.allowed_users {
                    config::Users::Everyone => "everyone".to_owned(),
                    config::Users::Only(ids) => join_ids(ids),
                },
                reply.to_lowercase()
            );
            field(
                1,
                &id.to_string(),
                Kind::Select(vec![keep.clone(), REMOVE.to_owned()]),
                keep,
            )
        }));
        Ok(Self {
            fields,
            step: 0,
            focus: KEY,
            menu: None,
            reveal: false,
            status: Line::default(),
            search_results: search.map_or(5, config::Search::results),
            mcp_names,
            guilds,
            attempted: false,
            plain: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
        })
    }

    /// Index of the first configured Discord server's row.
    fn guild_first(&self) -> usize {
        MCP_FIRST + self.mcp_names.len()
    }

    fn shown(&self, i: usize) -> bool {
        let field = &self.fields[i];
        field.step == self.step
            && match i {
                CUSTOM_MODEL => self.fields[MODEL].value == CUSTOM,
                TOKEN | USERS | GUILD_ADD => self.fields[DISCORD].value == ENABLED,
                GUILD_ID | GUILD_USERS | GUILD_REPLY => {
                    self.fields[DISCORD].value == ENABLED && self.fields[GUILD_ADD].value == YES
                }
                i if i >= self.guild_first() => self.fields[DISCORD].value == ENABLED,
                SEARCH_URL | SEARCH_USER | SEARCH_PASSWORD => self.fields[SEARCH].value == SEARXNG,
                BRAVE_KEY => self.fields[SEARCH].value == BRAVE,
                MCP_NAME | MCP_APPROVE => self.fields[MCP_ADD].value != NO,
                MCP_COMMAND | MCP_ARGS | MCP_ENV => self.fields[MCP_ADD].value == STDIO,
                MCP_URL | MCP_TOKEN => self.fields[MCP_ADD].value == HTTP,
                REVIEWER => self.fields[APPROVAL].value == AUTO,
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
    fn run(&mut self, terminal: &mut DefaultTerminal, path: &Path) -> Result<Option<Config>> {
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
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(config) = self.save(path)? {
                        return Ok(Some(config));
                    }
                }
                KeyCode::Enter if self.focus == BUTTON => {
                    if let Some(config) = self.save(path)? {
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

    /// Any edit clears a previous save error.
    fn edited(&mut self) {
        self.status = Line::default();
    }

    fn save(&mut self, path: &Path) -> Result<Option<Config>> {
        self.attempted = true;
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
        let config = Config::parse(&text).context("form produced an invalid config")?;

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
            _ => {
                self.required(DATABASE)?;
                self.timezone()?;
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

    /// `None` when empty, meaning this machine's time zone.
    fn timezone(&self) -> std::result::Result<Option<String>, String> {
        let zone = self.value(TIMEZONE);
        if zone.is_empty() {
            return Ok(None);
        }
        zone.parse::<chrono_tz::Tz>()
            .map_err(|_| format!("Time zone `{zone}` is not an IANA name like Asia/Taipei"))?;
        Ok(Some(zone))
    }

    fn discord(&self) -> std::result::Result<Option<DiscordAnswers>, String> {
        if self.fields[DISCORD].value != ENABLED {
            return Ok(None);
        }
        Ok(Some(DiscordAnswers {
            token: self.required(TOKEN)?,
            allowed_users: parse_user_ids(&self.value(USERS))?,
            guilds: self.guild_answers()?,
        }))
    }

    /// Configured servers not set to Remove, plus the one being added.
    fn guild_answers(&self) -> std::result::Result<Vec<GuildAnswers>, String> {
        let first = self.guild_first();
        let mut guilds: Vec<GuildAnswers> = self
            .guilds
            .iter()
            .enumerate()
            .filter(|(i, _)| self.fields[first + i].value != REMOVE)
            .map(|(_, (id, rule))| GuildAnswers {
                id: *id,
                mention_only: rule.mention_only,
                allowed_users: rule.allowed_users.clone(),
            })
            .collect();
        if self.fields[GUILD_ADD].value != YES {
            return Ok(guilds);
        }
        let id = parse_server_id(&self.required(GUILD_ID)?)?;
        if guilds.iter().any(|g| g.id == id) {
            return Err(format!(
                "server {id} exists; set it to Remove to replace it"
            ));
        }
        guilds.push(GuildAnswers {
            id,
            mention_only: self.fields[GUILD_REPLY].value == MENTIONED,
            allowed_users: match self.value(GUILD_USERS).as_str() {
                "*" => config::Users::Everyone,
                ids => config::Users::Only(parse_user_ids(ids)?),
            },
        });
        Ok(guilds)
    }

    fn search(&self) -> std::result::Result<Option<SearchAnswers>, String> {
        match self.fields[SEARCH].value.as_str() {
            SEARXNG => {}
            BRAVE => {
                return Ok(Some(SearchAnswers::Brave {
                    api_key: self.required(BRAVE_KEY)?,
                    results: self.search_results,
                }));
            }
            _ => return Ok(None),
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
        Ok(Some(SearchAnswers::Searxng {
            url,
            auth,
            results: self.search_results,
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
            log_level: self.required(LOG)?,
            auto_approval: (self.fields[APPROVAL].value == AUTO).then(|| {
                let reviewer = self.value(REVIEWER);
                (reviewer != SAME_MODEL).then_some(reviewer)
            }),
            database: PathBuf::from(self.required(DATABASE)?),
            timezone: self.timezone()?,
        })
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let rows = self.visible();
        // The Save row takes two lines: a gap, then the button.
        let fields_height = (rows.len() as u16 + 3).max(STEPS.len() as u16 + 2);
        let [header, main, help_area, status, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(fields_height),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [sidebar, body] =
            Layout::horizontal([Constraint::Length(18), Constraint::Fill(1)]).areas(main);

        frame.render_widget(
            Line::from(vec![
                " mitten ".black().on_cyan().bold(),
                "configure".bold(),
            ]),
            header,
        );
        frame.render_widget(
            Line::from(concat!("v", env!("CARGO_PKG_VERSION"), " "))
                .dark_gray()
                .right_aligned(),
            header,
        );

        let steps: Vec<Line> = STEPS
            .iter()
            .enumerate()
            .map(|(step, name)| {
                let (mark, style) = if step == self.step {
                    (
                        "●",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                } else if self.check_step(step).is_ok() {
                    ("✓", Style::new().fg(Color::Green))
                } else if self.attempted {
                    ("✗", Style::new().fg(Color::Red))
                } else {
                    ("○", Style::new().fg(Color::DarkGray))
                };
                let bar = if step == self.step { "▌" } else { " " };
                Line::from(vec![
                    bar.cyan(),
                    Span::styled(format!("{mark} {name}"), style),
                ])
            })
            .collect();
        frame.render_widget(
            Paragraph::new(steps).block(Block::bordered().border_type(BorderType::Rounded)),
            sidebar,
        );

        let mut lines: Vec<Line> = Vec::new();
        let mut menu_anchor = None;
        for (row, &i) in rows.iter().enumerate() {
            let focused = i == self.focus;
            if i == BUTTON {
                lines.push(Line::default());
                let button = if focused {
                    " Save ".black().on_cyan().bold()
                } else {
                    " Save ".cyan().on_dark_gray()
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
            let error = self.field_error(i);
            let value_style = if error.is_some() {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            let mark = match error {
                Some(_) => "  ✗".red(),
                None if CHECKED.contains(&i) && !field.value.trim().is_empty() => "  ✓".green(),
                None => Span::raw(""),
            };
            let line = Line::from(vec![
                if focused {
                    "▌ ".cyan()
                } else {
                    Span::raw("  ")
                },
                Span::styled(format!("{:<20} ", field.label), label_style),
                Span::styled(shown, value_style),
                Span::raw(cursor),
                mark,
            ]);
            lines.push(if focused {
                line.style(Style::new().bg(FOCUS_BG))
            } else {
                line
            });
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::Cyan))
                    .title(format!(" {} ", STEPS[self.step]).bold()),
            ),
            body,
        );

        let (title, about, warning) = match self.fields.get(self.focus) {
            Some(field) => {
                let (about, warning) = help(self.focus);
                (field.label.as_str(), about, warning)
            }
            None => (
                "Save",
                "Tests the connection to the model, then writes the config. ^S saves from any field.",
                "",
            ),
        };
        let mut text = vec![Line::from(about)];
        if !warning.is_empty() {
            text.push(Line::from(format!("⚠ {warning}")).yellow());
        }
        if let Some(err) = self.field_error(self.focus) {
            text.push(Line::from(format!("✗ {err}")).red());
        }
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::DarkGray))
                    .padding(Padding::horizontal(1))
                    .title(format!(" {title} ")),
            ),
            help_area,
        );
        frame.render_widget(Paragraph::new(self.status.clone()), status);
        let keys_text = if self.menu.is_some() {
            " ↑↓ choose  Enter select  Esc close"
        } else {
            " ←→ step  ↑↓ field  Enter open/next  ^R show secrets  ^S save  Esc back  ^C quit"
        };
        frame.render_widget(Paragraph::new(keys_text).dark_gray(), keys);

        if let (Some(menu), Some(anchor), Some(Kind::Select(values))) = (
            &mut self.menu,
            menu_anchor,
            self.fields.get(self.focus).map(|f| &f.kind),
        ) {
            let area = dropdown_area(body, anchor, values.len() as u16);
            let list = List::new(values.iter().map(String::as_str))
                .block(Block::bordered().border_type(BorderType::Rounded))
                .highlight_style(Style::new().black().on_cyan())
                .highlight_symbol("› ");
            frame.render_widget(Clear, area);
            frame.render_stateful_widget(list, area, menu);
        }
        if self.plain {
            for cell in frame.buffer_mut().content.iter_mut() {
                cell.set_fg(Color::Reset).set_bg(Color::Reset);
            }
        }
    }

    /// What is wrong with field `i`'s value, if anything. Empty required fields only count once
    /// a save was tried, so a fresh form isn't all red.
    fn field_error(&self, i: usize) -> Option<String> {
        if !self.fields.get(i).is_some_and(|_| self.shown(i)) {
            return None;
        }
        let value = self.value(i);
        if value.is_empty() {
            let required = matches!(
                i,
                KEY | CUSTOM_MODEL
                    | TOKEN
                    | USERS
                    | GUILD_ID
                    | GUILD_USERS
                    | SEARCH_URL
                    | BRAVE_KEY
                    | MCP_NAME
                    | MCP_COMMAND
                    | MCP_URL
                    | DATABASE
            );
            return (self.attempted && required)
                .then(|| format!("{} is required", self.fields[i].label));
        }
        match i {
            USERS => parse_user_ids(&value).err(),
            GUILD_ID => parse_server_id(&value).err(),
            GUILD_USERS if value != "*" => parse_user_ids(&value).err(),
            TIMEZONE => self.timezone().err(),
            SEARCH_URL | MCP_URL => (!value.starts_with("http://")
                && !value.starts_with("https://"))
            .then(|| "must start with http:// or https://".to_owned()),
            _ => None,
        }
    }
}

/// Fields whose value is checked as it is typed, so a valid one gets a ✓.
const CHECKED: &[usize] = &[USERS, GUILD_ID, GUILD_USERS, TIMEZONE, SEARCH_URL, MCP_URL];

/// Background of the focused row.
const FOCUS_BG: Color = Color::Indexed(236);

/// Help for field `i`, and a warning to show with it ("" for none).
fn help(i: usize) -> (&'static str, &'static str) {
    let about = match i {
        KEY => "Your OpenCode Go API key. ^R shows it.",
        MODEL => "The model Mitten talks with. Pick Custom… to type another model ID.",
        CUSTOM_MODEL => "An OpenCode Go model ID.",
        DISCORD => "Lets `mitten serve` chat through a Discord bot, by DM and in server channels.",
        TOKEN => {
            "Bot token from the Discord Developer Portal › Bot. Turn on Message Content Intent \
             there too, or the bot cannot connect."
        }
        USERS => {
            "Comma-separated Discord user IDs, used in DMs and in servers not added below. With \
             Developer Mode on, right-click a name › Copy User ID."
        }
        GUILD_ADD => {
            "Give one server its own users and reply rule. Servers already added are listed \
             below; set one to Remove to drop or replace it."
        }
        GUILD_ID => "With Developer Mode on, right-click the server › Copy Server ID.",
        GUILD_USERS => "Comma-separated user IDs, or * for everyone in the server.",
        GUILD_REPLY => "Every message, or only messages that @mention the bot, in threads too.",
        SEARCH => "Gives the model a web_search tool through SearXNG or the Brave Search API.",
        SEARCH_URL => "Your SearXNG instance, starting with http:// or https://.",
        SEARCH_USER | SEARCH_PASSWORD => {
            "Basic auth for SearXNG, if it needs it. Give both or neither."
        }
        BRAVE_KEY => "Your Brave Search API key.",
        LOG => "How much `mitten serve` logs.",
        DATABASE => "SQLite file that holds conversations and memory.",
        MCP_ADD => {
            "Connect an MCP server, a local command or a remote URL. Its tools reach the model \
             as <name>__<tool>."
        }
        MCP_NAME => "Letters, digits, _ and -. Tools are named <name>__<tool>.",
        MCP_COMMAND => "The program to start, like npx.",
        MCP_ARGS => "Arguments, separated by spaces.",
        MCP_ENV => "Environment variables as KEY=VALUE, separated by commas.",
        MCP_URL => "The server's URL, starting with http:// or https://.",
        MCP_TOKEN => "Bearer token for the server, if it needs one.",
        MCP_APPROVE => "Ask before each of this server's tool calls, or let them run.",
        APPROVAL => {
            "Who approves actions like settings changes and MCP calls: you every time, or \
             a reviewer model that passes low-risk ones and asks you about the rest."
        }
        REVIEWER => "The model that reviews actions when approval is Auto.",
        TIMEZONE => "IANA name like Asia/Taipei, for message times and scheduled jobs.",
        0 => "Where Mitten's model runs. Only OpenCode Go for now.",
        _ => "Keep it, or Remove it.",
    };
    let warning = match i {
        GUILD_USERS => {
            "With *, anyone in the server can make the bot read files on this machine. Only the \
             main list can approve actions."
        }
        MCP_APPROVE => "Never ask lets the model call this server's tools on its own.",
        _ => "",
    };
    (about, warning)
}

/// Places the dropdown just under field row `row`, aligned with the value column.
fn dropdown_area(body: Rect, row: u16, items: u16) -> Rect {
    let x = body.x + 1 + 2 + 21;
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
    guilds: Vec<GuildAnswers>,
}

/// One `[discord.guilds.<id>]` table.
struct GuildAnswers {
    id: u64,
    mention_only: bool,
    allowed_users: config::Users,
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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

fn parse_server_id(text: &str) -> std::result::Result<u64, String> {
    text.parse::<u64>()
        .map_err(|_| format!("`{text}` is not a Discord server ID (digits only)"))
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

enum SearchAnswers {
    Searxng {
        url: String,
        auth: Option<(String, String)>,
        results: usize,
    },
    Brave {
        api_key: String,
        results: usize,
    },
}

struct Answers {
    model: String,
    api_key: String,
    discord: Option<DiscordAnswers>,
    search: Option<SearchAnswers>,
    log_level: String,
    /// `Some` for auto approval, holding the reviewer model if it isn't the main one.
    auto_approval: Option<Option<String>>,
    database: PathBuf,
    /// `None` follows this machine's time zone.
    timezone: Option<String>,
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

/// Quotes and escapes a TOML string.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn render_toml(answers: &Answers) -> String {
    let mut text =
        "# Written by `mitten configure`; run it again to change these settings.\n".to_owned();
    if let Some(zone) = &answers.timezone {
        text.push_str(&format!("\ntimezone = {}\n", quote(zone)));
    }
    text.push_str(&format!(
        "\n\
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
    ));
    if let Some(reviewer) = &answers.auto_approval {
        text.push_str("\n[approval]\nmode = \"auto\"\n");
        if let Some(model) = reviewer {
            text.push_str(&format!("model = {}\n", quote(model)));
        }
    }
    match &answers.search {
        Some(SearchAnswers::Searxng { url, auth, results }) => {
            text.push_str(&format!(
                "\n[tools.searxng]\nurl = {}\nresults = {results}\n",
                quote(url)
            ));
            if let Some((user, password)) = auth {
                text.push_str(&format!(
                    "username = {}\npassword = {}\n",
                    quote(user),
                    quote(password)
                ));
            }
        }
        Some(SearchAnswers::Brave { api_key, results }) => text.push_str(&format!(
            "\n[tools.brave]\napi_key = {}\nresults = {results}\n",
            quote(api_key)
        )),
        None => {}
    }
    if let Some(discord) = &answers.discord {
        text.push_str(&format!(
            "\n[discord]\ntoken = {}\nallowed_users = [{}]\n",
            quote(&discord.token),
            join_ids(&discord.allowed_users)
        ));
        for guild in &discord.guilds {
            text.push_str(&format!(
                "\n[discord.guilds.{}]\nallowed_users = {}\n",
                guild.id,
                match &guild.allowed_users {
                    config::Users::Everyone => quote("*"),
                    config::Users::Only(ids) => format!("[{}]", join_ids(ids)),
                }
            ));
            if guild.mention_only {
                text.push_str("mention_only = true\n");
            }
        }
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
                guilds: vec![
                    GuildAnswers {
                        id: 100,
                        mention_only: true,
                        allowed_users: config::Users::Only(vec![2, 3]),
                    },
                    GuildAnswers {
                        id: 200,
                        mention_only: false,
                        allowed_users: config::Users::Everyone,
                    },
                ],
            }),
            search: Some(SearchAnswers::Searxng {
                url: "https://search.example.com".to_owned(),
                auth: Some(("me".to_owned(), "p\"w".to_owned())),
                results: 8,
            }),
            log_level: "info".to_owned(),
            auto_approval: Some(Some("glm-5.3-flash".to_owned())),
            database: PathBuf::from("/tmp/m.db"),
            timezone: Some("Asia/Taipei".to_owned()),
        });
        let config = Config::parse(&text).expect("valid");
        assert_eq!(config.timezone, chrono_tz::Tz::Asia__Taipei);
        assert_eq!(config.model, "qwen3.8-max");
        assert_eq!(
            config.approval,
            config::Approval::Auto {
                model: "glm-5.3-flash".to_owned()
            }
        );
        assert_eq!(config.api_key.as_str(), "sk-\"quoted\"");
        let discord = config.discord.as_ref().expect("discord");
        assert_eq!(discord.allowed_users, [1, 22]);
        assert!(discord.mention_only(Some(100)) && discord.can_talk(Some(100), 2));
        assert_eq!(discord.guilds[&200].allowed_users, config::Users::Everyone);
        let mut reopened = Form::new(Some(&config)).expect("form");
        let first = reopened.guild_first();
        assert_eq!(reopened.fields[first].label, "100");
        assert_eq!(
            reopened.fields[first].value,
            "Keep · 2, 3 · only when @mentioned"
        );
        assert_eq!(
            reopened.fields[first + 1].value,
            "Keep · everyone · every message"
        );
        reopened.fields[first + 1].value = REMOVE.to_owned();
        let kept = reopened.guild_answers().expect("valid");
        assert_eq!(
            kept.iter()
                .map(|g| (g.id, g.mention_only, g.allowed_users.clone()))
                .collect::<Vec<_>>(),
            [(100, true, config::Users::Only(vec![2, 3]))]
        );
        let Some(config::Search::Searxng(searxng)) = config.search else {
            panic!("search section");
        };
        assert_eq!(searxng.url, "https://search.example.com");
        assert_eq!(searxng.results, 8);
        let (user, password) = searxng.auth.expect("basic auth");
        assert_eq!((user.as_str(), password.as_str()), ("me", "p\"w"));
    }

    #[test]
    fn brave_choice_renders_its_section() {
        let mut form = Form::new(None).expect("form");
        form.fields[SEARCH].value = BRAVE.to_owned();
        form.step = 2;
        assert!(form.visible().contains(&BRAVE_KEY));
        assert!(!form.visible().contains(&SEARCH_URL));
        assert!(form.check_step(2).is_err(), "Brave needs a key");
        form.fields[BRAVE_KEY].value = "bk".to_owned();
        let Some(answers) = form.search().expect("valid") else {
            panic!("search on");
        };
        let text = render_toml(&Answers {
            model: "m".to_owned(),
            api_key: "k".to_owned(),
            discord: None,
            search: Some(answers),
            log_level: "warn".to_owned(),
            auto_approval: None,
            database: PathBuf::from("/tmp/m.db"),
            timezone: None,
        });
        let config = Config::parse(&text).expect("valid");
        let Some(config::Search::Brave(brave)) = &config.search else {
            panic!("brave section");
        };
        assert_eq!((brave.api_key.as_str(), brave.results), ("bk", 5));
        let reopened = Form::new(Some(&config)).expect("form");
        assert_eq!(reopened.fields[SEARCH].value, BRAVE);
        assert_eq!(reopened.fields[BRAVE_KEY].value, "bk");
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
        assert!(!form.visible().contains(&GUILD_ID));
        form.fields[GUILD_ADD].value = YES.to_owned();
        assert!(form.visible().contains(&GUILD_REPLY));
        assert!(form.check_step(form.step).is_err(), "server needs an ID");
        assert_eq!(
            form.field_error(GUILD_ID),
            None,
            "empty only counts after a save"
        );
        form.attempted = true;
        assert!(form.field_error(GUILD_ID).is_some());
        form.fields[GUILD_ID].value = "x".to_owned();
        form.fields[GUILD_USERS].value = "12".to_owned();
        assert!(form.field_error(GUILD_ID).is_some());
        assert!(form.check_step(form.step).is_err(), "server ID is digits");
        form.fields[GUILD_ID].value = "100".to_owned();
        form.fields[GUILD_USERS].value = String::new();
        assert!(form.check_step(form.step).is_err(), "server needs users");
        form.fields[GUILD_USERS].value = "*".to_owned();
        form.fields[GUILD_REPLY].value = MENTIONED.to_owned();
        form.check_step(form.step).expect("discord step valid");

        form.step = 2;
        assert!(!form.visible().contains(&SEARCH_URL));
        form.fields[SEARCH].value = SEARXNG.to_owned();
        assert!(form.visible().contains(&SEARCH_PASSWORD));
        assert!(!form.visible().contains(&BRAVE_KEY));
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

        form.step = 4;
        form.fields[TIMEZONE].value = "Taipei".to_owned();
        assert!(form.check_step(form.step).is_err(), "not an IANA name");
        form.fields[TIMEZONE].value = "Asia/Taipei".to_owned();
        form.check_step(form.step).expect("advanced step valid");

        let answers = form.answers().expect("all steps valid");
        assert_eq!(answers.model, "qwen-next");
        let discord = answers.discord.expect("discord");
        assert_eq!(discord.allowed_users, [12, 34]);
        assert_eq!(
            discord
                .guilds
                .iter()
                .map(|g| (g.id, g.mention_only, g.allowed_users.clone()))
                .collect::<Vec<_>>(),
            [(100, true, config::Users::Everyone)]
        );
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
        assert!(screen.contains("● Model"));
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
