use crate::api::client::PikPak;
use crate::api::models::{
    AboutResponse, DriveFile, DriveListResponse, FileInfo, RestoreResponse, TaskResponse,
};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::time::{Duration, Instant};

pub async fn list_page(client: &PikPak, parent_id: &str, limit: u32) -> Result<DriveListResponse> {
    let mut params = vec![
        ("limit".to_string(), limit.to_string()),
        ("thumbnail_size".to_string(), "SIZE_MEDIUM".to_string()),
    ];
    if !parent_id.is_empty() {
        params.push(("parent_id".to_string(), parent_id.to_string()));
    }
    client
        .authed_json(reqwest::Method::GET, "/drive/v1/files", &params, None)
        .await
        .context("cannot list the drive")
}

pub async fn list_all(client: &PikPak, parent_id: &str) -> Result<Vec<DriveFile>> {
    let mut files = Vec::new();
    let mut page_token = String::new();
    let mut seen = std::collections::HashSet::new();
    let mut pages = 0usize;
    loop {
        if pages > 10_000 {
            bail!("too many pages while listing drive parent {}", parent_id);
        }
        let mut params = vec![
            ("limit".to_string(), "200".to_string()),
            ("thumbnail_size".to_string(), "SIZE_MEDIUM".to_string()),
        ];
        if !parent_id.is_empty() {
            params.push(("parent_id".to_string(), parent_id.to_string()));
        }
        if !page_token.is_empty() {
            if !seen.insert(page_token.clone()) {
                bail!("drive listing repeated next_page_token: {page_token}");
            }
            params.push(("page_token".to_string(), page_token.clone()));
        }
        let page: DriveListResponse = client
            .authed_json(reqwest::Method::GET, "/drive/v1/files", &params, None)
            .await
            .context("cannot list the drive")?;
        files.extend(page.files);
        page_token = page.next_page_token;
        pages += 1;
        if page_token.is_empty() {
            break;
        }
    }
    Ok(files)
}

/// Resolve `/Folder/Sub/file.mkv` to a drive entry. `/` and an empty path both
/// resolve to a synthetic root folder with an empty id.
pub async fn resolve_path(client: &PikPak, path: &str) -> Result<DriveFile> {
    let path = path.trim();
    if path.is_empty() || path == "/" {
        return Ok(root_entry());
    }

    let parts: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Ok(root_entry());
    }

    let mut parent_id = String::new();
    let mut found: Option<DriveFile> = None;
    for (index, part) in parts.iter().enumerate() {
        let entries = list_all(client, &parent_id).await?;
        let mut matches: Vec<_> = entries
            .into_iter()
            .filter(|entry| entry.name == *part)
            .collect();
        if matches.len() > 1 {
            bail!(
                "ambiguous drive path: {} matches found for {}",
                matches.len(),
                part
            );
        }
        let entry = matches.pop().ok_or_else(|| {
            let prefix = parts[..index].join("/");
            if prefix.is_empty() {
                anyhow!("no such drive entry: {part}")
            } else {
                anyhow!("no such drive entry: /{prefix}/{part}")
            }
        })?;
        if index + 1 < parts.len() && !entry.is_folder() {
            bail!("/{} is not a folder", parts[..=index].join("/"));
        }
        parent_id = entry.id.clone();
        found = Some(entry);
    }

    Ok(found.expect("path had at least one component"))
}

fn root_entry() -> DriveFile {
    DriveFile {
        id: String::new(),
        name: "/".to_string(),
        kind: "drive#folder".to_string(),
        size: Some(0),
        hash: String::new(),
        web_content_link: String::new(),
        parent_id: String::new(),
        modified_time: String::new(),
        phase: String::new(),
    }
}

pub async fn file_info(client: &PikPak, file_id: &str) -> Result<FileInfo> {
    let params = vec![
        ("with_audit".to_string(), "true".to_string()),
        ("thumbnail_size".to_string(), "SIZE_LARGE".to_string()),
    ];
    client
        .authed_json(
            reqwest::Method::GET,
            &format!("/drive/v1/files/{file_id}"),
            &params,
            None,
        )
        .await
        .with_context(|| format!("cannot read drive file {file_id}"))
}

pub async fn download_link(client: &PikPak, file_id: &str) -> Result<String> {
    let info = file_info(client, file_id).await?;
    if !info.web_content_link.is_empty() {
        return Ok(info.web_content_link);
    }
    info.select_media(crate::cli::Quality::Original)
        .map(|media| media.url)
        .ok_or_else(|| anyhow!("file {file_id} has no download link"))
}

pub async fn quota(client: &PikPak) -> Result<AboutResponse> {
    client.about().await
}

pub async fn restore_share(
    client: &PikPak,
    share_id: &str,
    pass_code_token: &str,
    file_ids: &[String],
) -> Result<RestoreResponse> {
    let body = json!({
        "share_id": share_id,
        "pass_code_token": pass_code_token,
        "file_ids": file_ids,
    });
    client
        .authed_json(
            reqwest::Method::POST,
            "/drive/v1/share/restore",
            &[],
            Some(body),
        )
        .await
        .context("cannot copy the share to your drive")
}

pub async fn wait_task(client: &PikPak, task_id: &str, timeout: Duration) -> Result<TaskResponse> {
    let deadline = Instant::now() + timeout;
    loop {
        let params = vec![("thumbnail_size".to_string(), "SIZE_MEDIUM".to_string())];
        let task: TaskResponse = client
            .authed_json(
                reqwest::Method::GET,
                &format!("/drive/v1/tasks/{task_id}"),
                &params,
                None,
            )
            .await?;
        match task.phase.as_str() {
            "PHASE_TYPE_COMPLETE" => return Ok(task),
            "PHASE_TYPE_COMPLETE_WITH_ERROR" | "PHASE_TYPE_ERROR" | "PHASE_TYPE_FAILED" => {
                bail!(
                    "copy task failed ({phase}): {message}",
                    phase = task.phase,
                    message = task.message
                )
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for copy task {task_id}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn find_by_name(
    client: &PikPak,
    name: &str,
    size: Option<u64>,
    max_depth: usize,
) -> Result<Option<DriveFile>> {
    let mut stack = vec![(String::new(), 0usize)];
    let mut visited = std::collections::HashSet::new();
    while let Some((parent_id, depth)) = stack.pop() {
        if !parent_id.is_empty() && !visited.insert(parent_id.clone()) {
            continue;
        }
        for entry in list_all(client, &parent_id).await? {
            if entry.name == name && size.is_none_or(|wanted| entry.size.unwrap_or(0) == wanted) {
                return Ok(Some(entry));
            }
            if entry.is_folder() && depth < max_depth {
                stack.push((entry.id.clone(), depth + 1));
            }
        }
    }
    Ok(None)
}

pub async fn delete_file(client: &PikPak, file_id: &str) -> Result<()> {
    let _: serde_json::Value = client
        .authed_json(
            reqwest::Method::DELETE,
            &format!("/drive/v1/files/{file_id}"),
            &[],
            None,
        )
        .await?;
    Ok(())
}

pub async fn find_many_by_name(
    client: &PikPak,
    name: &str,
    size: Option<u64>,
    max_depth: usize,
) -> Result<Vec<DriveFile>> {
    let mut matches = Vec::new();
    let mut stack = vec![(String::new(), 0usize)];
    let mut visited = std::collections::HashSet::new();
    while let Some((parent_id, depth)) = stack.pop() {
        if !parent_id.is_empty() && !visited.insert(parent_id.clone()) {
            continue;
        }
        for entry in list_all(client, &parent_id).await? {
            if entry.name == name && size.is_none_or(|wanted| entry.size.unwrap_or(0) == wanted) {
                matches.push(entry.clone());
            }
            if entry.is_folder() && depth < max_depth {
                stack.push((entry.id.clone(), depth + 1));
            }
        }
    }
    Ok(matches)
}
