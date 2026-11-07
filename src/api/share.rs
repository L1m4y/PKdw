use crate::api::client::PikPak;
use crate::api::models::{
    FileInfo, FileInfoEnvelope, ShareDetailResponse, ShareEntry, ShareResponse,
};
use crate::ui;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone)]
pub struct ShareFiles {
    pub entries: Vec<ShareEntry>,
    pub tree: Vec<TreeEntry>,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub depth: usize,
    pub entry: ShareEntry,
}

pub async fn share_info(client: &PikPak, share_id: &str, pass_code: &str) -> Result<ShareResponse> {
    let params = vec![
        ("share_id".to_string(), share_id.to_string()),
        ("pass_code".to_string(), pass_code.to_string()),
        ("thumbnail_size".to_string(), "SIZE_LARGE".to_string()),
        ("limit".to_string(), "100".to_string()),
    ];
    client
        .anon_json::<ShareResponse>("/drive/v1/share", &params)
        .await
        .with_context(|| format!("cannot read share {share_id}"))
}

/// Resolve a share, prompting for a pass code only when the share needs one.
pub async fn resolve_pass_code_token(
    client: &PikPak,
    share_id: &str,
    pass_code: Option<&str>,
) -> Result<String> {
    let mut code = pass_code.unwrap_or("").to_string();
    let mut first = true;

    loop {
        let info = share_info(client, share_id, &code).await?;
        match info.share_status.as_str() {
            "OK" | "" => {
                if info.pass_code_token.is_empty() {
                    bail!("share {share_id} did not return a pass_code_token");
                }
                return Ok(info.pass_code_token);
            }
            "PASS_CODE_EMPTY" | "PASS_CODE_ERROR" => {
                if first && !code.is_empty() {
                    bail!("invalid pass code for share {share_id}");
                }
                if !atty_stdin() {
                    bail!(
                        "share {share_id} is protected; pass --pass-code <CODE> in non-interactive mode"
                    );
                }
                eprintln!("Share {share_id} is protected by a pass code.");
                let entered = rpassword::prompt_password("Pass code: ")
                    .context("cannot read the pass code")?;
                if entered.trim().is_empty() {
                    bail!("no pass code supplied");
                }
                code = entered.trim().to_string();
                first = false;
            }
            status => {
                let detail = if info.share_status_text.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", info.share_status_text)
                };
                bail!("share {share_id} is not available: {status}{detail}");
            }
        }
    }
}

pub async fn share_detail_page(
    client: &PikPak,
    share_id: &str,
    parent_id: &str,
    pass_code_token: &str,
    page_token: Option<&str>,
) -> Result<ShareDetailResponse> {
    let mut params = vec![
        ("share_id".to_string(), share_id.to_string()),
        ("pass_code_token".to_string(), pass_code_token.to_string()),
        ("thumbnail_size".to_string(), "SIZE_LARGE".to_string()),
        ("limit".to_string(), "100".to_string()),
        ("with_audit".to_string(), "true".to_string()),
        (
            "filters".to_string(),
            json!({"phase": {"eq": "PHASE_TYPE_COMPLETE"}, "trashed": {"eq": false}}).to_string(),
        ),
    ];
    if !parent_id.is_empty() {
        params.push(("parent_id".to_string(), parent_id.to_string()));
    }
    if let Some(page_token) = page_token {
        if !page_token.is_empty() {
            params.push(("page_token".to_string(), page_token.to_string()));
        }
    }
    client
        .anon_json::<ShareDetailResponse>("/drive/v1/share/detail", &params)
        .await
        .context("cannot list the share")
}

pub async fn collect_share(
    client: &PikPak,
    share_id: &str,
    pass_code_token: &str,
    print_tree: bool,
    quiet: bool,
) -> Result<ShareFiles> {
    collect_share_at(client, share_id, pass_code_token, "", print_tree, quiet).await
}

pub async fn collect_share_at(
    client: &PikPak,
    share_id: &str,
    pass_code_token: &str,
    parent_id: &str,
    print_tree: bool,
    quiet: bool,
) -> Result<ShareFiles> {
    let mut files = ShareFiles {
        entries: Vec::new(),
        tree: Vec::new(),
    };
    let mut visited = std::collections::HashSet::new();
    walk(
        client,
        share_id,
        pass_code_token,
        parent_id,
        0,
        print_tree,
        quiet,
        &mut files,
        &mut visited,
    )
    .await?;
    Ok(files)
}

#[allow(clippy::too_many_arguments)]
async fn walk(
    client: &PikPak,
    share_id: &str,
    pass_code_token: &str,
    parent_id: &str,
    depth: usize,
    print_tree: bool,
    quiet: bool,
    files: &mut ShareFiles,
    visited: &mut std::collections::HashSet<String>,
) -> Result<()> {
    if depth > 32 {
        bail!("share folder nesting is too deep");
    }
    if !parent_id.is_empty() && !visited.insert(parent_id.to_string()) {
        return Ok(());
    }

    let mut page_token = String::new();
    loop {
        let token = if page_token.is_empty() {
            None
        } else {
            Some(page_token.as_str())
        };
        let page = share_detail_page(client, share_id, parent_id, pass_code_token, token).await?;
        for entry in page.files {
            if print_tree && !quiet {
                ui::tree_line(depth, &entry);
            }
            if entry.is_folder() {
                files.tree.push(TreeEntry {
                    depth,
                    entry: entry.clone(),
                });
                Box::pin(walk(
                    client,
                    share_id,
                    pass_code_token,
                    &entry.id,
                    depth + 1,
                    print_tree,
                    quiet,
                    files,
                    visited,
                ))
                .await?;
            } else {
                files.tree.push(TreeEntry {
                    depth,
                    entry: entry.clone(),
                });
                files.entries.push(entry);
            }
        }
        page_token = page.next_page_token;
        if page_token.is_empty() {
            break;
        }
    }
    Ok(())
}

pub async fn file_info(
    client: &PikPak,
    share_id: &str,
    file_id: &str,
    pass_code_token: &str,
) -> Result<FileInfo> {
    let params = vec![
        ("share_id".to_string(), share_id.to_string()),
        ("file_id".to_string(), file_id.to_string()),
        ("pass_code_token".to_string(), pass_code_token.to_string()),
        ("with_audit".to_string(), "true".to_string()),
        ("thumbnail_size".to_string(), "SIZE_LARGE".to_string()),
    ];
    let value = client
        .anon_request(
            reqwest::Method::GET,
            "/drive/v1/share/file_info",
            &params,
            None,
        )
        .await?;
    parse_file_info(value).with_context(|| format!("share file {file_id} is not available"))
}

pub async fn file_info_authed(
    client: &PikPak,
    share_id: &str,
    file_id: &str,
    pass_code_token: &str,
) -> Result<FileInfo> {
    let params = vec![
        ("share_id".to_string(), share_id.to_string()),
        ("file_id".to_string(), file_id.to_string()),
        ("pass_code_token".to_string(), pass_code_token.to_string()),
        ("with_audit".to_string(), "true".to_string()),
        ("thumbnail_size".to_string(), "SIZE_LARGE".to_string()),
    ];
    let value = client
        .authed_request(
            reqwest::Method::GET,
            "/drive/v1/share/file_info",
            &params,
            None,
        )
        .await?;
    parse_file_info(value).with_context(|| format!("share file {file_id} is not available"))
}

fn parse_file_info(value: Value) -> Option<FileInfo> {
    if value.get("file_info").is_some() {
        serde_json::from_value::<FileInfoEnvelope>(value)
            .ok()
            .and_then(FileInfoEnvelope::into_info)
    } else {
        serde_json::from_value::<FileInfo>(value).ok()
    }
}

fn atty_stdin() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}
