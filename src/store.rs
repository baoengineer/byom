//! On-disk state: `~/.byoclaude` (or `$BYOCLAUDE_HOME`) and its owner-only files.
//!
//! ```text
//! config.json   settings            auth.json   credentials, keyed by provider
//! bridge.key    local bridge key    host-id     stable Sign in with ChatGPT host ID
//! cache/        model catalogs      logs/       bridge.log, bridge.err
//! ```
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Map, Value};

const MAX_PRIVATE_BYTES: u64 = 1024 * 1024;

pub fn home() -> Result<PathBuf> {
    crate::config::state_dir()
}

pub fn config_path() -> Result<PathBuf> {
    Ok(home()?.join("config.json"))
}

pub fn cache_dir() -> Result<PathBuf> {
    Ok(home()?.join("cache"))
}

pub fn logs_dir() -> Result<PathBuf> {
    Ok(home()?.join("logs"))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(logs_dir()?.join("bridge.log"))
}

/// Create a directory readable only by the owner, rejecting symlinks and shared modes.
pub fn private_dir(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        bail!("{} must be a regular directory", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o022 != 0 {
            bail!("{} must not be writable by other users", path.display());
        }
    }
    Ok(())
}

/// Read an owner-only file. `Ok(None)` when it does not exist.
pub fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let before = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if !before.is_file() || before.file_type().is_symlink() {
        bail!("{} must be a regular file, not a symlink", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if before.permissions().mode() & 0o077 != 0 {
            bail!("{} must have owner-only permissions (0600)", path.display());
        }
    }
    let file = File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let after = file.metadata()?;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            bail!("{} changed while being read", path.display());
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_PRIVATE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PRIVATE_BYTES {
        bail!("{} exceeds the size limit", path.display());
    }
    Ok(Some(bytes))
}

/// Atomically replace an owner-only file.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent directory")?;
    private_dir(parent)?;
    let temp = parent.join(format!(".write-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

/// Exclusive lock serializing credential changes across processes.
pub async fn lock() -> Result<File> {
    let dir = home()?;
    private_dir(&dir)?;
    let path = dir.join("auth.lock");
    if let Ok(meta) = fs::symlink_metadata(&path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        bail!("invalid lock file {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    tokio::task::spawn_blocking(move || -> Result<File> {
        file.lock_exclusive()?;
        Ok(file)
    })
    .await?
}

/// Credentials keyed by provider ID. Callers hold [`lock`] around read-modify-write.
pub mod auth {
    use super::*;

    fn path() -> Result<PathBuf> {
        Ok(home()?.join("auth.json"))
    }

    pub fn all() -> Result<Map<String, Value>> {
        match read_private(&path()?)? {
            None => Ok(Map::new()),
            Some(bytes) => match serde_json::from_slice(&bytes) {
                Ok(Value::Object(map)) => Ok(map),
                _ => bail!("auth.json is not a JSON object; fix or remove it and sign in again"),
            },
        }
    }

    pub fn get(provider: &str) -> Result<Option<Value>> {
        Ok(all()?.remove(provider))
    }

    pub fn set(provider: &str, value: Value) -> Result<()> {
        let mut map = all()?;
        map.insert(provider.to_owned(), value);
        write_private(&path()?, &serde_json::to_vec_pretty(&map)?)
    }

    pub fn remove(provider: &str) -> Result<bool> {
        let mut map = all()?;
        let existed = map.remove(provider).is_some();
        if existed {
            write_private(&path()?, &serde_json::to_vec_pretty(&map)?)?;
        }
        Ok(existed)
    }
}

/// Move a 0.1.0 state directory (`~/.byoclaude-rs`) into the current layout. Runs only when
/// `$BYOCLAUDE_HOME` is unset and the new home does not exist; the old directory is left as is.
pub fn migrate() -> Result<()> {
    if std::env::var_os("BYOCLAUDE_HOME").is_some() {
        return Ok(());
    }
    let new = home()?;
    let Some(user_home) = new.parent() else {
        return Ok(());
    };
    let old = user_home.join(".byoclaude-rs");
    if new.exists() || !old.is_dir() {
        return Ok(());
    }
    migrate_dir(&old, &new)
}

fn migrate_dir(old: &Path, new: &Path) -> Result<()> {
    private_dir(new)?;
    let copy = |name: &str, to: &Path| -> Result<()> {
        if let Some(bytes) = read_private(&old.join(name)).ok().flatten() {
            write_private(to, &bytes)?;
        }
        Ok(())
    };
    copy("bridge.key", &new.join("bridge.key"))?;
    copy("host-id", &new.join("host-id"))?;
    copy("models.json", &new.join("cache").join("models.json"))?;
    if let Some(bytes) = read_private(&old.join("chatgpt.json"))? {
        let session: Value = serde_json::from_slice(&bytes).context("reading old chatgpt.json")?;
        let mut map = Map::new();
        map.insert("openai".into(), session);
        write_private(&new.join("auth.json"), &serde_json::to_vec_pretty(&map)?)?;
    }
    if let Some(bytes) = read_private(&old.join("config.json")).ok().flatten()
        && let Ok(Value::Object(mut config)) = serde_json::from_slice::<Value>(&bytes)
    {
        if let Some(small) = config.remove("small_model") {
            config.insert("background".into(), small);
        }
        config.remove("provider");
        write_private(
            &new.join("config.json"),
            &serde_json::to_vec_pretty(&config)?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private(dir: &Path, name: &str, body: &str) {
        write_private(&dir.join(name), body.as_bytes()).unwrap();
    }

    #[test]
    fn migrates_old_layout_without_touching_it() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(".byoclaude-rs");
        let new = root.path().join(".byoclaude");
        private_dir(&old).unwrap();
        private(&old, "chatgpt.json", r#"{"access_token":"a"}"#);
        private(
            &old,
            "config.json",
            r#"{"model":"gpt-5.6-sol","small_model":"gpt-5.6-luna","provider":"openai"}"#,
        );
        private(&old, "models.json", "[]");
        private(&old, "bridge.key", &"a".repeat(64));
        migrate_dir(&old, &new).unwrap();
        let auth: Value =
            serde_json::from_slice(&read_private(&new.join("auth.json")).unwrap().unwrap())
                .unwrap();
        assert_eq!(auth["openai"]["access_token"], "a");
        let config: Value =
            serde_json::from_slice(&read_private(&new.join("config.json")).unwrap().unwrap())
                .unwrap();
        assert_eq!(
            config,
            serde_json::json!({"model": "gpt-5.6-sol", "background": "gpt-5.6-luna"})
        );
        assert!(new.join("cache/models.json").exists());
        assert!(new.join("bridge.key").exists());
        assert!(old.join("chatgpt.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn private_files_reject_shared_modes_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        assert!(read_private(&path).unwrap().is_none());
        write_private(&path, b"x").unwrap();
        assert_eq!(read_private(&path).unwrap().unwrap(), b"x");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&path).is_err());
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read_private(&link).is_err());
    }
}
