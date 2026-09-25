//! Installs `mitten serve` as a background service that starts on its own and restarts on exit:
//! a launchd agent on macOS, a systemd user unit on Linux.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const LABEL: &str = "dev.mitten.serve";
const UNIT: &str = "mitten.service";

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

fn plist_path(home: &Path) -> PathBuf {
    home.join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn launchd_install(exe: &Path, config_path: &Path, home: &Path) -> Result<()> {
    let log = home.join("Library/Logs/mitten.log");
    let plist = plist_path(home);

    let contents = render_plist(exe, config_path, home, &log);
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
    std::fs::write(&unit, render_unit(exe, config_path, home))
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

fn render_unit(exe: &Path, config: &Path, home: &Path) -> String {
    let home = home.to_string_lossy().replace('%', "%%");
    format!(
        r#"[Unit]
Description=Mitten agent (Discord)

[Service]
ExecStart={exe} serve --config {config}
WorkingDirectory=~
# The user manager's default PATH lacks cargo and ~/.local/bin, which MCP servers (npx, uvx) and the headless Chrome lookup need.
Environment="PATH={home}/.cargo/bin:{home}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
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

fn render_plist(exe: &Path, config: &Path, home: &Path, log: &Path) -> String {
    let [exe, config, home, log] =
        [exe, config, home, log].map(|p| xml_escape(&p.to_string_lossy()));
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
        <!-- launchd's default PATH lacks Homebrew and cargo, which MCP servers (npx, uvx), claude, and the headless Chrome lookup need. -->
        <key>PATH</key><string>{home}/.cargo/bin:{home}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
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
        );
        assert!(plist.contains("<string>serve</string>"));
        assert!(plist.contains("<string>/cfg/a&amp;b.toml</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(
            plist.contains("/home/.local/bin:"),
            "claude installs to ~/.local/bin"
        );
    }

    #[test]
    fn unit_runs_serve_with_quoted_paths() {
        let unit = render_unit(
            Path::new("/opt/my apps/mitten"),
            Path::new("/cfg/100%$x.toml"),
            Path::new("/home/me"),
        );
        assert!(
            unit.contains(r#"ExecStart="/opt/my apps/mitten" serve --config "/cfg/100%%$$x.toml""#),
            "{unit}"
        );
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("/home/me/.cargo/bin:"));
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
