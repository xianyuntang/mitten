mod agent;
mod chat;
mod config;
mod db;
mod discord;
mod memory;
mod onboarding;
mod service;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::BoxMakeWriter;

const USAGE: &str = "usage: mitten [chat|serve|configure|install|uninstall] [--config PATH]

  chat       talk in this terminal (default)
  configure  interactive setup: API key, model, Discord
  serve      run the Discord bot from [discord]
  install    run `mitten serve` in the background and keep it alive (launchd or systemd)
  uninstall  remove the background service";

#[tokio::main]
async fn main() -> Result<()> {
    let mut command = None;
    let mut config_path = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                config_path = Some(PathBuf::from(args.next().context("--config needs a path")?))
            }
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return Ok(());
            }
            Some(name @ ("chat" | "serve" | "configure" | "install" | "uninstall"))
                if command.is_none() =>
            {
                command = Some(name.to_owned())
            }
            _ => bail!("unexpected argument {arg:?}\n\n{USAGE}"),
        }
    }
    if command.as_deref() == Some("uninstall") {
        return service::uninstall();
    }
    let config_path = match config_path {
        Some(path) => std::path::absolute(path).context("failed to resolve --config path")?,
        None => std::env::home_dir()
            .context("cannot find home directory")?
            .join(".config/mitten/config.toml"),
    };

    if command.as_deref() == Some("configure") {
        onboarding::run(&config_path).await?;
        return Ok(());
    }
    // First run in a terminal: set up instead of failing on the missing file.
    let config = if !config_path.exists() && std::io::stdin().is_terminal() {
        onboarding::run(&config_path).await?
    } else {
        config::Config::load(&config_path)?
    };
    if command.as_deref() == Some("install") {
        if config.discord.is_none() {
            bail!("add a [discord] section to {} first", config_path.display());
        }
        return service::install(&config_path);
    }
    // The chat screen owns the terminal, so its logs go to a file next to the database.
    let writer = if command.as_deref() == Some("serve") {
        BoxMakeWriter::new(std::io::stderr)
    } else {
        let path = config.database_path.with_extension("log");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let file = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open log file {}", path.display()))?;
        BoxMakeWriter::new(std::sync::Mutex::new(file))
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.log_level).context("invalid log.level in config")?,
        )
        .with_writer(writer)
        .with_ansi(command.as_deref() == Some("serve") && std::io::stderr().is_terminal())
        .init();

    if command.as_deref() == Some("serve") {
        return discord::serve(config).await;
    }
    chat(config).await
}

async fn chat(config: config::Config) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("mitten chat needs an interactive terminal");
    }
    let db = db::Db::open(&config.database_path)?;
    let agent = agent::Agent::new(config, db, "terminal", "terminal").await?;
    chat::run(agent).await
}
