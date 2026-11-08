use crate::api::models::{FileInfo, ResolvedMedia};
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
    size: Option<u64>,
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
            unique_names(&mut candidates)?;

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
            unique_names(&mut candidates)?;
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
        candidates.retain(|candidate| candidate.size.unwrap_or(0) >= minimum);
    }
    if args.largest && !candidates.is_empty() {
        let largest = candidates
            .iter()
            .map(|candidate| candidate.size.unwrap_or(0))
            .max()
            .unwrap_or(0);
        candidates.retain(|candidate| candidate.size.unwrap_or(0) == largest);
    }
    Ok(())
}

fn print_selection(candidates: &[Candidate], output: &OutputArgs) {
    if output.quiet || output.json {
        return;
    }
    println!("{} file(s) selected:", candidates.len());
    for candidate in candidates {
        println!(
            "  {:>10}  {}",
            format_size(candidate.size.unwrap_or(0)),
            candidate.name
        );
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
        let Some(mut selected) = info.select_media(quality) else {
            if info.quota_blocked_original() && quality == Quality::Original {
                ui::warn(format!(
                    "{}: original stream needs more quota; use --quality 1080p",
                    name
                ));
            } else {
                ui::warn(format!(
                    "{}: no downloadable stream for {}",
                    name,
                    quality.key()
                ));
            }
            report.failed += 1;
            continue;
        };
        if selected.is_original && selected.expected_size.is_none() {
            selected.expected_size = info.size.or(candidate.size);
        }
        if selected.label.to_ascii_lowercase() != quality.key() {
            ui::warn(format!(
                "{} is not available for {}; using {}",
                quality.key(),
                name,
                selected.label
            ));
        }

        if !output.quiet {
            println!(
                "[{}/{}] {} ({}) [{}]",
                index + 1,
                candidates.len(),
                name,
                selected
                    .expected_size
                    .map(format_size)
                    .unwrap_or_else(|| "unknown size".to_string()),
                selected.label
            );
        }

        let inner: Arc<dyn LinkSource> = Arc::new(ShareLinkSource::new(
            client.clone(),
            share_id,
            candidate.file_id.clone(),
            token,
            false,
        ));
        let source: Arc<dyn LinkSource> = CachedLink::new(inner).shared();
        let source_id = format!("share:{share_id}:{}", candidate.file_id);

        match download_file(source, &source_id, &name, &selected, out_dir, options).await {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OneFileOutcome {
    Complete,
    Incomplete,
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
    // Fail before starting the batch when the session itself is unusable.
    let _ = client.require_session().await?;
    let mut report = Report::default();

    for (index, candidate) in candidates.iter().enumerate() {
        match download_one_restored(
            client,
            share_id,
            token,
            candidate,
            index,
            candidates.len(),
            out_dir,
            options,
            args,
            output,
        )
        .await
        {
            Ok(OneFileOutcome::Complete) => report.complete += 1,
            Ok(OneFileOutcome::Incomplete) => report.incomplete += 1,
            Err(error) if is_fatal_auth_error(&error) => return Err(error),
            Err(error) => {
                ui::warn(format!("{}: {error:#}", candidate.name));
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn download_one_restored(
    client: &PikPak,
    share_id: &str,
    token: &str,
    candidate: &Candidate,
    index: usize,
    total: usize,
    out_dir: &Path,
    options: &DownloadOptions,
    args: &GetArgs,
    output: &OutputArgs,
) -> Result<OneFileOutcome> {
    let mut info = match &candidate.info {
        Some(info) => info.clone(),
        None => share::file_info(client, share_id, &candidate.file_id, token).await?,
    };
    let name = if info.name.is_empty() {
        candidate.name.clone()
    } else {
        info.name.clone()
    };

    let Some(size) = resolve_restore_size(client, share_id, token, &mut info, candidate).await?
    else {
        ui::warn(format!(
            "{name}: size is unknown; cannot validate drive quota, using direct download"
        ));
        return download_share_stream(
            client, share_id, token, candidate, &info, &name, None, out_dir, options, args,
        )
        .await;
    };

    if !output.quiet {
        println!(
            "[{}/{}] {} ({}) - copy to drive",
            index + 1,
            total,
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
        return download_share_stream(
            client,
            share_id,
            token,
            candidate,
            &info,
            &name,
            Some(size),
            out_dir,
            options,
            args,
        )
        .await;
    }

    // Snapshot IDs before the restore. Name and size alone are not proof of
    // identity: a pre-existing homonym must never be downloaded or deleted.
    let before_ids = drive::snapshot_ids(client).await?;
    let response = drive::restore_share(
        client,
        share_id,
        token,
        std::slice::from_ref(&candidate.file_id),
    )
    .await?;

    if !response.task_id.is_empty() {
        // Only a field explicitly named `task_id` is polled. Unknown fields
        // are preserved in TaskResponse.extra but not guessed.
        drive::wait_task(client, &response.task_id, Duration::from_secs(30 * 60)).await?;
    } else if !response.id.is_empty() && !before_ids.contains(&response.id) {
        // `id` is not assumed to be a task id. It is only used when it can be
        // validated as the restored drive file.
        if let Ok(candidate_info) = drive::file_info(client, &response.id).await {
            if candidate_info.name == name
                && candidate_info.size.is_none_or(|actual| actual == size)
            {
                return finish_restored_download(
                    client,
                    &name,
                    size,
                    &response.id,
                    out_dir,
                    options,
                    args,
                    output,
                    true,
                )
                .await;
            }
        }
    }

    let created_id = wait_for_new_drive_file(client, &name, size, &before_ids).await?;
    finish_restored_download(
        client,
        &name,
        size,
        &created_id,
        out_dir,
        options,
        args,
        output,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_restored_download(
    client: &PikPak,
    name: &str,
    size: u64,
    drive_file_id: &str,
    out_dir: &Path,
    options: &DownloadOptions,
    args: &GetArgs,
    output: &OutputArgs,
    created_by_this_restore: bool,
) -> Result<OneFileOutcome> {
    let source: Arc<dyn LinkSource> = CachedLink::new(Arc::new(DriveLinkSource::new(
        client.clone(),
        drive_file_id,
    )))
    .shared();
    let media = ResolvedMedia::drive(Some(size));
    let result = download_file(
        source,
        &format!("drive:{drive_file_id}"),
        name,
        &media,
        out_dir,
        options,
    )
    .await;

    match result {
        Ok(DownloadOutcome::Complete(_)) => {
            // `--delete-after` is fail-closed: only an ID proven to be the
            // result of this restore can be removed, and only after the local
            // file has been verified.
            if args.delete_after {
                if !created_by_this_restore {
                    ui::warn(format!(
                        "not removing the drive copy of {name}: its identity was not proven"
                    ));
                } else {
                    match drive::delete_file(client, drive_file_id).await {
                        Ok(()) => ui::status(output.quiet, format!("removed drive copy: {name}")),
                        Err(error) => ui::warn(format!("could not remove drive copy: {error:#}")),
                    }
                }
            }
            Ok(OneFileOutcome::Complete)
        }
        Ok(DownloadOutcome::Incomplete { .. }) => Ok(OneFileOutcome::Incomplete),
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_share_stream(
    client: &PikPak,
    share_id: &str,
    token: &str,
    candidate: &Candidate,
    info: &FileInfo,
    name: &str,
    fallback_size: Option<u64>,
    out_dir: &Path,
    options: &DownloadOptions,
    args: &GetArgs,
) -> Result<OneFileOutcome> {
    let Some(mut selected) = info.select_media(args.quality) else {
        if info.quota_blocked_original() && args.quality == Quality::Original {
            bail!(
                "{name}: the original stream needs more quota; use --quality 1080p or free drive space"
            );
        }
        bail!("{name}: no downloadable stream for {}", args.quality.key());
    };
    if selected.is_original && selected.expected_size.is_none() {
        selected.expected_size = fallback_size;
    }
    let source: Arc<dyn LinkSource> = CachedLink::new(Arc::new(ShareLinkSource::new(
        client.clone(),
        share_id,
        candidate.file_id.clone(),
        token,
        false,
    )))
    .shared();
    let source_id = format!("share:{share_id}:{}", candidate.file_id);
    match download_file(source, &source_id, name, &selected, out_dir, options).await? {
        DownloadOutcome::Complete(_) => Ok(OneFileOutcome::Complete),
        DownloadOutcome::Incomplete { .. } => Ok(OneFileOutcome::Incomplete),
    }
}

async fn resolve_restore_size(
    client: &PikPak,
    share_id: &str,
    token: &str,
    info: &mut FileInfo,
    candidate: &Candidate,
) -> Result<Option<u64>> {
    let first = info.size;
    let second = candidate.size;
    if !sizes_conflict(first, second) {
        return Ok(first.or(second));
    }

    ui::warn(format!(
        "{}: file_info reports {} bytes but the share listing reports {}; re-checking metadata",
        info.name,
        first.unwrap_or(0),
        second.unwrap_or(0)
    ));
    *info = share::file_info(client, share_id, &candidate.file_id, token).await?;
    if sizes_conflict(info.size, candidate.size) {
        bail!(
            "{}: inconsistent size metadata (file_info={}, share={})",
            info.name,
            info.size
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            candidate
                .size
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        );
    }
    Ok(info.size.or(candidate.size))
}

fn sizes_conflict(first: Option<u64>, second: Option<u64>) -> bool {
    matches!((first, second), (Some(a), Some(b)) if a != b)
}

async fn wait_for_new_drive_file(
    client: &PikPak,
    name: &str,
    size: u64,
    before_ids: &std::collections::HashSet<String>,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(10 * 60);
    loop {
        let files = drive::collect_drive_files(client).await?;
        let mut matches: Vec<_> = files
            .into_iter()
            .filter(|file| {
                !file.is_folder()
                    && !before_ids.contains(&file.id)
                    && file.name == name
                    && file.size == Some(size)
            })
            .collect();
        match matches.len() {
            1 => return Ok(matches.remove(0).id),
            0 => {}
            count => bail!(
                "{name}: {count} new drive files match the restored name and size; refusing to guess"
            ),
        }
        if Instant::now() >= deadline {
            bail!(
                "{name}: the restored file did not appear as a new, uniquely identifiable drive entry"
            );
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
}

fn is_fatal_auth_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}");
    text.contains("not authenticated")
        || text.contains("token refresh failed")
        || text.contains("refresh token")
        || text.contains("invalid_grant")
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
                format_size(candidate.size.unwrap_or(0))
            );
        }
        let source: Arc<dyn LinkSource> = CachedLink::new(Arc::new(DriveLinkSource::new(
            client.clone(),
            candidate.file_id.clone(),
        )))
        .shared();
        let media = ResolvedMedia::drive(candidate.size);
        match download_file(
            source,
            &format!("drive:{}", candidate.file_id),
            &candidate.name,
            &media,
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

fn unique_names(candidates: &mut [Candidate]) -> Result<()> {
    let mut used = std::collections::HashSet::new();
    for candidate in candidates {
        candidate.name = unique_name(&mut used, &candidate.name)?;
    }
    Ok(())
}

fn unique_name(used: &mut std::collections::HashSet<String>, name: &str) -> Result<String> {
    let base = crate::util::sanitize_filename(name);
    if used.insert(base.to_lowercase()) {
        return Ok(base);
    }

    let (stem, extension) = match base.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (base.as_str(), String::new()),
    };

    // Reserve room for the suffix and the extension before truncating the
    // stem. Truncating first would eat the suffix and loop forever.
    for index in 2..=100_000u32 {
        let suffix = format!(" ({index})");
        let extension = if suffix.len() + extension.len() >= crate::util::MAX_COMPONENT_BYTES {
            String::new()
        } else {
            extension.clone()
        };
        let stem_budget =
            crate::util::MAX_COMPONENT_BYTES.saturating_sub(suffix.len() + extension.len());
        let stem = crate::util::truncate_utf8(stem, stem_budget);
        let candidate = format!("{stem}{suffix}{extension}");
        debug_assert!(candidate.len() <= crate::util::MAX_COMPONENT_BYTES);
        if candidate == base {
            continue;
        }
        if used.insert(candidate.to_lowercase()) {
            return Ok(candidate);
        }
    }

    bail!("cannot generate a unique filename for {name}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_conflicting_restore_sizes() {
        assert!(!sizes_conflict(Some(10), Some(10)));
        assert!(!sizes_conflict(Some(10), None));
        assert!(!sizes_conflict(None, Some(10)));
        assert!(!sizes_conflict(None, None));
        assert!(sizes_conflict(Some(10), Some(11)));
    }

    #[test]
    fn treats_session_failures_as_fatal() {
        assert!(is_fatal_auth_error(&anyhow!(
            "not authenticated; run `pkdw auth --from-clipboard` first"
        )));
        assert!(is_fatal_auth_error(&anyhow!(
            "PikPak rejected the refresh token (invalid_grant)"
        )));
        assert!(!is_fatal_auth_error(&anyhow!("not enough drive space")));
    }

    #[test]
    fn unique_names_terminate_for_long_ascii_names() {
        let mut used = std::collections::HashSet::new();
        let first = unique_name(&mut used, &"a".repeat(180)).unwrap();
        let second = unique_name(&mut used, &"a".repeat(180)).unwrap();
        assert_ne!(first, second);
        assert!(first.len() <= crate::util::MAX_COMPONENT_BYTES);
        assert!(second.len() <= crate::util::MAX_COMPONENT_BYTES);
        assert!(second.contains(" (2)"));
    }

    #[test]
    fn unique_names_keep_utf8_boundaries() {
        let mut used = std::collections::HashSet::new();
        let first = unique_name(&mut used, &"ñ😀".repeat(80)).unwrap();
        let second = unique_name(&mut used, &"ñ😀".repeat(80)).unwrap();
        assert_ne!(first, second);
        assert!(first.len() <= crate::util::MAX_COMPONENT_BYTES);
        assert!(second.len() <= crate::util::MAX_COMPONENT_BYTES);
        assert!(std::str::from_utf8(second.as_bytes()).is_ok());
    }

    #[test]
    fn unique_names_compare_case_insensitively() {
        let mut used = std::collections::HashSet::new();
        let first = unique_name(&mut used, "Movie.mkv").unwrap();
        let second = unique_name(&mut used, "movie.mkv").unwrap();
        assert_ne!(first.to_lowercase(), second.to_lowercase());
    }

    #[test]
    fn unique_names_bound_overlong_extensions() {
        let mut used = std::collections::HashSet::new();
        let name = format!("movie.{}", "x".repeat(300));
        let first = unique_name(&mut used, &name).unwrap();
        let second = unique_name(&mut used, &name).unwrap();
        assert_ne!(first, second);
        assert!(first.len() <= crate::util::MAX_COMPONENT_BYTES);
        assert!(second.len() <= crate::util::MAX_COMPONENT_BYTES);
    }
}
