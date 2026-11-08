use crate::api::{drive, PikPak};
use crate::cli::{LsArgs, OutputArgs};
use crate::ui;
use anyhow::Result;
use serde_json::json;

pub async fn run(client: &PikPak, args: &LsArgs, output: &OutputArgs) -> Result<i32> {
    let target = drive::resolve_path(client, &args.path).await?;

    if !target.is_folder() {
        if output.json {
            println!("{}", serde_json::to_string_pretty(&target)?);
        } else if !output.quiet {
            ui::drive_line(&target);
        }
        return Ok(0);
    }

    // Reuse the protected pagination used by drive::list_all. The manual
    // page-token loop here previously had no duplicate-token protection.
    let mut entries = drive::list_all(client, &target.id).await?;
    if entries.len() > usize::try_from(args.limit).unwrap_or(entries.len()) {
        entries.truncate(args.limit as usize);
    }

    entries.sort_by(|a, b| {
        b.is_folder()
            .cmp(&a.is_folder())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "path": args.path,
                "entries": entries,
            }))?
        );
        return Ok(0);
    }

    if !output.quiet {
        if entries.is_empty() {
            println!("(empty)");
        } else {
            for entry in &entries {
                ui::drive_line(entry);
            }
        }
    }
    Ok(0)
}
