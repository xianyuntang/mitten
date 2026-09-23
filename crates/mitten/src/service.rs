//! Installs `mitten serve` as a macOS launchd agent that starts at login and restarts on exit.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const LABEL: &str = "dev.mitten.serve";

fn plist_path() -> Result<PathBuf> {
    Ok(std::env::home_dir()
        .context("cannot find home directory")?
        .join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

// ponytail: macOS launchd only; add a systemd --user unit when Linux matters.
pub fn install(config_path: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`mitten install` only supports macOS (launchd) for now");
    }
    let exe = std::env::current_exe().context("failed to locate the mitten binary")?;
    let home = std::env::home_dir().context("cannot find home directory")?;
    let log = home.join("Library/Logs/mitten.log");
    let plist = plist_path()?;

    let contents = render_plist(&exe, config_path, &home, &log);
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

pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;
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
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .context("failed to run launchctl")?;
    if !status.success() {
        bail!("launchctl {} failed: {status}", args.join(" "));
    }
    Ok(())
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
        <!-- launchd's default PATH lacks Homebrew and cargo, which bash tool commands expect. -->
        <key>PATH</key><string>{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
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
