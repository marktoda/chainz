//! Durable, serialized persistence for the config model.

use super::{DEFAULT_CONFIG_RELATIVE, LEGACY_CONFIG_FILE};
use anyhow::{Context, Result, anyhow};
use dirs::home_dir;
use fs2::FileExt;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub(super) struct ConfigLock(std::fs::File);

impl ConfigLock {
    pub(super) async fn acquire() -> Result<Self> {
        let path = get_config_path().ok_or(anyhow!("Unable to find config path"))?;
        if let Some(dir) = path.parent() {
            ensure_private_dir(dir).await?;
        }
        let lock_path = path.with_extension("json.lock");
        tokio::task::spawn_blocking(move || {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&lock_path).with_context(|| {
                format!("Failed to open config lock at {}", lock_path.display())
            })?;
            file.lock_exclusive()
                .with_context(|| format!("Failed to lock config at {}", lock_path.display()))?;
            Ok(Self(file))
        })
        .await
        .context("Config lock task failed")?
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Replace a config durably and atomically on every supported platform.
/// `tempfile::persist` maps to rename on Unix and replace-existing
/// `MoveFileExW` on Windows.
pub(super) fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("Config path has no parent directory"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".config.json.")
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create temporary config in {}", dir.display()))?;
    #[cfg(unix)]
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temp.write_all(contents)?;
    temp.as_file().sync_all()?;
    let persisted = temp.persist(path).map_err(|error| error.error)?;
    persisted.sync_all()?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

pub(super) async fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    #[cfg(unix)]
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}

pub(super) async fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    match tokio::fs::metadata(path).await {
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to restrict config permissions at {}",
                            path.display()
                        )
                    })?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(super) fn get_config_path() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir).join("chainz").join("config.json")),
        _ => Some(home_dir()?.join(DEFAULT_CONFIG_RELATIVE)),
    }
}

fn legacy_config_path() -> Option<PathBuf> {
    Some(home_dir()?.join(LEGACY_CONFIG_FILE))
}

pub(super) async fn migrate_legacy_config(new_path: &Path) -> Result<()> {
    let Some(legacy) = legacy_config_path() else {
        return Ok(());
    };
    migrate_legacy_config_from(&legacy, new_path).await
}

async fn migrate_legacy_config_from(legacy: &Path, new_path: &Path) -> Result<()> {
    match tokio::fs::metadata(new_path).await {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", new_path.display()));
        }
    }
    match tokio::fs::metadata(legacy).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", legacy.display()));
        }
    }
    if let Some(dir) = new_path.parent() {
        ensure_private_dir(dir)
            .await
            .with_context(|| format!("Failed to create config directory at {}", dir.display()))?;
    }
    tokio::fs::rename(legacy, new_path).await.with_context(|| {
        format!(
            "Failed to migrate config from {} to {}",
            legacy.display(),
            new_path.display()
        )
    })?;
    eprintln!(
        "Migrated config from {} to {}",
        legacy.display(),
        new_path.display()
    );
    Ok(())
}

pub(crate) fn config_exists() -> bool {
    get_config_path().map(|path| path.exists()).unwrap_or(false)
        || legacy_config_path()
            .map(|path| path.exists())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::migrate_legacy_config_from;
    use tempfile::TempDir;

    #[tokio::test]
    async fn migration_moves_an_explicit_legacy_path() {
        let home = TempDir::new().unwrap();
        let legacy = home.path().join(".chainz.json");
        let current = home.path().join("chainz").join("config.json");
        tokio::fs::write(&legacy, "legacy config").await.unwrap();

        migrate_legacy_config_from(&legacy, &current).await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&current).await.unwrap(),
            "legacy config"
        );
        assert!(!legacy.exists());
    }
}
