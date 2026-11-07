use crate::api::client::PikPak;
use crate::api::models::FileInfo;
use crate::api::{drive, share};
use crate::cli::Quality;
use crate::ui;
use crate::util::{format_size, parse_content_range, sanitize_filename};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use reqwest::{header::RANGE, StatusCode};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt;

#[derive(Debug, Clone)]
pub struct DownloadOptions {
    pub http: reqwest::Client,
    pub connections: usize,
    pub chunk_size: u64,
    pub rounds: u16,
    pub quiet: bool,
    pub overwrite: bool,
}

#[derive(Debug, Clone)]
pub enum DownloadOutcome {
    Complete(PathBuf),
    Incomplete {
        part: PathBuf,
        missing_bytes: u64,
        missing_segments: usize,
    },
}

#[async_trait]
pub trait LinkSource: Send + Sync {
    /// Resolve a fresh download URL. `force_refresh` bypasses any cache and is
    /// used after a failed range request.
    async fn link(&self, force_refresh: bool) -> Result<String>;
}

pub struct CachedLink {
    inner: Arc<dyn LinkSource>,
    cache: Mutex<Option<(String, Instant)>>,
    ttl: Duration,
}

impl CachedLink {
    pub fn new(inner: Arc<dyn LinkSource>) -> Self {
        Self {
            inner,
            cache: Mutex::new(None),
            ttl: Duration::from_secs(90),
        }
    }

    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[async_trait]
impl LinkSource for CachedLink {
    async fn link(&self, force_refresh: bool) -> Result<String> {
        let mut cache = self.cache.lock().await;
        if !force_refresh {
            if let Some((url, fetched_at)) = cache.as_ref() {
                if fetched_at.elapsed() < self.ttl {
                    return Ok(url.clone());
                }
            }
        }
        let url = self.inner.link(force_refresh).await?;
        *cache = Some((url.clone(), Instant::now()));
        Ok(url)
    }
}

pub struct ShareLinkSource {
    client: PikPak,
    share_id: String,
    file_id: String,
    pass_code_token: String,
    quality: Quality,
    authenticated: bool,
}

impl ShareLinkSource {
    pub fn new(
        client: PikPak,
        share_id: impl Into<String>,
        file_id: impl Into<String>,
        pass_code_token: impl Into<String>,
        quality: Quality,
        authenticated: bool,
    ) -> Self {
        Self {
            client,
            share_id: share_id.into(),
            file_id: file_id.into(),
            pass_code_token: pass_code_token.into(),
            quality,
            authenticated,
        }
    }
}

#[async_trait]
impl LinkSource for ShareLinkSource {
    async fn link(&self, _force_refresh: bool) -> Result<String> {
        let info = if self.authenticated {
            share::file_info_authed(
                &self.client,
                &self.share_id,
                &self.file_id,
                &self.pass_code_token,
            )
            .await?
        } else {
            share::file_info(
                &self.client,
                &self.share_id,
                &self.file_id,
                &self.pass_code_token,
            )
            .await?
        };
        selected_url(&info, self.quality)
    }
}

pub struct DriveLinkSource {
    client: PikPak,
    file_id: String,
}

impl DriveLinkSource {
    pub fn new(client: PikPak, file_id: impl Into<String>) -> Self {
        Self {
            client,
            file_id: file_id.into(),
        }
    }
}

#[async_trait]
impl LinkSource for DriveLinkSource {
    async fn link(&self, _force_refresh: bool) -> Result<String> {
        drive::download_link(&self.client, &self.file_id).await
    }
}

pub fn selected_url(info: &FileInfo, quality: Quality) -> Result<String> {
    if let Some(media) = info.select_media(quality) {
        if media.label.to_ascii_lowercase() != quality.key() {
            ui::warn(format!(
                "{} is not available for {}; using {}",
                quality.key(),
                info.name,
                media.label
            ));
        }
        return Ok(media.url);
    }
    if info.quota_blocked_original() && quality == Quality::Original {
        bail!(
            "{}: the original stream needs more quota; use --quality 1080p or free drive space",
            info.name
        );
    }
    bail!(
        "{}: no downloadable stream for quality {}",
        info.name,
        quality.key()
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartState {
    version: u32,
    source_id: String,
    size: u64,
    completed: Vec<(u64, u64)>,
}

impl PartState {
    fn new(source_id: &str, size: u64) -> Self {
        Self {
            version: 1,
            source_id: source_id.to_string(),
            size,
            completed: Vec::new(),
        }
    }

    fn completed_bytes(&self) -> u64 {
        self.completed
            .iter()
            .map(|(start, end)| end.saturating_sub(*start) + 1)
            .sum()
    }

    fn covers(&self, start: u64, end: u64) -> bool {
        self.completed
            .iter()
            .any(|(done_start, done_end)| *done_start <= start && end <= *done_end)
    }

    fn insert(&mut self, start: u64, end: u64) {
        self.completed.push((start, end));
        self.completed.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.completed.len());
        for (current_start, current_end) in self.completed.drain(..) {
            if let Some(last) = merged.last_mut() {
                if current_start <= last.1.saturating_add(1) {
                    last.1 = last.1.max(current_end);
                    continue;
                }
            }
            merged.push((current_start, current_end));
        }
        self.completed = merged;
    }
}

pub async fn download_file(
    source: Arc<dyn LinkSource>,
    source_id: &str,
    name: &str,
    size: u64,
    dest_dir: &Path,
    options: &DownloadOptions,
) -> Result<DownloadOutcome> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .with_context(|| format!("cannot create {}", dest_dir.display()))?;
    let dest = dest_dir.join(sanitize_filename(name));
    let part = append_suffix(&dest, ".part");
    let state_path = append_suffix(&dest, ".part.pkdw.json");

    if options.overwrite {
        remove_if_exists(&part).await?;
        remove_if_exists(&state_path).await?;
        remove_if_exists(&dest).await?;
    }

    if dest.exists() {
        let existing = std::fs::metadata(&dest)?.len();
        if size == 0 || existing == size {
            ui::status(
                options.quiet,
                format!(
                    "already downloaded: {} ({})",
                    dest.display(),
                    format_size(existing)
                ),
            );
            return Ok(DownloadOutcome::Complete(dest));
        }
        bail!(
            "destination exists with a different size: {} ({} bytes, expected {}); use --overwrite",
            dest.display(),
            existing,
            size
        );
    }

    if size == 0 {
        File::create(&dest)?;
        return Ok(DownloadOutcome::Complete(dest));
    }

    let state_existed = state_path.exists();
    let part_existed = part.exists();
    let mut state = match std::fs::read_to_string(&state_path) {
        Ok(raw) => serde_json::from_str::<PartState>(&raw)
            .with_context(|| format!("resume state is corrupt: {}", state_path.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => PartState::new(source_id, size),
        Err(error) => return Err(error.into()),
    };

    if state.size != size || state.source_id != source_id {
        bail!(
            "resume state belongs to a different file ({} bytes, source {}); use --overwrite",
            state.size,
            state.source_id
        );
    }

    // A state file without its partial file would make us skip bytes that are
    // not on disk. An interrupted first run (part created, no state yet) is
    // safe to adopt because every segment will be written again.
    if state_existed && !part_existed {
        state.completed.clear();
    }

    let file = Arc::new(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&part)
            .with_context(|| format!("cannot open {}", part.display()))?,
    );
    let current_len = file.metadata()?.len();
    if current_len > size {
        bail!(
            "partial file is larger than expected ({} > {}); use --overwrite",
            current_len,
            size
        );
    }
    if current_len < size {
        file.set_len(size)
            .with_context(|| format!("cannot reserve {} bytes", size))?;
    }

    // Create the sidecar immediately so a crash during the first segment is
    // still resumable on the next run.
    if !state_existed || !part_existed {
        save_state(&state_path, &state)?;
    }

    let state = Arc::new(Mutex::new(state));
    let completed_before = state.lock().await.completed_bytes();
    if completed_before > 0 {
        ui::status(
            options.quiet,
            format!(
                "resuming {}: {} already present",
                dest.display(),
                format_size(completed_before)
            ),
        );
    }

    let bar = ui::progress_bar(size, name, options.quiet);
    if completed_before > 0 {
        bar.set_position(completed_before.min(size));
    }

    let chunk = options.chunk_size.max(1024 * 1024);
    let all_segments = plan_segments(size, chunk);
    let mut pending: Vec<(u64, u64)> = {
        let guard = state.lock().await;
        all_segments
            .into_iter()
            .filter(|(start, end)| !guard.covers(*start, *end))
            .collect()
    };

    let mut last_missing = 0usize;
    for round in 1..=options.rounds.max(1) {
        if pending.is_empty() {
            break;
        }
        if round > 1 {
            let wait = Duration::from_secs((round as u64 * 5).min(30));
            ui::status(
                options.quiet,
                format!(
                    "round {round}: retrying {} segment(s) after {}s",
                    pending.len(),
                    wait.as_secs()
                ),
            );
            tokio::time::sleep(wait).await;
        }

        let results = futures::stream::iter(pending.iter().copied())
            .map(|(start, end)| {
                let source = source.clone();
                let file = file.clone();
                let state = state.clone();
                let state_path = state_path.clone();
                let bar = bar.clone();
                let http = options.http.clone();
                async move {
                    let result =
                        download_segment(source, http, file, state, state_path, bar, start, end)
                            .await;
                    (start, end, result)
                }
            })
            .buffer_unordered(options.connections.max(1))
            .collect::<Vec<_>>()
            .await;

        pending = results
            .into_iter()
            .filter_map(|(start, end, result)| match result {
                Ok(()) => None,
                Err(error) => {
                    if !options.quiet {
                        eprintln!("segment {start}-{end} failed: {error:#}");
                    }
                    Some((start, end))
                }
            })
            .collect();
        last_missing = pending.len();
    }

    if pending.is_empty() {
        file.sync_all().ok();
        drop(file);
        if dest.exists() {
            remove_if_exists(&dest).await?;
        }
        tokio::fs::rename(&part, &dest)
            .await
            .with_context(|| format!("cannot move {} into place", part.display()))?;
        remove_if_exists(&state_path).await?;
        bar.finish_and_clear();
        ui::status(
            options.quiet,
            format!("saved: {} ({})", dest.display(), format_size(size)),
        );
        return Ok(DownloadOutcome::Complete(dest));
    }

    bar.finish_and_clear();
    let (missing_bytes, missing_segments) = {
        let bytes = pending
            .iter()
            .map(|(start, end)| end.saturating_sub(*start) + 1)
            .sum();
        (bytes, last_missing)
    };
    ui::warn(format!(
        "incomplete: {} missing in {} segment(s); kept {} for the next run",
        format_size(missing_bytes),
        missing_segments,
        part.display()
    ));
    Ok(DownloadOutcome::Incomplete {
        part,
        missing_bytes,
        missing_segments,
    })
}

#[allow(clippy::too_many_arguments)]
async fn download_segment(
    source: Arc<dyn LinkSource>,
    http: reqwest::Client,
    file: Arc<File>,
    state: Arc<Mutex<PartState>>,
    state_path: PathBuf,
    bar: indicatif::ProgressBar,
    start: u64,
    end: u64,
) -> Result<()> {
    let expected = end - start + 1;
    let attempts = 4u32;
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 0..attempts {
        let force = attempt > 0;
        let url = match source.link(force).await {
            Ok(url) => url,
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        match fetch_range(&http, &url, start, end, &file).await {
            Ok(()) => {
                {
                    let mut guard = state.lock().await;
                    guard.insert(start, end);
                    save_state(&state_path, &guard)?;
                }
                bar.inc(expected);
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("range {start}-{end} failed")))
}

async fn fetch_range(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    file: &File,
) -> Result<()> {
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .with_context(|| format!("range request {start}-{end} failed"))?;

    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        bail!("range {start}-{end} is not available from the CDN (HTTP 416)");
    }
    if status == StatusCode::OK {
        bail!("the CDN ignored the Range header for {start}-{end}");
    }
    if !status.is_success() {
        bail!("range {start}-{end} failed (HTTP {status})");
    }

    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("range response has no Content-Range"))?;
    let (actual_start, actual_end, _) = parse_content_range(content_range)?;
    if actual_start != start || actual_end < end {
        bail!(
            "CDN returned range {actual_start}-{actual_end}, expected {start}-{end} (short or shifted)"
        );
    }

    let mut stream = response.bytes_stream();
    let mut offset = start;
    while let Some(chunk) = stream.try_next().await.context("download stream failed")? {
        if chunk.is_empty() {
            continue;
        }
        write_all_at(file, &chunk, offset)
            .with_context(|| format!("cannot write at offset {offset}"))?;
        offset += chunk.len() as u64;
        if offset > end + 1 {
            bail!("CDN sent more bytes than requested for {start}-{end}");
        }
    }
    if offset != end + 1 {
        bail!(
            "CDN sent {} of {} bytes for range {start}-{end}",
            offset.saturating_sub(start),
            end - start + 1
        );
    }
    Ok(())
}

fn write_all_at(file: &File, bytes: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        file.write_all_at(bytes, offset)
    }
    #[cfg(windows)]
    {
        let mut written = 0usize;
        while written < bytes.len() {
            let count = file.seek_write(&bytes[written..], offset + written as u64)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write bytes",
                ));
            }
            written += count;
        }
        Ok(())
    }
}

fn save_state(path: &Path, state: &PartState) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec(state)?;
    std::fs::write(&tmp, encoded)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn plan_segments(size: u64, chunk: u64) -> Vec<(u64, u64)> {
    if size == 0 || chunk == 0 {
        return Vec::new();
    }
    let mut segments = Vec::with_capacity(size.div_ceil(chunk) as usize);
    let mut start = 0u64;
    while start < size {
        let end = start.saturating_add(chunk - 1).min(size - 1);
        segments.push((start, end));
        start = end + 1;
    }
    segments
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs((2u64.pow(attempt.min(4))).min(20))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_fixed_grid() {
        assert_eq!(plan_segments(10, 4), vec![(0, 3), (4, 7), (8, 9)]);
        assert_eq!(plan_segments(8, 4), vec![(0, 3), (4, 7)]);
        assert_eq!(plan_segments(0, 4), Vec::<(u64, u64)>::new());
    }

    #[test]
    fn merges_completed_ranges() {
        let mut state = PartState::new("id", 100);
        state.insert(10, 19);
        state.insert(20, 29);
        state.insert(0, 9);
        assert_eq!(state.completed, vec![(0, 29)]);
        assert!(state.covers(0, 29));
        assert!(!state.covers(0, 30));
        assert_eq!(state.completed_bytes(), 30);
    }

    #[test]
    fn appends_suffix_without_replacing_extension() {
        let path = Path::new("/tmp/movie.mkv");
        assert_eq!(
            append_suffix(path, ".part"),
            PathBuf::from("/tmp/movie.mkv.part")
        );
    }
}
