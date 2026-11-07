use anyhow::{Context, Result};
use std::env;
use std::path::{Path, PathBuf};

/// All runtime paths are derived from the executable location so the folder can
/// be moved or renamed without editing configuration.
#[derive(Debug, Clone)]
pub struct Paths {
    pub program_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub env_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let program_dir = match env::var_os("PKDW_HOME").filter(|v| !v.is_empty()) {
            Some(value) => absolute(PathBuf::from(value))?,
            None => discover_program_dir()?,
        };

        let downloads_dir = match env::var_os("PKDW_DOWNLOADS").filter(|v| !v.is_empty()) {
            Some(value) => absolute(PathBuf::from(value))?,
            None => program_dir.join("downloads"),
        };

        let env_file = match env::var_os("PKDW_ENV_FILE").filter(|v| !v.is_empty()) {
            Some(value) => absolute(PathBuf::from(value))?,
            None => program_dir.join(".env"),
        };

        Ok(Self {
            program_dir,
            downloads_dir,
            env_file,
        })
    }

    pub fn ensure_downloads(&self) -> Result<()> {
        std::fs::create_dir_all(&self.downloads_dir).with_context(|| {
            format!(
                "cannot create download directory {}",
                self.downloads_dir.display()
            )
        })
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = env::current_dir().context("cannot read the current directory")?;
    Ok(cwd.join(path))
}

fn discover_program_dir() -> Result<PathBuf> {
    let exe = env::current_exe().context("cannot locate the pkdw executable")?;
    let mut current = exe
        .parent()
        .map(Path::to_path_buf)
        .context("the pkdw executable has no parent directory")?;

    // Development builds live under <project>/target/{debug,release}[/deps].
    // Walk up a few levels and stop at the Cargo project root, or just above
    // the target directory. Installed binaries fall back to their own folder.
    for _ in 0..5 {
        if is_this_project(&current) {
            return Ok(current);
        }
        if current.file_name().is_some_and(|name| name == "target") {
            if let Some(parent) = current.parent() {
                return Ok(parent.to_path_buf());
            }
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }

    exe.parent()
        .map(Path::to_path_buf)
        .context("the pkdw executable has no parent directory")
}

fn is_this_project(dir: &Path) -> bool {
    let manifest = dir.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(manifest) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim();
        line == "name = \"pkdw\"" || line == "name = 'pkdw'"
    })
}
