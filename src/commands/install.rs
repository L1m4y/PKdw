use crate::util::expand_tilde;
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::PathBuf;

#[cfg_attr(not(unix), allow(unused_variables))]
pub async fn run(destination: PathBuf, force: bool, quiet: bool) -> Result<i32> {
    let destination_dir = expand_tilde(&destination);
    let source = std::env::current_exe().context("cannot locate the pkdw executable")?;
    let destination = destination_dir.join("pkdw");

    fs::create_dir_all(&destination_dir)
        .with_context(|| format!("cannot create {}", destination_dir.display()))?;

    if destination.exists() || destination.is_symlink() {
        if !force {
            return Err(anyhow!(
                "{} already exists; rerun with --force to replace it",
                destination.display()
            ));
        }
        match fs::remove_file(&destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove {}", destination.display()));
            }
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&source, &destination).with_context(|| {
            format!(
                "cannot create symlink {} -> {}",
                destination.display(),
                source.display()
            )
        })?;

        let on_path = is_on_path(&destination_dir);

        if !quiet {
            println!(
                "installed: {} -> {}",
                destination.display(),
                source.display()
            );
            if !on_path {
                println!(
                    "note: {} is not in PATH. Add it with:\n  export PATH=\"{}:$PATH\"",
                    destination_dir.display(),
                    destination_dir.display()
                );
            } else {
                println!("available as `pkdw`.");
            }
            println!(
                "symlink keeps downloads and .env in the source tree:\n  {}",
                source.parent().unwrap_or(&source).display()
            );
        }
        let _ = source;
        Ok(0)
    }

    #[cfg(not(unix))]
    {
        // Keep the build portable on systems without Unix symlink semantics.
        let _ = source;
        return Err(anyhow!(
            "pkdw install is currently only supported on Unix-like systems; use the binary directly"
        ));
    }
}

#[cfg(unix)]
fn is_on_path(dir: &std::path::Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|entry| {
        match (fs::canonicalize(&entry), fs::canonicalize(dir)) {
            (Ok(a), Ok(b)) => a == b,
            _ => entry == dir,
        }
    })
}
