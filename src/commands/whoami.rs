use crate::api::PikPak;
use crate::cli::OutputArgs;
use crate::ui;
use crate::util::format_size;
use anyhow::Result;
use serde_json::json;

pub async fn run(client: &PikPak, output: &OutputArgs) -> Result<i32> {
    let session = client.require_session().await?;
    let about = client.about().await?;
    let vip = client.vip().await.ok();

    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "sub": session.sub,
                "profile": session.profile.name,
                "device_id": client.device_id(),
                "quota": {
                    "limit": about.quota.limit,
                    "usage": about.quota.usage,
                    "usage_in_trash": about.quota.usage_in_trash,
                },
                "vip": vip.as_ref().map(|vip| json!({
                    "type": vip.data.vip_type,
                    "status": vip.data.status,
                    "expire": vip.data.expire,
                })),
            }))?
        );
        return Ok(0);
    }

    if !output.quiet {
        let used = about.quota.usage;
        let limit = about.quota.limit;
        let percent = used.saturating_mul(100).checked_div(limit).unwrap_or(0);
        ui::info_line(
            "user",
            if session.sub.is_empty() {
                "?"
            } else {
                &session.sub
            },
        );
        ui::info_line("profile", session.profile.name);
        ui::info_line("device id", client.device_id());
        ui::info_line(
            "drive quota",
            format!(
                "{} of {} used ({percent}%)",
                format_size(used),
                format_size(limit)
            ),
        );
        if let Some(vip) = vip {
            if !vip.data.vip_type.is_empty() || !vip.data.status.is_empty() {
                ui::info_line(
                    "vip",
                    format!(
                        "{} / {}{}",
                        vip.data.vip_type,
                        vip.data.status,
                        if vip.data.expire.is_empty() {
                            String::new()
                        } else {
                            format!(" (expires {})", vip.data.expire)
                        }
                    ),
                );
            }
        }
    }
    Ok(0)
}
