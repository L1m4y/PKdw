use crate::paths::Paths;
use crate::util::format_size;
use anyhow::{Context, Result};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub async fn run(output: &crate::cli::OutputArgs) -> Result<i32> {
    let paths = Paths::discover()?;
    let config = crate::config::ConfigFile::load(paths.env_file.clone())?;
    let mut failures = 0usize;
    let mut checks: Vec<(&str, String, bool)> = Vec::new();

    let downloads_ok = (|| -> Result<()> {
        fs::create_dir_all(&paths.downloads_dir)?;
        let probe = paths.downloads_dir.join(".pkdw-doctor-probe");
        fs::write(&probe, b"ok")?;
        fs::remove_file(&probe)?;
        Ok(())
    })()
    .is_ok();
    if !downloads_ok {
        failures += 1;
    }
    checks.push(("program dir", paths.program_dir.display().to_string(), true));
    checks.push((
        "download dir",
        paths.downloads_dir.display().to_string(),
        downloads_ok,
    ));

    if paths.env_file.exists() {
        let perms_ok = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                match fs::metadata(&paths.env_file) {
                    Ok(meta) => meta.permissions().mode() & 0o777 == 0o600,
                    Err(_) => false,
                }
            }
            #[cfg(not(unix))]
            {
                true
            }
        };
        if !perms_ok {
            failures += 1;
        }
        checks.push((
            "config file",
            format!(
                "{} (mode {})",
                paths.env_file.display(),
                if perms_ok { "0600" } else { "not 0600" }
            ),
            perms_ok,
        ));
    } else {
        checks.push((
            "config file",
            format!("{} (not created yet)", paths.env_file.display()),
            true,
        ));
    }

    let token_set = config
        .get_with_env(crate::config::KEY_REFRESH_TOKEN)
        .is_some_and(|value| !value.is_empty() && value != "your_refresh_token");
    checks.push((
        "refresh token",
        if token_set {
            "stored".to_string()
        } else {
            "not set (run `pkdw auth`)".to_string()
        },
        true,
    ));

    let device = config
        .get_with_env(crate::config::KEY_DEVICE_ID)
        .unwrap_or_else(|| "(not created yet)".to_string());
    checks.push(("device id", device, true));

    let proxy = config
        .get_with_env(crate::config::KEY_PROXY)
        .filter(|value| !value.trim().is_empty());
    checks.push((
        "proxy",
        if proxy.is_some() {
            "configured".to_string()
        } else {
            "direct / environment".to_string()
        },
        true,
    ));

    if let Ok(release) = paths
        .program_dir
        .join("target")
        .join("release")
        .join("pkdw")
        .metadata()
    {
        checks.push(("release binary", format_size(release.len()), true));
    } else {
        checks.push((
            "release binary",
            "not built (run `cargo build --release`)".to_string(),
            true,
        ));
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("cannot build the HTTP client")?;
    let api_ok = match client.get("https://api-drive.mypikpak.com/").send().await {
        Ok(response) => {
            checks.push((
                "PikPak API",
                format!("reachable (HTTP {})", response.status().as_u16()),
                true,
            ));
            true
        }
        Err(error) => {
            checks.push(("PikPak API", format!("unreachable: {error}"), false));
            false
        }
    };
    if !api_ok {
        failures += 1;
    }

    let bin_dir = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin");
    let bin_on_path = is_on_path(&bin_dir);
    checks.push((
        "install location",
        if bin_on_path {
            format!("{} is in PATH", bin_dir.display())
        } else {
            format!("{} is not in PATH", bin_dir.display())
        },
        true,
    ));

    let clipboard = cfg!(feature = "clipboard");
    checks.push((
        "clipboard support",
        if clipboard {
            "compiled in".to_string()
        } else {
            "not compiled (build with --features clipboard)".to_string()
        },
        true,
    ));

    let cargo_ok = Command::new("cargo").arg("--version").output().is_ok();
    checks.push((
        "Rust toolchain",
        if cargo_ok {
            "available (optional)".to_string()
        } else {
            "not available (optional)".to_string()
        },
        true,
    ));

    if output.json {
        let payload = json!({
            "paths": {
                "program_dir": paths.program_dir,
                "downloads_dir": paths.downloads_dir,
                "env_file": paths.env_file
            },
            "checks": checks
                .iter()
                .map(|(name, value, ok)| json!({"name": name, "value": value, "ok": ok}))
                .collect::<Vec<_>>(),
            "failures": failures
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for (name, value, ok) in &checks {
            println!(
                "{}  {:<20} {}",
                if *ok { "ok  " } else { "FAIL" },
                name,
                value
            );
        }
        if failures == 0 {
            println!("\ndoctor: all core checks passed");
        } else {
            println!("\ndoctor: {failures} check(s) failed");
        }
    }

    Ok(if failures == 0 { 0 } else { 1 })
}

fn is_on_path(dir: &Path) -> bool {
    if let Some(path_var) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path_var) {
            if canonicalize_equal(&entry, dir) {
                return true;
            }
        }
    }
    false
}

fn canonicalize_equal(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}
