use crate::cli::OutputArgs;
use crate::config::{ConfigFile, KEY_DEVICE_ID, KEY_PROXY, KEY_REFRESH_TOKEN};
use crate::paths::Paths;
use anyhow::Result;
use serde_json::json;

pub fn run(paths: &Paths, output: &OutputArgs) -> Result<i32> {
    let config = ConfigFile::load(paths.env_file.clone())?;
    let token = config
        .get_with_env(KEY_REFRESH_TOKEN)
        .is_some_and(|value| !value.is_empty() && value != "your_refresh_token");
    let device = config
        .get_with_env(KEY_DEVICE_ID)
        .unwrap_or_else(|| "(not created yet)".to_string());
    let proxy = config
        .get_with_env(KEY_PROXY)
        .filter(|value| !value.trim().is_empty());

    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "program_dir": paths.program_dir,
                "downloads_dir": paths.downloads_dir,
                "env_file": paths.env_file,
                "device_id": device,
                "refresh_token_stored": token,
                "proxy_configured": proxy.is_some(),
            }))?
        );
        return Ok(0);
    }

    if !output.quiet {
        println!("program dir    {}", paths.program_dir.display());
        println!("downloads      {}", paths.downloads_dir.display());
        println!("config file    {}", paths.env_file.display());
        println!("device id      {device}");
        println!(
            "refresh token  {}",
            if token {
                "stored"
            } else {
                "not set (run `pkdw auth`)"
            }
        );
        println!(
            "proxy          {}",
            if proxy.is_some() {
                "configured"
            } else {
                "direct / environment"
            }
        );
    }
    Ok(0)
}
