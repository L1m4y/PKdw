use crate::api::client::PikPak;
use crate::api::models::FileInfo;
use crate::api::{drive, share};
use crate::cli::Quality;
use crate::ui;
use crate::util::{format_size, parse_content_range, sanitize_filename, ParsedContentRange};
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

const PART_STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartState {
    version: u32,
    source_id: String,
    size: u64,
    representation: String,
    completed: Vec<(u64, u64)>,
}

impl PartState {
    fn new(source_id: &str, size: u64, representation: &str) -> Self {
        Self {
            version: PART_STATE_VERSION,
            source_id: source_id.to_string(),
            size,
            representation: representation.to_string(),
            completed: Vec::new(),
        }
    }

    fn validate_for(&self, expected_size: u64, representation: &str) -> Result<()> {
        if self.version != PART_STATE_VERSION {
            bail!("resume state version {} is not supported", self.version);
        }
        if self.size != expected_size {
            bail!(
                "resume state belongs to a different file ({} bytes, expected {})",
                self.size,
                expected_size
            );
        }
        if self.representation != representation {
            bail!(
                "resume state belongs to representation {}, not {}",
                self.representation,
                representation
            );
        }
        let mut total = 0u64;
        let mut last_end: Option<u64> = None;
        for (start, end) in &self.completed {
            if start > end {
                bail!("resume state contains an invalid range, start > end");
            }
            if *end >= self.size {
                bail!("resume state contains a range outside the file size");
            }
            if let Some(last) = last_end {
                if *start <= last {
                    bail!("resume state contains overlapping or out-of-order ranges");
                }
            }
            last_end = Some(*end);
            total = total
                .checked_add(end.saturating_sub(*start) + 1)
                .ok_or_else(|| anyhow!("resume state contains an implausibly large range"))?;
        }
        if total > self.size {
            bail!("resume state contains more completed bytes than the file");
        }
        Ok(())
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
    size: Option<u64>,
    dest_dir: &Path,
    options: &DownloadOptions,
    representation: &str,
) -> Result<DownloadOutcome> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .with_context(|| format!("cannot create {}", dest_dir.display()))?;
    let size = if let Some(size) = size {
        size
    } else {
        let url = source.link(true).await?;
        // The probe resolves and caches this exact URL. Keep using it for the
        // segment requests instead of rotating the link a second time.
        probe_content_length(&options.http, &url).await?.size
    };

    let dest = dest_dir.join(sanitize_filename(name));
    let part = append_suffix(&dest, ".part");
    let state_path = append_suffix(&dest, ".part.pkdw.json");
    let manifest_path = append_suffix(&dest, ".pkdw.manifest.json");

    if options.overwrite {
        remove_if_exists(&part).await?;
        remove_if_exists(&state_path).await?;
        remove_if_exists(&manifest_path).await?;
        remove_if_exists(&dest).await?;
    }

    if dest.exists() {
        let existing = std::fs::metadata(&dest)?.len();
        if existing != size {
            bail!(
                "destination exists with a different size: {} ({} bytes, expected {}); use --overwrite",
                dest.display(),
                existing,
                size
            );
        }

        let mut manifest_ok = std::fs::read_to_string(&manifest_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .is_some_and(|manifest| {
                manifest
                    .get("source_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(source_id)
                    && manifest.get("size").and_then(serde_json::Value::as_u64) == Some(size)
                    && manifest
                        .get("representation")
                        .and_then(serde_json::Value::as_str)
                        == Some(representation)
            });
        if manifest_ok {
            manifest_ok = manifest_hash_ok(&manifest_path, &dest);
        }
        if manifest_ok {
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
            "destination {} has the expected size but no verified manifest; use --overwrite",
            dest.display()
        );
    }

    let state_existed = state_path.exists();
    let part_existed = part.exists();
    let mut state = match std::fs::read_to_string(&state_path) {
        Ok(raw) => serde_json::from_str::<PartState>(&raw)
            .with_context(|| format!("resume state is corrupt: {}", state_path.display()))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            PartState::new(source_id, size, representation)
        }
        Err(error) => return Err(error.into()),
    };

    let part_truncated = part_existed
        && std::fs::metadata(&part)
            .map(|metadata| metadata.len() < size)
            .unwrap_or(false);

    if state.source_id != source_id {
        bail!(
            "resume state belongs to a different source ({}; expected {})",
            state.source_id,
            source_id
        );
    }
    state.validate_for(size, representation)?;

    // Do not trust byte ranges whose file is missing or has been truncated.
    // Persist that invalidation immediately; otherwise the next launch may
    // still believe that segments which are no longer on disk are complete.
    if !part_existed || part_truncated {
        state.completed.clear();
        if part_truncated {
            ui::warn("partial file is shorter than its sidecar; all segments will be retried");
        }
        save_state(&state_path, &state)?;
    } else if !state_existed {
        // A file can be created ahead of the first completed segment. Keep
        // the sidecar present so an early interruption is still resumable.
        save_state(&state_path, &state)?;
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
                let total_size = size;
                async move {
                    let result = download_segment(
                        source, http, file, state, state_path, bar, start, end, total_size,
                    )
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
        file.sync_all()
            .context("failed to sync completed download")?;
        drop(file);
        if dest.exists() {
            remove_if_exists(&dest).await?;
        }
        let sha256 = sha256_file(&part)?;
        tokio::fs::rename(&part, &dest)
            .await
            .with_context(|| format!("cannot move {} into place", part.display()))?;
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "source_id": source_id,
                "size": size,
                "representation": representation,
                "sha256": sha256,
            }))?,
        )?;
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
    total_size: u64,
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

        match fetch_range(&http, &url, start, end, &file, total_size).await {
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
    expected_total: u64,
) -> Result<()> {
    let response = client
        .get(url)
        .header(RANGE, format!("bytes={start}-{end}"))
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .with_context(|| format!("range request {start}-{end} failed"))?;

    let status = response.status();
    if status == StatusCode::OK {
        bail!("the CDN ignored Range while fetching {start}-{end} (HTTP 200)");
    }
    if status != StatusCode::PARTIAL_CONTENT {
        bail!("range {start}-{end} was not delivered as 206 Partial Content (HTTP {status})");
    }
    ensure_identity_encoding(&response)?;

    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("range response has no Content-Range"))?;
    match parse_content_range(content_range)? {
        crate::util::ParsedContentRange::Unsatisfied { total } => {
            bail!("range {start}-{end} is unavailable from the CDN (HTTP 416; complete length {total})")
        }
        crate::util::ParsedContentRange::Satisfied {
            start: actual_start,
            end: actual_end,
            total: actual_total,
        } => {
            if actual_start != start || actual_end != end {
                bail!(
                    "CDN returned range {actual_start}-{actual_end}, expected {start}-{end} (short or shifted)"
                );
            }
            if expected_total > 0 {
                let total = actual_total.ok_or_else(|| {
                    anyhow!("CDN did not report a numeric total for range {start}-{end}")
                })?;
                if total != expected_total {
                    bail!("CDN returned total {total}, expected {expected_total} for range {start}-{end}");
                }
            }
        }
    }

    let mut stream = response.bytes_stream();
    let mut offset = start;
    while let Some(chunk) = stream.try_next().await.context("download stream failed")? {
        if chunk.is_empty() {
            continue;
        }
        let next = offset
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| anyhow!("range {start}-{end} length overflow"))?;
        if next > end + 1 {
            bail!("CDN sent more bytes than requested for range {start}-{end}");
        }
        write_all_at(file, &chunk, offset)
            .with_context(|| format!("cannot write at offset {offset}"))?;
        offset = next;
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

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn manifest_hash_ok(manifest_path: &Path, dest: &Path) -> bool {
    let manifest = match std::fs::read_to_string(manifest_path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => value,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let Some(expected) = manifest.get("sha256").and_then(serde_json::Value::as_str) else {
        return false;
    };
    sha256_file(dest)
        .map(|actual| actual == expected)
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct ProbeResult {
    size: u64,
}

/// Probe a CDN link with a one-byte range request.
///
/// The URL is never included in errors: PikPak CDN links carry signed query
/// parameters that must not end up in terminals, CI logs or diagnostics.
async fn probe_content_length(client: &reqwest::Client, url: &str) -> Result<ProbeResult> {
    let response = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .context("cannot probe the CDN link")?;

    let status = response.status();

    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| anyhow!("probe returned HTTP 416 without Content-Range"))?;
        return match parse_content_range(content_range)? {
            ParsedContentRange::Unsatisfied { total } => Ok(ProbeResult { size: total }),
            ParsedContentRange::Satisfied { .. } => Err(anyhow!(
                "probe returned HTTP 416 with a satisfied Content-Range"
            )),
        };
    }

    if status == StatusCode::OK {
        return Err(anyhow!(
            "the CDN ignored Range (HTTP 200); sequential GET downloads are not supported"
        ));
    }
    if status != StatusCode::PARTIAL_CONTENT {
        return Err(anyhow!(
            "the CDN answered the size probe with HTTP {status}"
        ));
    }

    ensure_identity_encoding(&response)?;
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("probe response has no Content-Range"))?
        .to_string();
    let total = match parse_content_range(&content_range)? {
        ParsedContentRange::Satisfied {
            start: 0,
            end: 0,
            total: Some(total),
        } => total,
        ParsedContentRange::Satisfied { start, end, total } => {
            return Err(anyhow!(
            "probe returned range {start}-{end} with total {total:?}, expected bytes 0-0/<total>"
        ))
        }
        ParsedContentRange::Unsatisfied { .. } => {
            return Err(anyhow!(
                "probe reported an unavailable range instead of bytes 0-0"
            ))
        }
    };

    let body = response
        .bytes()
        .await
        .context("cannot read the probe response body")?;
    if body.len() != 1 {
        return Err(anyhow!(
            "probe returned {} bytes for a bytes=0-0 request",
            body.len()
        ));
    }

    Ok(ProbeResult { size: total })
}

fn ensure_identity_encoding(response: &reqwest::Response) -> Result<()> {
    let Some(encoding) = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    if encoding.eq_ignore_ascii_case("identity") {
        return Ok(());
    }
    bail!("the CDN returned Content-Encoding: {encoding}; byte ranges need identity encoding")
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
        let mut state = PartState::new("id", 100, "original");
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

    fn probe_client() -> reqwest::Client {
        reqwest::Client::builder().no_gzip().build().unwrap()
    }

    async fn spawn_raw_server(response: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let _ = socket.write_all(response.as_bytes()).await;
        });
        format!("http://{address}/file?token=SECRET&sign=SECRET2")
    }

    #[tokio::test]
    async fn probe_accepts_satisfied_range_with_numeric_total() {
        let url = spawn_raw_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/5\r\nConnection: close\r\n\r\nX",
        )
        .await;
        let result = probe_content_length(&probe_client(), &url).await.unwrap();
        assert_eq!(result.size, 5);
    }

    #[tokio::test]
    async fn probe_rejects_satisfied_range_without_numeric_total() {
        let url = spawn_raw_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/*\r\nConnection: close\r\n\r\nX",
        )
        .await;
        assert!(probe_content_length(&probe_client(), &url).await.is_err());
    }

    #[tokio::test]
    async fn probe_accepts_unsatisfied_zero_length() {
        let url = spawn_raw_server(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nContent-Range: bytes */0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let result = probe_content_length(&probe_client(), &url).await.unwrap();
        assert_eq!(result.size, 0);
    }

    #[tokio::test]
    async fn probe_accepts_unsatisfied_numeric_length() {
        let url = spawn_raw_server(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nContent-Range: bytes */123\r\nConnection: close\r\n\r\n",
        )
        .await;
        let result = probe_content_length(&probe_client(), &url).await.unwrap();
        assert_eq!(result.size, 123);
    }

    #[tokio::test]
    async fn probe_rejects_416_without_content_range() {
        let url = spawn_raw_server(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(probe_content_length(&probe_client(), &url).await.is_err());
    }

    #[tokio::test]
    async fn probe_rejects_200() {
        let url = spawn_raw_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        )
        .await;
        let error = probe_content_length(&probe_client(), &url)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("ignored Range"));
    }

    #[tokio::test]
    async fn probe_rejects_compressed_ranges() {
        let url = spawn_raw_server(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 0-0/5\r\nContent-Encoding: gzip\r\nConnection: close\r\n\r\nX",
        )
        .await;
        let error = probe_content_length(&probe_client(), &url)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("Content-Encoding"));
    }

    #[tokio::test]
    async fn probe_errors_never_include_the_signed_url() {
        let url = spawn_raw_server(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let error = probe_content_length(&probe_client(), &url)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains("SECRET"));
        assert!(!text.contains("SECRET2"));
        assert!(!text.contains("token="));
    }
}
