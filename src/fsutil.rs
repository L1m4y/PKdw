use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// Advisory interprocess lock. The lock file is stable and is never removed
/// while another process might still hold it open.
pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create lock directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("cannot open lock file {}", path.display()))?;
        fs2::FileExt::lock_exclusive(&file)
            .with_context(|| format!("cannot lock {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

struct TempGuard {
    path: PathBuf,
    committed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Create a unique temporary file next to `dest`. On Unix the file is created
/// with mode 0600 in the same `open()` call, so there is no readable window.
pub fn create_unique_temp(dest: &Path) -> Result<(PathBuf, File)> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    for _ in 0..1000 {
        let candidate = parent.join(format!(
            ".{name}.tmp.{}.{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot create temporary file in {}", parent.display())
                })
            }
        }
    }

    bail!(
        "cannot create a unique temporary file next to {}",
        dest.display()
    )
}

/// Replace `dest` with a fully written temporary file.
pub fn replace_file(tmp: &Path, dest: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // std::fs::rename does not replace an existing destination on all
        // Windows filesystems. Prefer a real rename and fall back to a
        // non-atomic remove+rename only when the destination already exists.
        match std::fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                ) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot replace {} with {}", dest.display(), tmp.display())
                })
            }
        }
        match std::fs::remove_file(dest) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot remove old {}", dest.display()))
            }
        }
        std::fs::rename(tmp, dest)
            .with_context(|| format!("cannot move {} into place", tmp.display()))
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(tmp, dest)
            .with_context(|| format!("cannot move {} into place", tmp.display()))
    }
}

/// Persist the directory entry created by a rename on Unix.
pub fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let dir = File::open(parent)
            .with_context(|| format!("cannot open directory {}", parent.display()))?;
        dir.sync_all()
            .with_context(|| format!("cannot sync directory {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Write `bytes` to a unique temporary file, fsync it and atomically replace
/// `dest`. The temporary file is removed on every error path.
pub fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<()> {
    let (tmp, mut file) = create_unique_temp(dest)?;
    let mut guard = TempGuard {
        path: tmp.clone(),
        committed: false,
    };
    file.write_all(bytes)
        .with_context(|| format!("cannot write temporary file {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot sync temporary file {}", tmp.display()))?;
    drop(file);
    replace_file(&tmp, dest)?;
    guard.committed = true;
    sync_parent_dir(dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_files_are_private_from_creation() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("secret");
        let (path, file) = create_unique_temp(&dest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("config");
        atomic_write(&dest, b"one").unwrap();
        atomic_write(&dest, b"two").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn failed_replace_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("config");
        std::fs::create_dir(&dest).unwrap();
        assert!(atomic_write(&dest, b"secret").is_err());
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("config")]);
    }

    #[test]
    fn concurrent_writers_use_distinct_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("config");
        let mut handles = Vec::new();
        for index in 0..8 {
            let dest = dest.clone();
            handles.push(std::thread::spawn(move || {
                atomic_write(&dest, format!("value-{index}").as_bytes()).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let text = std::fs::read_to_string(&dest).unwrap();
        assert!(text.starts_with("value-"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
