use crate::api::PikPak;
use crate::cli::{AuthArgs, OutputArgs};
use crate::ui;
use crate::util::format_size;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::io::{IsTerminal, Read};

pub async fn run(client: &PikPak, args: &AuthArgs, output: &OutputArgs) -> Result<i32> {
    if args.logout {
        client.logout().await?;
        ui::status(output.quiet, "refresh token removed from disk");
        return Ok(0);
    }

    let token = if let Some(token) = &args.token {
        token.trim().to_string()
    } else if args.from_clipboard {
        read_clipboard()?
    } else if args.stdin {
        read_stdin()?
    } else if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Refresh token (input hidden): ")
            .context("cannot read the refresh token")?
            .trim()
            .to_string()
    } else {
        read_stdin()?
    };

    if token.is_empty() {
        bail!("no refresh token supplied");
    }

    let session = client.login_with_token(&token).await?;
    let about = client.about().await.ok();
    let vip = client.vip().await.ok();

    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sub": session.sub,
                "profile": session.profile.name,
                "quota_limit": about.as_ref().map(|about| about.quota.limit),
                "quota_usage": about.as_ref().map(|about| about.quota.usage),
                "vip_type": vip.as_ref().map(|vip| vip.data.vip_type.clone()),
            }))?
        );
        return Ok(0);
    }

    ui::status(
        output.quiet,
        format!(
            "authenticated as {} using the {} client",
            if session.sub.is_empty() {
                "an unknown user"
            } else {
                &session.sub
            },
            session.profile.name
        ),
    );
    if let Some(about) = about {
        ui::status(
            output.quiet,
            format!(
                "drive quota: {} of {} used",
                format_size(about.quota.usage),
                format_size(about.quota.limit)
            ),
        );
    }
    ui::status(
        output.quiet,
        "the rotated refresh token was written to the local .env file",
    );
    Ok(0)
}

fn read_stdin() -> Result<String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("cannot read standard input")?;
    let token = extract_token(&input).ok_or_else(|| anyhow!("no refresh token found on stdin"))?;
    Ok(token)
}

#[cfg(feature = "clipboard")]
fn read_clipboard() -> Result<String> {
    let mut clipboard = arboard::Clipboard::new().context("cannot open the system clipboard")?;
    let text = clipboard
        .get_text()
        .context("the clipboard does not contain text")?;
    extract_token(&text).ok_or_else(|| anyhow!("no refresh token found in the clipboard"))
}

#[cfg(not(feature = "clipboard"))]
fn read_clipboard() -> Result<String> {
    bail!("this build has no clipboard support; use --token or --stdin")
}

/// Accept the token itself, a line containing it, or the browser console
/// expression that copies it.
fn extract_token(text: &str) -> Option<String> {
    let mut candidates = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_matches('\'').trim_matches('"');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("os.") {
            if rest.len() >= 16
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            {
                candidates.push(line.to_string());
            }
        } else if line.len() >= 40
            && line
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            && !line.contains("copy(")
            && !line.contains("console.")
        {
            candidates.push(line.to_string());
        }
    }

    candidates.into_iter().max_by_key(String::len)
}

#[cfg(test)]
mod tests {
    use super::extract_token;

    #[test]
    fn finds_token_lines() {
        let text =
            "copy(JSON.parse(localStorage...).refresh_token)\nos.abcDEF1234567890_xyz-123456\n";
        assert_eq!(
            extract_token(text).as_deref(),
            Some("os.abcDEF1234567890_xyz-123456")
        );
    }

    #[test]
    fn ignores_console_code() {
        assert_eq!(extract_token("console.log('hi')"), None);
    }
}
