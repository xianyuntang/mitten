//! Installs `mitten serve` as a background service that starts on its own and restarts on exit:
//! a launchd agent on macOS, a systemd user unit on Linux.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const LABEL: &str = "dev.mitten.serve";
const UNIT: &str = "mitten.service";
const INSTALL_SCRIPT: &str = "https://raw.githubusercontent.com/xianyuntang/mitten/main/install.sh";

pub fn install(config_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate the mitten binary")?;
    let home = std::env::home_dir().context("cannot find home directory")?;
    if cfg!(target_os = "macos") {
        launchd_install(&exe, config_path, &home)
    } else if cfg!(target_os = "linux") {
        systemd_install(&exe, config_path, &home)
    } else {
        bail!("`mitten install` supports macOS (launchd) and Linux (systemd)")
    }
}

pub fn uninstall() -> Result<()> {
    let home = std::env::home_dir().context("cannot find home directory")?;
    if cfg!(target_os = "macos") {
        launchd_uninstall(&home)
    } else if cfg!(target_os = "linux") {
        systemd_uninstall(&home)
    } else {
        bail!("`mitten uninstall` supports macOS (launchd) and Linux (systemd)")
    }
}

/// Reinstalls the latest release over this binary with install.sh, then restarts the background
/// service if one is installed so it picks up the new binary.
pub fn update() -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate the mitten binary")?;
    let dir = exe
        .parent()
        .context("the mitten binary has no parent directory")?;
    // Download first so a failed fetch fails here instead of piping nothing into sh.
    let script =
        format!("script=$(curl -fsSL {INSTALL_SCRIPT}) && printf '%s\\n' \"$script\" | sh");
    let status = Command::new("sh")
        .args(["-c", &script])
        .env("MITTEN_INSTALL_DIR", dir)
        .env("MITTEN_UPDATING", "1")
        .status()
        .context("failed to run sh")?;
    if !status.success() {
        bail!("update failed: {status}");
    }
    let home = std::env::home_dir().context("cannot find home directory")?;
    if cfg!(target_os = "macos") && plist_path(&home).exists() {
        let plist = plist_path(&home);
        let _ = launchctl(&["unload", path_str(&plist)?]);
        launchctl(&["load", "-w", path_str(&plist)?])?;
        println!("restarted the background service");
    } else if cfg!(target_os = "linux") && unit_path(&home).exists() {
        systemctl(&["restart", UNIT])?;
        println!("restarted the background service");
    }
    Ok(())
}

fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn launchd_install(exe: &Path, config_path: &Path, home: &Path) -> Result<()> {
    let log = home.join("Library/Logs/mitten.log");
    let plist = plist_path(home);

    let path = service_path(home, std::env::var("PATH").ok().as_deref(), LAUNCHD_PATH);
    let contents = render_plist(exe, config_path, home, &log, &path);
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    // Reinstalling: stop the old copy first; failure just means it wasn't loaded.
    let _ = launchctl(&["unload", path_str(&plist)?]);
    std::fs::write(&plist, contents)
        .with_context(|| format!("failed to write {}", plist.display()))?;
    launchctl(&["load", "-w", path_str(&plist)?])?;

    println!("installed {}\nlogs: {}", plist.display(), log.display());
    Ok(())
}

fn launchd_uninstall(home: &Path) -> Result<()> {
    let plist = plist_path(home);
    if !plist.exists() {
        println!("not installed");
        return Ok(());
    }
    let _ = launchctl(&["unload", path_str(&plist)?]);
    std::fs::remove_file(&plist)
        .with_context(|| format!("failed to remove {}", plist.display()))?;
    println!("removed {}", plist.display());
    Ok(())
}

fn launchctl(args: &[&str]) -> Result<()> {
    run("launchctl", args)
}

fn unit_path(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(|| home.join(".config"), PathBuf::from)
        .join("systemd/user")
        .join(UNIT)
}

fn systemd_install(exe: &Path, config_path: &Path, home: &Path) -> Result<()> {
    let unit = unit_path(home);
    if let Some(dir) = unit.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let path = service_path(home, std::env::var("PATH").ok().as_deref(), SYSTEMD_PATH);
    std::fs::write(&unit, render_unit(exe, config_path, &path))
        .with_context(|| format!("failed to write {}", unit.display()))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", UNIT])?;
    // Restart rather than start so a reinstall picks up the new binary and config.
    systemctl(&["restart", UNIT])?;
    // Without lingering, user services stop at logout and do not start at boot.
    if run("loginctl", &["enable-linger"]).is_err() {
        println!(
            "warning: could not enable lingering, so mitten stops when you log out.\n\
             fix: sudo loginctl enable-linger $USER"
        );
    }
    println!(
        "installed {}\nlogs: journalctl --user -u mitten -f",
        unit.display()
    );
    Ok(())
}

fn systemd_uninstall(home: &Path) -> Result<()> {
    let unit = unit_path(home);
    if !unit.exists() {
        println!("not installed");
        return Ok(());
    }
    // Failure just means it wasn't running; lingering stays on since other services may need it.
    let _ = systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(&unit).with_context(|| format!("failed to remove {}", unit.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    println!("removed {}", unit.display());
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    run("systemctl", &[&["--user"], args].concat())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed: {status}", args.join(" "));
    }
    Ok(())
}

/// Fallback directories on the service PATH; `~` is the home directory.
const LAUNCHD_PATH: &[&str] = &[
    "~/.cargo/bin",
    "~/.local/bin",
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
];
const SYSTEMD_PATH: &[&str] = &[
    "~/.cargo/bin",
    "~/.local/bin",
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// The PATH the service runs with: the installing shell's PATH first, so node, npx, and claude
/// resolve the same way as in the terminal (whatever manages them: asdf, nvm, Homebrew, …), then
/// `fallback`. Relative entries are dropped, since the service's working directory differs.
/// Like Hermes' gateway install, tools set up later need `mitten install` again.
fn service_path(home: &Path, shell: Option<&str>, fallback: &[&str]) -> String {
    let fallback = fallback.iter().map(|dir| match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => (*dir).to_owned(),
    });
    let mut dirs: Vec<String> = Vec::new();
    for dir in shell
        .unwrap_or_default()
        .split(':')
        .map(str::to_owned)
        .chain(fallback)
    {
        if Path::new(&dir).is_absolute() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.join(":")
}

fn render_unit(exe: &Path, config: &Path, path: &str) -> String {
    let path = path
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!(
        r#"[Unit]
Description=Mitten agent (Discord)

[Service]
ExecStart={exe} serve --config {config}
WorkingDirectory=~
# PATH from the shell that ran `mitten install`, so MCP servers (npx, uvx), claude, and the headless
# Chrome lookup resolve as in the terminal. Run `mitten install` again after installing new tools.
Environment="PATH={path}"
Restart=always
RestartSec=30

[Install]
WantedBy=default.target
"#,
        exe = systemd_quote(exe),
        config = systemd_quote(config),
    )
}

/// Quotes one ExecStart argument: escapes quotes, backslashes, and systemd's `%` and `$` expansions.
fn systemd_quote(path: &Path) -> String {
    let text = path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{text}\"")
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not UTF-8: {}", path.display()))
}

fn render_plist(exe: &Path, config: &Path, home: &Path, log: &Path, path: &str) -> String {
    let [exe, config, home, log] =
        [exe, config, home, log].map(|p| xml_escape(&p.to_string_lossy()));
    let path = xml_escape(path);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{config}</string>
    </array>
    <key>WorkingDirectory</key><string>{home}</string>
    <key>EnvironmentVariables</key>
    <dict>
        <!-- PATH from the shell that ran `mitten install`, so MCP servers (npx, uvx), claude, and the
             headless Chrome lookup resolve as in the terminal. Reinstall after installing new tools. -->
        <key>PATH</key><string>{path}</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ThrottleInterval</key><integer>30</integer>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_serve_with_escaped_paths() {
        let plist = render_plist(
            Path::new("/bin/mitten"),
            Path::new("/cfg/a&b.toml"),
            Path::new("/home"),
            Path::new("/log"),
            "/Users/me/.asdf/shims:/usr/bin",
        );
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>/cfg/a&amp;b.toml</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains("<string>/Users/me/.asdf/shims:/usr/bin</string>"));
    }

    #[test]
    fn unit_runs_serve_with_quoted_paths() {
        let unit = render_unit(
            Path::new("/opt/my apps/mitten"),
            Path::new("/cfg/100%$x.toml"),
            "/opt/100%/bin:/usr/bin",
        );
        assert!(
            unit.contains(r#"ExecStart="/opt/my apps/mitten" serve --config "/cfg/100%%$$x.toml""#),
            "{unit}"
        );
        assert!(unit.contains("Restart=always"));
        assert!(
            unit.contains(r#"Environment="PATH=/opt/100%%/bin:/usr/bin""#),
            "{unit}"
        );
    }

    #[test]
    fn service_path_puts_shell_first_then_fallback_without_duplicates() {
        let path = service_path(
            Path::new("/home/me"),
            Some("/home/me/.asdf/shims:.:bin:/usr/bin:/opt/homebrew/bin"),
            &["~/.local/bin", "/usr/bin"],
        );
        assert_eq!(
            path,
            "/home/me/.asdf/shims:/usr/bin:/opt/homebrew/bin:/home/me/.local/bin"
        );
        assert_eq!(
            service_path(Path::new("/h"), None, &["~/.local/bin"]),
            "/h/.local/bin"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_passes_plutil_lint() {
        use std::io::Write;
        let plist = render_plist(
            Path::new("/bin/mitten"),
            Path::new("/cfg.toml"),
            Path::new("/home"),
            Path::new("/log"),
            "/usr/bin:/bin",
        );
        let mut child = Command::new("plutil")
            .args(["-lint", "-"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("plutil runs");
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(plist.as_bytes())
            .expect("write plist");
        assert!(child.wait().expect("plutil exits").success());
    }
}
