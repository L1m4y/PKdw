use base64::Engine;
use serde::Deserialize;

/// The Android client used by the official app and most third-party clients.
pub const ANDROID_SALTS: &[&str] = &[
    "",
    "E32cSkYXC2bciKJGxRsE8ZgwmH/YwkvpD6/O9guSOa2irCwciH4xPHaH",
    "QtqgfMgHP2TFl",
    "zOKgHT56L7nIzFzDpUGhpWFrgP53m3G6ML",
    "S",
    "THxpsktzfFXizUv7DK1y/N7NZ1WhayViluBEvAJJ8bA1Wr6",
    "y9PXH3xGUhG/zQI8CaapRw2LhldCaFM9CRlKpZXJvj+pifu",
    "+RaaG7T8FRTI4cP019N5y9ofLyHE9ySFUr",
    "6Pf1l8UTeuzYldGtb/d",
];

/// Salts extracted from the mypikpak.com web bundle. Tokens issued by the web
/// client (`aud` = YUMx5nI8ZU8Ap8pm) must use this chain and package name.
pub const WEB_SALTS: &[&str] = &[
    "fyZ4+p77W1U4zcWBUwefAIFhFxvADWtT1wzolCxhg9q7etmGUjXr",
    "uSUX02HYJ1IkyLdhINEFcCf7l2",
    "iWt97bqD/qvjIaPXB2Ja5rsBWtQtBZZmaHH2rMR41",
    "3binT1s/5a1pu3fGsN",
    "8YCCU+AIr7pg+yd7CkQEY16lDMwi8Rh4WNp5",
    "DYS3StqnAEKdGddRP8CJrxUSFh",
    "crquW+4",
    "ryKqvW9B9hly+JAymXCIfag5Z",
    "Hr08T/NDTX1oSJfHk90c",
    "i",
];

#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub name: &'static str,
    pub client_id: &'static str,
    pub client_secret: &'static str,
    pub version: &'static str,
    pub package: &'static str,
    pub salts: &'static [&'static str],
    pub redirect_uri: Option<&'static str>,
}

pub const ANDROID: Profile = Profile {
    name: "android",
    client_id: "YNxT9w7GMdWvEOKa",
    client_secret: "dbw2OtmVEeuUvIptb1Coyg",
    version: "1.21.0",
    package: "com.pikcloud.pikpak",
    salts: ANDROID_SALTS,
    redirect_uri: Some("ttps://api.mypikpak.com/v1/auth/callback"),
};

pub const WEB: Profile = Profile {
    name: "web",
    client_id: "YUMx5nI8ZU8Ap8pm",
    client_secret: "",
    version: "undefined",
    package: "drive.mypikpak.com",
    salts: WEB_SALTS,
    redirect_uri: None,
};

pub const PROFILES: &[Profile] = &[ANDROID, WEB];

pub fn profile_by_client_id(client_id: &str) -> Option<Profile> {
    PROFILES
        .iter()
        .copied()
        .find(|profile| profile.client_id == client_id)
}

/// Read the `aud` claim from a JWT access token without verifying the
/// signature. The server remains the authority; this only selects the right
/// captcha signature table.
pub fn jwt_aud(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        aud: serde_json::Value,
    }
    let payload: Payload = serde_json::from_slice(&bytes).ok()?;
    match payload.aud {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Array(values) => values
            .into_iter()
            .find_map(|value| value.as_str().map(str::to_string)),
        _ => None,
    }
}

pub fn captcha_sign(profile: &Profile, device_id: &str, timestamp_ms: &str) -> String {
    let mut state = format!(
        "{}{}{}{}{}",
        profile.client_id, profile.version, profile.package, device_id, timestamp_ms
    );
    for salt in profile.salts {
        state = crate::util::md5_hex(format!("{state}{salt}").as_bytes());
    }
    format!("1.{state}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_is_deterministic_and_prefixed() {
        let a = captcha_sign(&ANDROID, "device", "1700000000000");
        let b = captcha_sign(&ANDROID, "device", "1700000000000");
        assert_eq!(a, b);
        assert!(a.starts_with("1."));
        assert_eq!(a.len(), 34);
        assert_ne!(a, captcha_sign(&WEB, "device", "1700000000000"));
    }

    #[test]
    fn detects_profile_from_client_id() {
        assert_eq!(
            profile_by_client_id(ANDROID.client_id).unwrap().name,
            "android"
        );
        assert_eq!(profile_by_client_id(WEB.client_id).unwrap().name, "web");
        assert!(profile_by_client_id("nope").is_none());
    }

    #[test]
    fn reads_aud_from_jwt() {
        // Header and signature are irrelevant for profile selection.
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"aud":"YNxT9w7GMdWvEOKa","sub":"user"}"#);
        let token = format!("e30.{payload}.sig");
        assert_eq!(jwt_aud(&token).as_deref(), Some(ANDROID.client_id));
    }
}
