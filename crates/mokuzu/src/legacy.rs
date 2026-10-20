//! Moves an install made under the project's old name, mitten, over to mokuzu: its config and
//! data move to mokuzu's paths, and its background service is replaced by mokuzu's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::service;

/// Moves whatever of mitten's is left, and returns `config_path` with mitten's config directory
/// swapped for mokuzu's. `serving` is true under `serve`, which mitten's service runs once
/// install.sh points the `mitten` name at mokuzu.
pub fn migrate(config_path: PathBuf, serving: bool) -> Result<PathBuf> {
    let home = std::env::home_dir().context("cannot find home directory")?;
    let legacy_service = service::legacy_installed(&home);
    if legacy_service {
        // Gone before mokuzu's service starts, so that one finds nothing left to replace.
        service::remove_legacy(&home)?;
        // Stopped before its database moves, unless this process is that service; then it
        // stops once mokuzu's is up, below.
        if !serving {
            service::stop_legacy();
        }
    }
    if pending(&home) {
        move_data(&home)?;
        println!("moved mitten's config and data to mokuzu's paths");
    }
    let config_path = match config_path.strip_prefix(home.join(CONFIG.0)) {
        Ok(rest) => home.join(CONFIG.1).join(rest),
        Err(_) => config_path,
    };
    if legacy_service {
        service::install(&config_path)?;
        if serving {
            service::stop_legacy();
        }
        println!("replaced mitten's background service with mokuzu's");
    }
    Ok(config_path)
}

fn pending(home: &Path) -> bool {
    [CONFIG, DATA]
        .iter()
        .any(|(old, new)| home.join(old).is_dir() && !home.join(new).exists())
}

const CONFIG: (&str, &str) = (".config/mitten", ".config/mokuzu");
const DATA: (&str, &str) = (".local/share/mitten", ".local/share/mokuzu");

fn move_data(home: &Path) -> Result<()> {
    for (old, new) in [CONFIG, DATA] {
        let (old, new) = (home.join(old), home.join(new));
        if old.is_dir() && !new.exists() {
            std::fs::rename(&old, &new).with_context(|| {
                format!("failed to move {} to {}", old.display(), new.display())
            })?;
        }
    }
    let data = home.join(DATA.1);
    for ext in ["db", "db-wal", "db-shm", "log"] {
        let from = data.join(format!("mitten.{ext}"));
        if from.exists() {
            let to = data.join(format!("mokuzu.{ext}"));
            std::fs::rename(&from, &to).with_context(|| {
                format!("failed to move {} to {}", from.display(), to.display())
            })?;
        }
    }
    // `configure` writes the database path into the config; point it at the moved file.
    let config = home.join(CONFIG.1).join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&config) {
        let old = home.join(DATA.0).join("mitten.db");
        let old = old.to_string_lossy();
        if text.contains(old.as_ref()) {
            let new = data.join("mokuzu.db");
            std::fs::write(&config, text.replace(old.as_ref(), &new.to_string_lossy()))
                .with_context(|| format!("failed to update {}", config.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_config_database_and_its_path() {
        let home = std::env::temp_dir().join(format!("mokuzu-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let old_db = home.join(".local/share/mitten/mitten.db");
        std::fs::create_dir_all(home.join(".config/mitten")).expect("config dir");
        std::fs::create_dir_all(old_db.parent().expect("parent")).expect("data dir");
        std::fs::write(&old_db, "db").expect("db");
        std::fs::write(
            home.join(".config/mitten/config.toml"),
            format!("[database]\npath = \"{}\"\n", old_db.display()),
        )
        .expect("config");

        assert!(pending(&home));
        move_data(&home).expect("moves");
        assert!(!pending(&home));
        let new_db = home.join(".local/share/mokuzu/mokuzu.db");
        assert_eq!(std::fs::read_to_string(&new_db).expect("db moved"), "db");
        let config =
            std::fs::read_to_string(home.join(".config/mokuzu/config.toml")).expect("config moved");
        assert!(config.contains(&new_db.display().to_string()), "{config}");
        std::fs::remove_dir_all(&home).expect("clean up");
    }
}
