use crate::cli::Quality;
use crate::util::{de_lenient_u64, format_size};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct ShareResponse {
    #[serde(default)]
    pub share_status: String,
    #[serde(default)]
    pub share_status_text: String,
    #[serde(default)]
    pub pass_code_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShareDetailResponse {
    #[serde(default)]
    pub files: Vec<ShareEntry>,
    #[serde(default)]
    pub next_page_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShareEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub size: u64,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub phase: String,
}

impl ShareEntry {
    pub fn is_folder(&self) -> bool {
        self.kind.ends_with("folder")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaLink {
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaVideo {
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub media_name: String,
    #[serde(default)]
    pub resolution_name: String,
    #[serde(default)]
    pub is_origin: bool,
    #[serde(default)]
    pub is_visible: Option<bool>,
    #[serde(default)]
    pub need_more_quota: Option<bool>,
    #[serde(default)]
    pub link: Option<MediaLink>,
    #[serde(default)]
    pub video: Option<MediaVideo>,
}

impl Media {
    pub fn url(&self) -> Option<&str> {
        self.link
            .as_ref()
            .map(|link| link.url.as_str())
            .filter(|url| !url.is_empty())
    }

    pub fn label(&self) -> &str {
        if self.is_origin || self.media_name.eq_ignore_ascii_case("original") {
            "original"
        } else if !self.media_name.is_empty() {
            &self.media_name
        } else {
            &self.resolution_name
        }
    }

    fn height(&self) -> Option<u32> {
        self.video.as_ref().and_then(|video| video.height)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub size: u64,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub web_content_link: String,
    #[serde(default)]
    pub medias: Vec<Media>,
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct SelectedMedia {
    pub url: String,
    pub label: String,
    pub is_original: bool,
}

impl FileInfo {
    pub fn is_folder(&self) -> bool {
        self.kind.ends_with("folder")
    }

    /// Pick a stream by quality. Unknown or lower-quality requests fall back to
    /// the best available visible stream, never to a stream the server marked
    /// as quota-restricted.
    pub fn select_media(&self, quality: Quality) -> Option<SelectedMedia> {
        let visible = |media: &&Media| {
            media.is_visible != Some(false)
                && media.need_more_quota != Some(true)
                && media.url().is_some()
        };

        if let Some(media) = self
            .medias
            .iter()
            .filter(visible)
            .find(|media| media.is_origin || media.media_name.eq_ignore_ascii_case("original"))
        {
            if quality == Quality::Original {
                return Some(SelectedMedia {
                    url: media.url()?.to_string(),
                    label: "original".to_string(),
                    is_original: true,
                });
            }
        }

        if quality == Quality::Original && !self.web_content_link.is_empty() {
            return Some(SelectedMedia {
                url: self.web_content_link.clone(),
                label: "original".to_string(),
                is_original: true,
            });
        }

        let target = match quality {
            Quality::Original => None,
            Quality::P1080 => Some(1080),
            Quality::P720 => Some(720),
            Quality::P480 => Some(480),
        };

        let mut candidates: Vec<&Media> = self.medias.iter().filter(visible).collect();
        if let Some(target) = target {
            let exact = candidates.iter().copied().find(|media| {
                media.media_name.eq_ignore_ascii_case(&format!("{target}P"))
                    || media.height() == Some(target)
            });
            if let Some(media) = exact {
                return Some(SelectedMedia {
                    url: media.url()?.to_string(),
                    label: media.label().to_string(),
                    is_original: media.is_origin,
                });
            }

            // Nearest lower resolution, then nearest higher, then original.
            candidates.sort_by_key(|media| {
                let height = media.height().unwrap_or(u32::MAX);
                if height <= target {
                    (0, target - height)
                } else {
                    (1, height - target)
                }
            });
        } else {
            candidates.sort_by_key(|media| {
                (
                    !media.is_origin,
                    std::cmp::Reverse(media.height().unwrap_or(0)),
                )
            });
        }

        if let Some(media) = candidates.first() {
            return Some(SelectedMedia {
                url: media.url()?.to_string(),
                label: media.label().to_string(),
                is_original: media.is_origin,
            });
        }

        if !self.web_content_link.is_empty() {
            return Some(SelectedMedia {
                url: self.web_content_link.clone(),
                label: quality.key().to_string(),
                is_original: quality == Quality::Original,
            });
        }
        None
    }

    pub fn quota_blocked_original(&self) -> bool {
        self.medias.iter().any(|media| {
            (media.is_origin || media.media_name.eq_ignore_ascii_case("original"))
                && media.need_more_quota == Some(true)
        })
    }

    pub fn available_qualities(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .medias
            .iter()
            .filter(|media| media.is_visible != Some(false) && media.url().is_some())
            .map(|media| {
                let mut label = media.label().to_string();
                if let Some(height) = media.height() {
                    if !label.eq_ignore_ascii_case(&format!("{height}p")) && height > 0 {
                        label = format!("{label} ({height}p)");
                    }
                }
                if media.need_more_quota == Some(true) {
                    label.push_str(" [quota]");
                }
                label
            })
            .collect();
        labels.dedup();
        labels
    }

    pub fn describe(&self) -> String {
        let mut text = format!("{}  {}", self.name, format_size(self.size));
        let qualities = self.available_qualities();
        if !qualities.is_empty() {
            text.push_str(&format!("  [{}]", qualities.join(", ")));
        }
        if self.quota_blocked_original() {
            text.push_str("  original requires more quota");
        }
        text
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileInfoEnvelope {
    #[serde(default)]
    pub file_info: Option<FileInfo>,
}

impl FileInfoEnvelope {
    pub fn into_info(self) -> Option<FileInfo> {
        self.file_info
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DriveFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub size: u64,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub web_content_link: String,
    #[serde(default)]
    pub parent_id: String,
    #[serde(default)]
    pub modified_time: String,
    #[serde(default)]
    pub phase: String,
}

impl DriveFile {
    pub fn is_folder(&self) -> bool {
        self.kind.ends_with("folder")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DriveListResponse {
    #[serde(default)]
    pub files: Vec<DriveFile>,
    #[serde(default)]
    pub next_page_token: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Quota {
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub limit: u64,
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub usage: u64,
    #[serde(default, deserialize_with = "de_lenient_u64")]
    pub usage_in_trash: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AboutResponse {
    #[serde(default)]
    pub quota: Quota,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VipData {
    #[serde(default, rename = "type")]
    pub vip_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub expire: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct VipResponse {
    #[serde(default)]
    pub data: VipData,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub sub: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaptchaInitResponse {
    #[serde(default)]
    pub captcha_token: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TaskResponse {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub progress: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RestoreResponse {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> FileInfo {
        serde_json::from_value(serde_json::json!({
            "name": "movie.mkv",
            "size": "6110009965",
            "web_content_link": "https://example.invalid/slow",
            "medias": [
                {"media_name":"Original","is_origin":true,"link":{"url":"https://example.invalid/original"}},
                {"media_name":"1080P","video":{"height":1080},"link":{"url":"https://example.invalid/1080"}},
                {"media_name":"720P","video":{"height":720},"link":{"url":"https://example.invalid/720"}},
                {"media_name":"480P","need_more_quota":true,"link":{"url":"https://example.invalid/480"}}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn selects_original_and_transcodes() {
        let info = info();
        assert_eq!(
            info.select_media(Quality::Original).unwrap().url,
            "https://example.invalid/original"
        );
        assert_eq!(
            info.select_media(Quality::P1080).unwrap().url,
            "https://example.invalid/1080"
        );
        assert_eq!(
            info.select_media(Quality::P720).unwrap().url,
            "https://example.invalid/720"
        );
        // The 480p stream is quota-blocked; the nearest allowed stream is 720p.
        assert_eq!(
            info.select_media(Quality::P480).unwrap().url,
            "https://example.invalid/720"
        );
    }

    #[test]
    fn parses_string_sizes() {
        assert_eq!(info().size, 6_110_009_965);
    }
}
