use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const KEY_REFRESH_TOKEN: &str = "PIKPAK_REFRESH_TOKEN";
pub const KEY_DEVICE_ID: &str = "PIKPAK_DEVICE_ID";
pub const KEY_PROXY: &str = "PIKPAK_PROXY";
pub const KEY_CLIENT_ID: &str = "PIKPAK_CLIENT_ID";
pub const KEY_CLIENT_SECRET: &str = "PIKPAK_CLIENT_SECRET";

/// A small, comment-preserving `.env` file stored next to the executable.
#[derive(Clone)]
pub struct ConfigFile {
    path: PathBuf,
    values: BTreeMap<String, String>,
}

impl fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigFile")
            .field("path", &self.path)
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ConfigFile {
    pub fn load(path: PathBuf) -> Result<Self> {
        let mut values = BTreeMap::new();
        match fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let Some((key, value)) = line.split_once('=') else {
                        continue;
                    };
                    let key = key.trim();
                    if key.is_empty() {
                        continue;
                    }
                    values.insert(key.to_string(), unquote(value.trim()).to_string());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration file {}", path.display()));
            }
        }
        Ok(Self { path, values })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    /// File first, then the process environment. This lets CI or a wrapper
    /// provide a token without writing it to disk.
    pub fn get_with_env(&self, key: &str) -> Option<String> {
        self.get(key)
            .map(str::to_string)
            .or_else(|| std::env::var(key).ok().filter(|value| !value.is_empty()))
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.values.insert(key.to_string(), value.to_string());
    }

    pub fn remove(&mut self, key: &str) {
        self.values.remove(key);
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn save(&self) -> Result<()> {
        let existing = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot read configuration file {}", self.path.display())
                });
            }
        };
        let mut output: Vec<String> = Vec::new();
        let mut written = BTreeMap::<String, bool>::new();

        for line in existing.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                output.push(line.to_string());
                continue;
            }
            let Some((key, _)) = trimmed.split_once('=') else {
                output.push(line.to_string());
                continue;
            };
            let key = key.trim();
            match self.values.get(key) {
                Some(value) if !value.is_empty() => {
                    output.push(format!("{key}={value}"));
                    written.insert(key.to_string(), true);
                }
                _ => {
                    // Keep the key as an empty assignment instead of deleting
                    // user comments that may document it.
                    output.push(format!("{key}="));
                    written.insert(key.to_string(), true);
                }
            }
        }

        for (key, value) in &self.values {
            if written.get(key).copied().unwrap_or(false) {
                continue;
            }
            if value.is_empty() {
                output.push(format!("{key}="));
            } else {
                output.push(format!("{key}={value}"));
            }
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create config directory {}", parent.display()))?;
        }
        crate::fsutil::atomic_write(&self.path, format!("{}\n", output.join("\n")).as_bytes())
    }
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comments_empty_values_and_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# comment\nA=1\nB=\"two words\"\nC=\nsingle='x'\n").unwrap();
        let config = ConfigFile::load(path).unwrap();
        assert_eq!(config.get("A"), Some("1"));
        assert_eq!(config.get("B"), Some("two words"));
        assert_eq!(config.get("C"), None);
        assert_eq!(config.get("single"), Some("x"));
    }

    #[test]
    fn preserves_comments_and_updates_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# keep me\nA=1\nB=2\n").unwrap();
        let mut config = ConfigFile::load(path.clone()).unwrap();
        config.set("A", "9");
        config.set("C", "3");
        config.save().unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("# keep me"));
        assert!(text.contains("A=9"));
        assert!(text.contains("B=2"));
        assert!(text.contains("C=3"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_private_and_leaves_no_temporary_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let mut config = ConfigFile::load(path.clone()).unwrap();
        config.set(KEY_REFRESH_TOKEN, "secret");
        config.save().unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_saves_do_not_share_a_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        fs::write(&path, "# keep\nA=1\n").unwrap();

        let mut handles = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut config = ConfigFile::load(path).unwrap();
                config.set("THREAD", &index.to_string());
                config.save().unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        let config = ConfigFile::load(path.clone()).unwrap();
        assert_eq!(config.get("A"), Some("1"));
        assert!(config.get("THREAD").is_some());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
