use anyhow::{bail, Result};
use rand::RngCore;
use std::path::{Path, PathBuf};
use url::Url;

pub fn expand_tilde(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = text.strip_prefix("~/").unwrap_or("");
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Share {
        share_id: String,
        file_id: Option<String>,
        pass_code: Option<String>,
    },
    Drive(String),
}

impl Target {
    pub fn is_share(&self) -> bool {
        matches!(self, Self::Share { .. })
    }
}

pub fn parse_target(raw: &str, pass_code_flag: Option<String>) -> Result<Target> {
    let raw = raw.trim().trim_matches('\'').trim_matches('"');
    if raw.is_empty() {
        bail!("empty target");
    }

    if raw.starts_with('/') {
        return Ok(Target::Drive(raw.to_string()));
    }

    let normalized = if !raw.contains("://") && raw.contains("mypikpak.") {
        format!("https://{raw}")
    } else {
        raw.to_string()
    };

    if let Ok(url) = Url::parse(&normalized) {
        if !matches!(url.scheme(), "http" | "https") {
            bail!("unsupported URL scheme: {}", url.scheme());
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if !(host == "mypikpak.com"
            || host.ends_with(".mypikpak.com")
            || host == "mypikpak.net"
            || host.ends_with(".mypikpak.net"))
        {
            bail!("not a PikPak URL: {raw}");
        }

        let segments: Vec<&str> = url
            .path_segments()
            .map(|parts| parts.filter(|part| !part.is_empty()).collect())
            .unwrap_or_default();

        if segments.first().copied() == Some("s") {
            let share_id = segments
                .get(1)
                .copied()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("share URL has no share id"))?;
            let file_id = segments.get(2).copied().filter(|id| !id.is_empty());
            let pass_code = pass_code_flag.or_else(|| {
                url.query_pairs()
                    .find(|(key, _)| key == "p" || key == "pass_code")
                    .map(|(_, value)| value.into_owned())
                    .filter(|value| !value.is_empty())
            });
            return Ok(Target::Share {
                share_id: share_id.to_string(),
                file_id: file_id.map(str::to_string),
                pass_code,
            });
        }

        bail!("unsupported PikPak URL; expected a share link like https://mypikpak.com/s/<id>");
    }

    // Raw share id, or a bare file id. Share ids are the common copy/paste
    // case; drive files can always be selected with a /path instead.
    if is_identifier(raw) {
        return Ok(Target::Share {
            share_id: raw.to_string(),
            file_id: None,
            pass_code: pass_code_flag,
        });
    }

    bail!("cannot understand target: {raw}")
}

fn is_identifier(value: &str) -> bool {
    value.len() >= 16
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').trim();
    let base = if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_string()
    } else {
        trimmed.to_string()
    };

    // Preserve an extension and bound the resulting UTF-8 byte length.
    let (stem, extension) = match base.rfind('.') {
        Some(index) if index > 0 => (&base[..index], &base[index..]),
        _ => (base.as_str(), ""),
    };

    let mut result = String::new();
    let limit = 180usize;
    let extension_bytes = extension.len();
    for ch in stem.chars() {
        if result.len() + ch.len_utf8() + extension_bytes > limit {
            break;
        }
        result.push(ch);
    }
    if result.is_empty() {
        result.push('f');
        result.push('i');
        result.push('l');
        result.push('e');
    }

    let mut candidate = format!("{result}{extension}");

    // Windows reserves certain file names regardless of extension. The
    // alteration goes in the stem, before the dot; `CON.txt_` is still
    // treated specially by Windows.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&result.to_ascii_uppercase().as_str()) {
        candidate = format!("{}_{extension}", result)
            .trim_end_matches(|c: char| c.is_whitespace())
            .to_string();
        if candidate.len() > 180 {
            // Remove the extension if necessary to preserve the limit on full
            // byte length. The trailing marker keeps the name from colliding.
            candidate = format!("{}_", result);
            candidate.truncate(180);
        }
    }

    candidate
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

pub fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_size(bytes_per_second.max(0.0) as u64))
}

pub fn md5_hex(input: &[u8]) -> String {
    hex::encode(md5::compute(input).0)
}

pub fn random_device_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    md5_hex(&bytes)
}

pub fn parse_content_range(value: &str) -> Result<(u64, u64, Option<u64>)> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range unit: {value}"))?;
    let (bounds, total) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range: {value}"))?;
    let (start, end) = bounds
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("invalid Content-Range bounds: {bounds}"))?;
    let start: u64 = start
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid Content-Range start: {start}"))?;
    let end: u64 = end
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid Content-Range end: {end}"))?;
    if end < start {
        bail!("invalid Content-Range: end {end} precedes start {start}");
    }
    let total = total
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|_| total.trim() != "*");
    Ok((start, end, total))
}

pub fn size_from_str(value: Option<&str>) -> u64 {
    value.and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// Parse sizes returned as numbers, strings, or absent.
pub fn de_optional_u64<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct Lenient;
    impl<'de> Visitor<'de> for Lenient {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a byte count as a number or decimal string")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Option<u64>, E> {
            Ok(Some(value))
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Option<u64>, E> {
            Ok(Some(value.max(0) as u64))
        }

        fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Option<u64>, E> {
            Ok(Some(value.max(0.0) as u64))
        }

        fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Option<u64>, E> {
            Ok(value.parse().ok())
        }

        fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Option<u64>, E> {
            self.visit_str(&value)
        }

        fn visit_none<E: de::Error>(self) -> std::result::Result<Option<u64>, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> std::result::Result<Option<u64>, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(Lenient)
}

pub fn de_lenient_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct Lenient;
    impl<'de> Visitor<'de> for Lenient {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a byte count as a number or decimal string")
        }

        fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<u64, E> {
            Ok(value)
        }

        fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<u64, E> {
            Ok(value.max(0) as u64)
        }

        fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<u64, E> {
            Ok(value.max(0.0) as u64)
        }

        fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<u64, E> {
            Ok(value.parse().unwrap_or(0))
        }

        fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<u64, E> {
            self.visit_str(&value)
        }

        fn visit_none<E: de::Error>(self) -> std::result::Result<u64, E> {
            Ok(0)
        }

        fn visit_unit<E: de::Error>(self) -> std::result::Result<u64, E> {
            Ok(0)
        }
    }

    deserializer.deserialize_any(Lenient)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_share_urls_and_pass_codes() {
        let target = parse_target("https://mypikpak.com/s/ABC123/file456?p=xyz", None).unwrap();
        assert_eq!(
            target,
            Target::Share {
                share_id: "ABC123".into(),
                file_id: Some("file456".into()),
                pass_code: Some("xyz".into()),
            }
        );
        let target = parse_target("mypikpak.net/s/ABC123", None).unwrap();
        assert!(target.is_share());
    }

    #[test]
    fn parses_drive_paths() {
        assert_eq!(
            parse_target("/Movies/a.mkv", None).unwrap(),
            Target::Drive("/Movies/a.mkv".into())
        );
    }

    #[test]
    fn rejects_unrelated_urls() {
        assert!(parse_target("https://example.com/s/ABC", None).is_err());
    }

    #[test]
    fn sanitizes_path_characters() {
        assert_eq!(sanitize_filename("a/b\\c:d*e?f"), "a_b_c_d_e_f");
        assert_eq!(sanitize_filename(".."), "file");
        assert_eq!(sanitize_filename(""), "file");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1024), "1.00 KiB");
        assert_eq!(format_size(1024 * 1024 * 3), "3.00 MiB");
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 10-19/100").unwrap(),
            (10, 19, Some(100))
        );
        assert_eq!(parse_content_range("bytes 0-0/*").unwrap(), (0, 0, None));
    }
}
