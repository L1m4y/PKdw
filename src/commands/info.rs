use crate::api::{drive, share, PikPak};
use crate::cli::{InfoArgs, OutputArgs};
use crate::ui;
use crate::util::{format_size, parse_target, Target};
use anyhow::Result;
use serde_json::json;

pub async fn run(client: &PikPak, args: &InfoArgs, output: &OutputArgs) -> Result<i32> {
    let target = parse_target(&args.target, args.pass_code.clone())?;
    match target {
        Target::Share {
            share_id,
            file_id,
            pass_code,
        } => {
            let pass_code = pass_code.as_deref();
            let token = share::resolve_pass_code_token(client, &share_id, pass_code).await?;

            if let Some(file_id) = file_id {
                let info = share::file_info(client, &share_id, &file_id, &token).await?;
                if info.is_folder() {
                    let files = share::collect_share_at(
                        client,
                        &share_id,
                        &token,
                        &file_id,
                        !output.json && !output.quiet,
                        output.quiet,
                    )
                    .await?;
                    print_share_summary(&files, output)?;
                } else {
                    print_file_info(&info, output);
                }
                return Ok(0);
            }

            let files = share::collect_share(
                client,
                &share_id,
                &token,
                !output.json && !output.quiet,
                output.quiet,
            )
            .await?;
            print_share_summary(&files, output)?;
            Ok(0)
        }
        Target::Drive(path) => {
            let entry = drive::resolve_path(client, &path).await?;
            if entry.is_folder() {
                let entries = drive::list_all(client, &entry.id).await?;
                if output.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "path": path,
                            "entries": entries,
                        }))?
                    );
                } else if !output.quiet {
                    if entries.is_empty() {
                        println!("(empty folder)");
                    } else {
                        for entry in entries {
                            ui::drive_line(&entry);
                        }
                    }
                }
            } else {
                let info = drive::file_info(client, &entry.id).await?;
                print_file_info(&info, output);
            }
            Ok(0)
        }
    }
}

fn print_share_summary(files: &share::ShareFiles, output: &OutputArgs) -> Result<()> {
    let total: u64 = files.entries.iter().map(|entry| entry.size).sum();
    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "files": files.entries,
                "total_size": total,
            }))?
        );
        return Ok(());
    }
    if !output.quiet {
        println!("{} file(s), {}", files.entries.len(), format_size(total));
    }
    Ok(())
}

fn print_file_info(info: &crate::api::models::FileInfo, output: &OutputArgs) {
    if output.json {
        let qualities = info.available_qualities();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "id": info.id,
                "name": info.name,
                "size": info.size,
                "hash": info.hash,
                "qualities": qualities,
                "quota_blocked_original": info.quota_blocked_original(),
                "has_web_content_link": !info.web_content_link.is_empty(),
            }))
            .unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }
    if output.quiet {
        return;
    }
    ui::info_line("name", &info.name);
    ui::info_line("size", format_size(info.size));
    if !info.id.is_empty() {
        ui::info_line("id", &info.id);
    }
    if !info.hash.is_empty() {
        ui::info_line("hash", &info.hash);
    }
    let qualities = info.available_qualities();
    if !qualities.is_empty() {
        ui::info_line("streams", qualities.join(", "));
    }
    if info.quota_blocked_original() {
        println!("original stream needs more quota");
    }
    if !info.web_content_link.is_empty() {
        ui::info_line("direct link", "available");
    }
    if info.name.is_empty() {
        eprintln!("warning: the server returned a file without a name");
    }
}
