use crate::api::models::{DriveFile, FileInfo};
use crate::api::{drive, share, PikPak};
use crate::cli::{GetArgs, OutputArgs, Quality};
use crate::download::{
    download_file, CachedLink, DownloadOptions, DownloadOutcome, DriveLinkSource, LinkSource,
    ShareLinkSource,
};
use crate::ui;
use crate::util::{expand_tilde, format_size, parse_target, Target};
use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct Candidate {
    file_id: String,
    name: String,
    size: u64,
    info: Option<FileInfo>,
}

#[derive(Debug, Default)]
struct Report {
    complete: usize,
    incomplete: usize,
    failed: usize,
}

pub async fn run(client: &PikPak, args: &GetArgs, output: &OutputArgs) -> Result<i32> {
    let target = parse_target(&args.target, args.pass_code.clone())?;
    let out_dir = match &args.output {
        Some(path) => expand_tilde(path),
        None => client.paths().downloads_dir.clone(),
    };
    let options = DownloadOptions {
        http: client.http().clone(),
        connections: args.connections as usize,
        chunk_size: args.chunk_size * MIB,
        rounds: args.rounds,
        quiet: output.quiet,
        overwrite: args.overwrite,
    };

    match target {
        Target::Share {
            share_id,
            file_id,
            pass_code,
        } => {
            let token =
                share::resolve_pass_code_token(client, &share_id, pass_code.as_deref()).await?;
            let mut candidates =
                share_candidates(client, &share_id, &token, file_id.as_deref(), output).await?;
            apply_filters(&mut candidates, args)?;
            unique_names(&mut candidates);

            if candidates.is_empty() {
                bail!("no files matched the selected filters");
            }
            print_selection(&candidates, output);

            if args.dry_run {
                if output.json {
                    print_dry_run_json(&candidates)?;
                }
                return Ok(0);
            }

            out_dir_ready(&out_dir).await?;
            let report = if args.restore {
                download_restored(
                    client,
                    &share_id,
                    &token,
                    &candidates,
                    &out_dir,
                    &options,
                    args,
                    output,
                )
                .await?
            } else {
                download_shared(
                    client,
                    &share_id,
                    &token,
                    &candidates,
                    &out_dir,
                    &options,
                    args.quality,
                    output,
                )
                .await?
            };
            finish_report(&report, output)
        }
        Target::Drive(path) => {
            if args.restore {
                ui::warn("--restore only applies to share links; ignoring it for drive paths");
            }
            let entry = drive::resolve_path(client, &path).await?;
            let mut candidates = if entry.is_folder() {
                collect_drive_candidates(client, &entry.id).await?
            } else {
                vec![Candidate {
                    file_id: entry.id.clone(),
                    name: entry.name.clone(),
                    size: entry.size,
                    info: None,
                }]
            };
            apply_filters(&mut candidates, args)?;
            unique_names(&mut candidates);
            if candidates.is_empty() {
                bail!("no files matched the selected filters");
            }
            print_selection(&candidates, output);
            if args.dry_run {
                if output.json {
                    print_dry_run_json(&candidates)?;
                }
                return Ok(0);
            }
            if args.quality != Quality::Original {
                ui::warn("drive downloads always use the original file; --quality is ignored");
            }
            out_dir_ready(&out_dir).await?;
            let report = download_drive(client, &candidates, &out_dir, &options, output).await?;
            finish_report(&report, output)
        }
    }
}

async fn share_candidates(
    client: &PikPak,
    share_id: &str,
    token: &str,
    file_id: Option<&str>,
    output: &OutputArgs,
) -> Result<Vec<Candidate>> {
    if let Some(file_id) = file_id {
        let info = share::file_info(client, share_id, file_id, token).await?;
        if info.is_folder() {
            let files =
                share::collect_share_at(client, share_id, token, file_id, false, output.quiet)
                    .await?;
            return Ok(files
                .entries
                .into_iter()
                .map(|entry| Candidate {
                    file_id: entry.id,
                    name: entry.name,
                    size: entry.size,
                    info: None,
                })
                .collect());
        }
        return Ok(vec![Candidate {
            file_id: info.id.clone(),
            name: info.name.clone(),
            size: info.size,
            info: Some(info),
        }]);
    }

    let files = share::collect_share(client, share_id, token, false, output.quiet).await?;
    Ok(files
        .entries
        .into_iter()
        .map(|entry| Candidate {
            file_id: entry.id,
            name: entry.name,
            size: entry.size,
            info: None,
        })
        .collect())
}

fn apply_filters(candidates: &mut Vec<Candidate>, args: &GetArgs) -> Result<()> {
    if let Some(name) = &args.name {
        let needle = name.to_lowercase();
        candidates.retain(|candidate| candidate.name.to_lowercase().contains(&needle));
    }
    if args.min_size > 0 {
        let minimum = args.min_size.saturating_mul(MIB);
        candidates.retain(|candidate| candidate.size >= minimum);
    }
    if args.largest && !candidates.is_empty() {
        let largest = candidates
            .iter()
            .map(|candidate| candidate.size)
            .max()
            .unwrap_or(0);
        candidates.retain(|candidate| candidate.size == largest);
    }
    Ok(())
}

fn print_selection(candidates: &[Candidate], output: &OutputArgs) {
    if output.quiet || output.json {
        return;
    }
    println!("{} file(s) selected:", candidates.len());
    for candidate in candidates {
        println!("  {:>10}  {}", format_size(candidate.size), candidate.name);
    }
    println!();
}

fn print_dry_run_json(candidates: &[Candidate]) -> Result<()> {
    let files: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            json!({
                "id": candidate.file_id,
                "name": candidate.name,
                "size": candidate.size,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "files": files }))?
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn download_shared(
    client: &PikPak,
    share_id: &str,
    token: &str,
    candidates: &[Candidate],
    out_dir: &Path,
    options: &DownloadOptions,
    quality: Quality,
    output: &OutputArgs,
) -> Result<Report> {
    let mut report = Report::default();

    for (index, candidate) in candidates.iter().enumerate() {
        let info = match &candidate.info {
            Some(info) => info.clone(),
            None => match share::file_info(client, share_id, &candidate.file_id, token).await {
                Ok(info) => info,
                Err(error) => {
                    ui::warn(format!("{}: {error:#}", candidate.name));
                    report.failed += 1;
                    continue;
                }
            },
        };
        let name = if info.name.is_empty() {
            candidate.name.clone()
        } else {
            info.name.clone()
        };
        let size = info.size.max(candidate.size);

        if !output.quiet {
            println!(
                "[{}/{}] {} ({})",
                index + 1,
                candidates.len(),
                name,
                format_size(size)
            );
        }

        let inner: Arc<dyn LinkSource> = Arc::new(ShareLinkSource::new(
            client.clone(),
            share_id,
            candidate.file_id.clone(),
            token,
            quality,
            false,
        ));
        let source: Arc<dyn LinkSource> = CachedLink::new(inner).shared();
        let source_id = format!("share:{share_id}:{}", candidate.file_id);

        match download_file(source, &source_id, &name, size, out_dir, options).await {
            Ok(DownloadOutcome::Complete(_)) => report.complete += 1,
            Ok(DownloadOutcome::Incomplete { .. }) => report.incomplete += 1,
            Err(error) => {
                ui::warn(format!("{name}: {error:#}"));
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn download_restored(
    client: &PikPak,
    share_id: &str,
    token: &str,
    candidates: &[Candidate],
    out_dir: &Path,
    options: &DownloadOptions,
    args: &GetArgs,
    output: &OutputArgs,
) -> Result<Report> {
    let session = client.require_session().await?;
    let _ = session;
    let mut report = Report::default();

    for (index, candidate) in candidates.iter().enumerate() {
        let info = match &candidate.info {
            Some(info) => info.clone(),
            None => share::file_info(client, share_id, &candidate.file_id, token).await?,
        };
        let name = if info.name.is_empty() {
            candidate.name.clone()
        } else {
            info.name.clone()
        };
        let size = info.size.max(candidate.size);

        if !output.quiet {
            println!(
                "[{}/{}] {} ({}) - copy to drive",
                index + 1,
                candidates.len(),
                name,
                format_size(size)
            );
        }

        let about = drive::quota(client).await?;
        let free = about.quota.limit.saturating_sub(about.quota.usage);
        if free < size {
            ui::warn(format!(
                "not enough drive space for {name} ({} free, {} needed); using the share stream instead",
                format_size(free),
                format_size(size)
            ));
            let inner: Arc<dyn LinkSource> = Arc::new(ShareLinkSource::new(
                client.clone(),
                share_id,
                candidate.file_id.clone(),
                token,
                args.quality,
                false,
            ));
            let source: Arc<dyn LinkSource> = CachedLink::new(inner).shared();
            let source_id = format!("share:{share_id}:{}", candidate.file_id);
            match download_file(source, &source_id, &name, size, out_dir, options).await {
                Ok(DownloadOutcome::Complete(_)) => report.complete += 1,
                Ok(DownloadOutcome::Incomplete { .. }) => report.incomplete += 1,
                Err(error) => {
                    ui::warn(format!("{name}: {error:#}"));
                    report.failed += 1;
                }
            }
            continue;
        }

        let mut restored = false;
        let drive_file = match drive::find_by_name(client, &name, Some(size), 5).await? {
            Some(file) => Some(file),
            None => {
                let response = drive::restore_share(
                    client,
                    share_id,
                    token,
                    std::slice::from_ref(&candidate.file_id),
                )
                .await?;
                let task_id = if response.task_id.is_empty() {
                    response.id
                } else {
                    response.task_id
                };
                if !task_id.is_empty() {
                    drive::wait_task(client, &task_id, Duration::from_secs(30 * 60)).await?;
                }
                restored = true;
                wait_for_drive_file(client, &name, size, Duration::from_secs(120)).await?
            }
        };

        let Some(drive_file) = drive_file else {
            ui::warn(format!(
                "{name}: it did not appear in the drive after copying"
            ));
            report.failed += 1;
            continue;
        };

        let source: Arc<dyn LinkSource> = CachedLink::new(Arc::new(DriveLinkSource::new(
            client.clone(),
            drive_file.id.clone(),
        )))
        .shared();
        let result = download_file(
            source,
            &format!("drive:{}", drive_file.id),
            &name,
            size,
            out_dir,
            options,
        )
        .await;

        match result {
            Ok(DownloadOutcome::Complete(_)) => {
                report.complete += 1;
                if args.delete_after && restored {
                    match drive::delete_file(client, &drive_file.id).await {
                        Ok(()) => ui::status(output.quiet, format!("removed drive copy: {name}")),
                        Err(error) => ui::warn(format!("could not remove drive copy: {error:#}")),
                    }
                }
            }
            Ok(DownloadOutcome::Incomplete { .. }) => report.incomplete += 1,
            Err(error) => {
                ui::warn(format!("{name}: {error:#}"));
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

async fn wait_for_drive_file(
    client: &PikPak,
    name: &str,
    size: u64,
    timeout: Duration,
) -> Result<Option<DriveFile>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(file) = drive::find_by_name(client, name, Some(size), 5).await? {
            return Ok(Some(file));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

async fn collect_drive_candidates(client: &PikPak, folder_id: &str) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    let mut stack = vec![folder_id.to_string()];
    let mut visited = std::collections::HashSet::new();

    while let Some(parent) = stack.pop() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        for entry in drive::list_all(client, &parent).await? {
            if entry.is_folder() {
                stack.push(entry.id.clone());
            } else {
                out.push(Candidate {
                    file_id: entry.id,
                    name: entry.name,
                    size: entry.size,
                    info: None,
                });
            }
        }
    }
    Ok(out)
}

async fn download_drive(
    client: &PikPak,
    candidates: &[Candidate],
    out_dir: &Path,
    options: &DownloadOptions,
    output: &OutputArgs,
) -> Result<Report> {
    let mut report = Report::default();
    for (index, candidate) in candidates.iter().enumerate() {
        if !output.quiet {
            println!(
                "[{}/{}] {} ({})",
                index + 1,
                candidates.len(),
                candidate.name,
                format_size(candidate.size)
            );
        }
        let source: Arc<dyn LinkSource> = CachedLink::new(Arc::new(DriveLinkSource::new(
            client.clone(),
            candidate.file_id.clone(),
        )))
        .shared();
        match download_file(
            source,
            &format!("drive:{}", candidate.file_id),
            &candidate.name,
            candidate.size,
            out_dir,
            options,
        )
        .await
        {
            Ok(DownloadOutcome::Complete(_)) => report.complete += 1,
            Ok(DownloadOutcome::Incomplete { .. }) => report.incomplete += 1,
            Err(error) => {
                ui::warn(format!("{}: {error:#}", candidate.name));
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

async fn out_dir_ready(path: &Path) -> Result<()> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|error| anyhow!("cannot create {}: {error}", path.display()))
}

fn unique_names(candidates: &mut [Candidate]) {
    let mut used = std::collections::HashSet::new();
    for candidate in candidates {
        candidate.name = unique_name(&mut used, &candidate.name);
    }
}

fn unique_name(used: &mut std::collections::HashSet<String>, name: &str) -> String {
    let base = crate::util::sanitize_filename(name);
    if used.insert(base.clone()) {
        return base;
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) => (stem.to_string(), format!(".{ext}")),
        None => (base.clone(), String::new()),
    };
    for index in 2.. {
        let candidate = crate::util::sanitize_filename(&format!("{stem} ({index}){ext}"));
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    base
}

fn finish_report(report: &Report, output: &OutputArgs) -> Result<i32> {
    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "completed": report.complete,
                "incomplete": report.incomplete,
                "failed": report.failed,
            }))?
        );
    } else if !output.quiet {
        println!(
            "\nresult: {} complete, {} incomplete, {} failed",
            report.complete, report.incomplete, report.failed
        );
    }
    if report.failed > 0 {
        Ok(1)
    } else if report.incomplete > 0 {
        Ok(2)
    } else {
        Ok(0)
    }
}
