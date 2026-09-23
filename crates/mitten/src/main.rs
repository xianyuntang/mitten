mod agent;
mod config;
mod db;
mod discord;
mod onboarding;
mod service;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, Stdin};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: mitten [chat|serve|configure|install|uninstall] [--config PATH]

  chat       talk in this terminal (default)
  configure  interactive setup: API key, model, Discord
  serve      run the Discord bot from [discord]
  install    run `mitten serve` at login and keep it alive (macOS launchd)
  uninstall  remove the launchd service";

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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.log_level).context("invalid log.level in config")?,
        )
        .with_writer(std::io::stderr)
        .init();

    if command.as_deref() == Some("serve") {
        return discord::serve(config).await;
    }
    chat(config).await
}

async fn chat(config: config::Config) -> Result<()> {
    let db = db::Db::open(&config.database_path)?;
    let mut agent = agent::Agent::new(config, db, "terminal").await?;
    println!("mitten ({})", agent.describe());
    let mut terminal = Terminal {
        input: BufReader::new(tokio::io::stdin()).lines(),
    };
    let mut stdout = tokio::io::stdout();

    loop {
        stdout.write_all(b"\n> ").await?;
        stdout.flush().await?;
        let Some(line) = terminal.input.next_line().await? else {
            break;
        };
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if matches!(prompt, "/exit" | "/quit") {
            break;
        }
        if prompt == "/new" {
            agent.reset().await?;
            println!("started a new conversation");
            continue;
        }
        if let Err(err) = agent.run_turn(prompt, &mut terminal).await {
            eprintln!("error: {err:#}");
        }
    }
    Ok(())
}

struct Terminal {
    input: Lines<BufReader<Stdin>>,
}

impl agent::Io for Terminal {
    async fn say(&mut self, text: &str) -> Result<()> {
        println!("{text}");
        Ok(())
    }

    async fn confirm(&mut self, command: &str) -> Result<bool> {
        println!("\n$ {command}\nrun? [y/N] ");
        let answer = self
            .input
            .next_line()
            .await
            .context("failed to read confirmation")?
            .unwrap_or_default();
        Ok(answer.trim().eq_ignore_ascii_case("y"))
    }
}
