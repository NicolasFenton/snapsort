//! Where the SubQ API key comes from: `--api-key`, then `SUBQ_API_KEY`, then
//! a key saved by a previous run, then (in a terminal) a one-time prompt.

use std::{
    env, fmt, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const ENV_VAR: &str = "SUBQ_API_KEY";
/// Overrides the directory holding the saved key (used by tests).
const CONFIG_DIR_ENV: &str = "SNAPSORT_CONFIG_DIR";
const FILE_NAME: &str = "credentials.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    Flag,
    Env,
    /// Read from, or just written to, the saved credentials file.
    Saved(PathBuf),
    /// Typed at the prompt but could not be saved.
    Prompt,
}

impl KeySource {
    pub fn saved_path(&self) -> Option<&Path> {
        match self {
            KeySource::Saved(path) => Some(path),
            _ => None,
        }
    }
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeySource::Flag => f.write_str("--api-key"),
            KeySource::Env => write!(f, "the {ENV_VAR} environment variable"),
            KeySource::Saved(path) => write!(f, "the saved key in {}", path.display()),
            KeySource::Prompt => f.write_str("the prompt"),
        }
    }
}

#[derive(Debug)]
pub struct ApiKey {
    pub value: String,
    pub source: KeySource,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    api_key: String,
}

/// The key given on the command line or via `SUBQ_API_KEY` (clap merges
/// both into one value; comparing with the environment tells them apart).
pub fn from_args(value: Option<&str>) -> Option<ApiKey> {
    let value = value.map(str::trim).filter(|v| !v.is_empty())?;
    let from_env = env::var(ENV_VAR).is_ok_and(|e| e.trim() == value);
    Some(ApiKey {
        value: value.to_owned(),
        source: if from_env {
            KeySource::Env
        } else {
            KeySource::Flag
        },
    })
}

/// `~/.config/snapsort/credentials.json` (or `$XDG_CONFIG_HOME/...`) on
/// macOS/Linux, `%APPDATA%\snapsort\credentials.json` on Windows.
pub fn saved_key_path() -> Option<PathBuf> {
    let non_empty = |name: &str| {
        env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let dir = if let Some(dir) = non_empty(CONFIG_DIR_ENV) {
        dir
    } else if cfg!(windows) {
        non_empty("APPDATA")?.join("snapsort")
    } else {
        non_empty("XDG_CONFIG_HOME")
            .or_else(|| non_empty("HOME").map(|home| home.join(".config")))?
            .join("snapsort")
    };
    Some(dir.join(FILE_NAME))
}

pub fn load_saved() -> Result<Option<ApiKey>> {
    match saved_key_path() {
        Some(path) => load_from(&path),
        None => Ok(None),
    }
}

pub fn load_from(path: &Path) -> Result<Option<ApiKey>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let stored: Stored = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not a valid snapsort key file; delete it to be asked again",
            path.display()
        )
    })?;
    let value = stored.api_key.trim().to_owned();
    Ok((!value.is_empty()).then(|| ApiKey {
        value,
        source: KeySource::Saved(path.to_path_buf()),
    }))
}

/// Save the key readable only by the current user (0600 inside a 0700
/// folder on Unix; `%APPDATA%` is already per-user on Windows). Written to a
/// temp file and renamed so a crash never leaves a half-written key.
pub fn save_to(path: &Path, key: &str) -> Result<()> {
    let dir = path
        .parent()
        .context("credentials path has no parent folder")?;
    create_private_dir(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(&Stored {
        api_key: key.to_owned(),
    })?;
    text.push('\n');
    let temp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let result = write_private(&temp, text.as_bytes()).and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("failed to save the API key to {}", path.display()))
}

pub fn forget(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Prompting needs a person at a terminal; scripts and CI must not hang.
pub fn can_prompt() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

/// Ask for the key with hidden input. `None` if nothing was entered.
pub fn prompt() -> Result<Option<String>> {
    let mut stderr = io::stderr();
    writeln!(
        stderr,
        "No SubQ API key found. Get one at https://console.subq.ai"
    )?;
    let key = rpassword::prompt_password("Paste your SubQ API key (input is hidden): ")
        .context("couldn't read the API key from the terminal; set SUBQ_API_KEY instead")?;
    let key = key.trim().to_owned();
    Ok((!key.is_empty()).then_some(key))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Set at creation so the key is never briefly world-readable.
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/snapsort/credentials.json");
        assert!(load_from(&path).unwrap().is_none());

        save_to(&path, "sk-first").unwrap();
        save_to(&path, "sk-second").unwrap();
        let key = load_from(&path).unwrap().unwrap();
        assert_eq!(key.value, "sk-second");
        assert_eq!(key.source, KeySource::Saved(path.clone()));

        forget(&path).unwrap();
        forget(&path).unwrap();
        assert!(load_from(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_key_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapsort/credentials.json");
        save_to(&path, "sk-secret").unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn rejects_corrupt_files_and_ignores_blank_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        fs::write(&path, "not json").unwrap();
        assert!(load_from(&path).is_err());
        fs::write(&path, r#"{"api_key": "  "}"#).unwrap();
        assert!(load_from(&path).unwrap().is_none());
    }
}
