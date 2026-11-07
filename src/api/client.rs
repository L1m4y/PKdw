use crate::api::models::*;
use crate::api::profiles::{
    captcha_sign, jwt_aud, profile_by_client_id, Profile, ANDROID, PROFILES,
};
use crate::config::{ConfigFile, KEY_DEVICE_ID, KEY_PROXY, KEY_REFRESH_TOKEN};
use crate::paths::Paths;
use crate::util::random_device_id;
use anyhow::{anyhow, bail, Context, Result};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const DEFAULT_AUTH_BASE: &str = "https://user.mypikpak.com";
const DEFAULT_DRIVE_BASE: &str = "https://api-drive.mypikpak.com";
const CAPTCHA_TTL: Duration = Duration::from_secs(30 * 60);
const ACCESS_REFRESH_MARGIN: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct Session {
    pub access_token: String,
    pub refresh_token: String,
    pub sub: String,
    pub profile: Profile,
    pub expires_at: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("sub", &self.sub)
            .field("profile", &self.profile.name)
            .field(
                "expires_in",
                &self.expires_at.saturating_duration_since(Instant::now()),
            )
            .finish_non_exhaustive()
    }
}

struct CachedCaptcha {
    token: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct PikPak {
    http: reqwest::Client,
    paths: Paths,
    config: Arc<Mutex<ConfigFile>>,
    device_id: String,
    session: Arc<Mutex<Option<Session>>>,
    captcha: Arc<Mutex<HashMap<String, CachedCaptcha>>>,
    auth_base: String,
    drive_base: String,
}

impl PikPak {
    pub async fn new(paths: Paths) -> Result<Self> {
        let mut config = ConfigFile::load(paths.env_file.clone())?;

        let device_id = match config.get_with_env(KEY_DEVICE_ID) {
            Some(value) if !value.trim().is_empty() => value,
            _ => {
                let value = random_device_id();
                config.set(KEY_DEVICE_ID, &value);
                config.save()?;
                value
            }
        };

        let proxy = config.get_with_env(KEY_PROXY);
        let mut builder = reqwest::Client::builder()
            .user_agent("ANDROID-com.pikcloud.pikpak/1.21.0")
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(300))
            .pool_max_idle_per_host(64)
            .redirect(reqwest::redirect::Policy::limited(10));

        if let Some(proxy) = proxy.filter(|value| !value.trim().is_empty()) {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.trim())
                    .with_context(|| format!("invalid proxy URL in {KEY_PROXY}"))?,
            );
        }

        let http = builder.build().context("cannot build the HTTP client")?;
        let auth_base =
            std::env::var("PIKPAK_AUTH_BASE_URL").unwrap_or_else(|_| DEFAULT_AUTH_BASE.to_string());
        let drive_base = std::env::var("PIKPAK_DRIVE_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_DRIVE_BASE.to_string());

        Ok(Self {
            http,
            paths,
            config: Arc::new(Mutex::new(config)),
            device_id,
            session: Arc::new(Mutex::new(None)),
            captcha: Arc::new(Mutex::new(HashMap::new())),
            auth_base,
            drive_base,
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub async fn config_file(&self) -> ConfigFile {
        self.config.lock().await.clone()
    }

    pub async fn has_stored_token(&self) -> bool {
        let config = self.config.lock().await;
        config
            .get_with_env(KEY_REFRESH_TOKEN)
            .is_some_and(|token| !token.trim().is_empty() && token != "your_refresh_token")
    }

    pub async fn session(&self) -> Result<Option<Session>> {
        if let Some(session) = self.session.lock().await.as_ref() {
            if Instant::now() + ACCESS_REFRESH_MARGIN < session.expires_at {
                return Ok(Some(session.clone()));
            }
        }

        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_ref() {
            if Instant::now() + ACCESS_REFRESH_MARGIN < session.expires_at {
                return Ok(Some(session.clone()));
            }
        }

        let refresh_token = {
            let config = self.config.lock().await;
            config
                .get_with_env(KEY_REFRESH_TOKEN)
                .filter(|token| !token.trim().is_empty() && token != "your_refresh_token")
        };
        let Some(refresh_token) = refresh_token else {
            return Ok(None);
        };

        let session = self.exchange_refresh_token(&refresh_token).await?;
        *guard = Some(session.clone());
        Ok(Some(session))
    }

    pub async fn require_session(&self) -> Result<Session> {
        self.session()
            .await?
            .ok_or_else(|| anyhow!("not authenticated; run `pkdw auth --from-clipboard` first"))
    }

    pub async fn login_with_token(&self, token: &str) -> Result<Session> {
        let token = token.trim();
        if token.is_empty() {
            bail!("the refresh token is empty");
        }
        if token == "your_refresh_token" {
            bail!("that is the placeholder token, not a real refresh token");
        }

        let previous = {
            let config = self.config.lock().await;
            config.get(KEY_REFRESH_TOKEN).map(str::to_string)
        };
        {
            let mut config = self.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, token);
            config.save()?;
        }
        *self.session.lock().await = None;

        match self.exchange_refresh_token(token).await {
            Ok(session) => {
                *self.session.lock().await = Some(session.clone());
                Ok(session)
            }
            Err(error) => {
                // Do not replace a working token with one the server rejected.
                let mut config = self.config.lock().await;
                match previous {
                    Some(previous) => {
                        config.set(KEY_REFRESH_TOKEN, &previous);
                        let _ = config.save();
                    }
                    None => {
                        config.remove(KEY_REFRESH_TOKEN);
                        let _ = config.save();
                    }
                }
                Err(error)
            }
        }
    }

    pub async fn logout(&self) -> Result<()> {
        let mut config = self.config.lock().await;
        config.remove(KEY_REFRESH_TOKEN);
        config.save()?;
        *self.session.lock().await = None;
        Ok(())
    }

    async fn exchange_refresh_token(&self, refresh_token: &str) -> Result<Session> {
        let mut last_error: Option<anyhow::Error> = None;

        for profile in PROFILES {
            match self.try_exchange(refresh_token, *profile).await {
                Ok(mut session) => {
                    if !session.refresh_token.is_empty() {
                        let mut config = self.config.lock().await;
                        config.set(KEY_REFRESH_TOKEN, &session.refresh_token);
                        // A rotated token must be persisted before any other
                        // request can use or replace it.
                        if let Err(error) = config.save() {
                            eprintln!(
                                "warning: could not persist the rotated refresh token: {error:#}"
                            );
                        }
                    }
                    if let Some(aud) = jwt_aud(&session.access_token) {
                        if let Some(profile) = profile_by_client_id(&aud) {
                            session.profile = profile;
                        }
                    }
                    return Ok(session);
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("token refresh failed")))
    }

    async fn try_exchange(&self, refresh_token: &str, profile: Profile) -> Result<Session> {
        let url = format!("{}/v1/auth/token", self.auth_base);
        let response = self
            .http
            .post(&url)
            .header("X-Device-Id", &self.device_id)
            .json(&json!({
                "client_id": profile.client_id,
                "client_secret": profile.client_secret,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .with_context(|| format!("token refresh request failed ({})", profile.name))?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            if is_grant_rejection(&text) {
                bail!(
                    "PikPak rejected the refresh token for the {} client (invalid_grant). \
                     Refresh tokens are single-use: every login rotates them, so the web app \
                     or an older copy of pkdw may have already replaced this one. Copy a fresh \
                     token from the browser and run `pkdw auth --from-clipboard` again.",
                    profile.name
                );
            }
            bail!(
                "token refresh failed for the {} client (HTTP {status}): {}",
                profile.name,
                redact(&truncate(&text, 500), refresh_token)
            );
        }

        let token: TokenResponse =
            serde_json::from_str(&text).context("invalid token response JSON")?;
        if token.access_token.is_empty() {
            bail!("token refresh returned an empty access token");
        }
        let ttl = if token.expires_in == 0 {
            3600
        } else {
            token.expires_in
        };
        let profile = jwt_aud(&token.access_token)
            .and_then(|aud| profile_by_client_id(&aud))
            .unwrap_or(profile);

        Ok(Session {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            sub: token.sub,
            profile,
            expires_at: Instant::now() + Duration::from_secs(ttl),
        })
    }

    pub async fn refresh_rejected_session(&self, rejected_access: &str) -> Result<Option<Session>> {
        {
            let mut guard = self.session.lock().await;
            if guard
                .as_ref()
                .is_some_and(|session| session.access_token == rejected_access)
            {
                *guard = None;
            }
        }
        self.session().await
    }

    /// Cheap identity check for `whoami` before fetching quota.
    pub async fn about(&self) -> Result<AboutResponse> {
        self.authed_json(Method::GET, "/drive/v1/about", &[], None)
            .await
    }

    pub async fn vip(&self) -> Result<VipResponse> {
        self.authed_json(Method::GET, "/drive/v1/privilege/vip", &[], None)
            .await
    }

    pub async fn anon_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<T> {
        let value = self.anon_request(Method::GET, path, params, None).await?;
        serde_json::from_value(value).context("invalid JSON from PikPak")
    }

    pub async fn authed_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        body: Option<Value>,
    ) -> Result<T> {
        let value = self.authed_request(method, path, params, body).await?;
        serde_json::from_value(value).context("invalid JSON from PikPak")
    }

    pub async fn anon_request(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let action = format!("{method}:{path}");
        let mut retried = false;
        loop {
            let captcha = self.captcha(&action, None).await?;
            let url = format!("{}{}", self.drive_base, path);
            let mut request = self
                .http
                .request(method.clone(), &url)
                .query(params)
                .header("X-Device-Id", &self.device_id)
                .header("X-Captcha-Token", captcha)
                .header("x-client-id", ANDROID.client_id)
                .header("Accept", "application/json");
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.context("PikPak API request failed")?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let value = parse_json_or_text(&text);
            if status.is_success() {
                return Ok(value);
            }
            if is_captcha_error(&value) && !retried {
                retried = true;
                self.invalidate_captcha(&action).await;
                continue;
            }
            return Err(api_error("PikPak API", status, &value));
        }
    }

    pub async fn authed_request(
        &self,
        method: Method,
        path: &str,
        params: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let action = format!("{method}:{path}");
        let mut retried_captcha = false;
        let mut retried_auth = false;

        loop {
            let session = self.require_session().await?;
            let captcha = self.captcha(&action, Some(&session)).await?;
            let url = format!("{}{}", self.drive_base, path);
            let mut request = self
                .http
                .request(method.clone(), &url)
                .query(params)
                .bearer_auth(&session.access_token)
                .header("X-Device-Id", &self.device_id)
                .header("X-Captcha-Token", captcha)
                .header("x-client-id", session.profile.client_id)
                .header("Accept", "application/json");
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.context("PikPak API request failed")?;
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            let value = parse_json_or_text(&text);

            if status.is_success() {
                return Ok(value);
            }
            if is_captcha_error(&value) && !retried_captcha {
                retried_captcha = true;
                self.invalidate_captcha(&action).await;
                continue;
            }
            if is_auth_error(status, &value) && !retried_auth {
                retried_auth = true;
                self.refresh_rejected_session(&session.access_token).await?;
                continue;
            }
            return Err(api_error("PikPak API", status, &value));
        }
    }

    pub async fn captcha(&self, action: &str, session: Option<&Session>) -> Result<String> {
        let key = format!(
            "{}|{}|{}",
            action,
            session
                .map(|s| s.profile.client_id)
                .unwrap_or(ANDROID.client_id),
            session
                .map(|s| s.access_token.chars().take(12).collect::<String>())
                .unwrap_or_default()
        );

        if let Some(cached) = self.captcha.lock().await.get(&key) {
            if Instant::now() < cached.expires_at {
                return Ok(cached.token.clone());
            }
        }

        let profile = session.map(|s| s.profile).unwrap_or(ANDROID);
        let timestamp = now_millis().to_string();
        let sign = captcha_sign(&profile, &self.device_id, &timestamp);
        let mut body = json!({
            "action": action,
            "captcha_token": "",
            "client_id": profile.client_id,
            "device_id": self.device_id,
            "meta": {
                "captcha_sign": sign,
                "user_id": session.map(|s| s.sub.as_str()).unwrap_or(""),
                "package_name": profile.package,
                "client_version": profile.version,
                "timestamp": timestamp,
            },
        });
        if let Some(redirect_uri) = profile.redirect_uri {
            body["redirect_uri"] = json!(redirect_uri);
        }

        let url = format!(
            "{}/v1/shield/captcha/init?client_id={}",
            self.auth_base, profile.client_id
        );
        let mut request = self
            .http
            .post(&url)
            .header("X-Device-Id", &self.device_id)
            .json(&body);
        if let Some(session) = session {
            request = request.bearer_auth(&session.access_token);
        }
        let response = request.send().await.context("captcha request failed")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        let value = parse_json_or_text(&text);
        if !status.is_success() {
            return Err(api_error("captcha init", status, &value));
        }
        let captcha: CaptchaInitResponse =
            serde_json::from_value(value).context("invalid captcha response")?;
        if captcha.captcha_token.is_empty() {
            let hint = if captcha.url.is_empty() {
                "the server wants human verification"
            } else {
                "the server returned a human-verification URL"
            };
            bail!("no captcha token: {hint}; open PikPak in a browser and try again");
        }

        let ttl = if captcha.expires_in == 0 {
            CAPTCHA_TTL
        } else {
            Duration::from_secs(captcha.expires_in.min(30 * 60))
        };
        let token = captcha.captcha_token.clone();
        self.captcha.lock().await.insert(
            key,
            CachedCaptcha {
                token: token.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(token)
    }

    pub async fn invalidate_captcha(&self, action: &str) {
        let mut cache = self.captcha.lock().await;
        cache.retain(|key, _| !key.starts_with(&format!("{action}|")));
    }

    pub fn auth_base(&self) -> &str {
        &self.auth_base
    }

    pub fn drive_base(&self) -> &str {
        &self.drive_base
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn parse_json_or_text(text: &str) -> Value {
    if text.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(text).unwrap_or_else(|_| json!({ "raw": truncate(text, 800) }))
}

fn api_error(context: &str, status: StatusCode, value: &Value) -> anyhow::Error {
    let code = value.get("error_code").and_then(Value::as_u64);
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = value
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let raw = value.get("raw").and_then(Value::as_str).unwrap_or_default();

    let detail = if !description.is_empty() {
        description
    } else if !error.is_empty() {
        error
    } else if !raw.is_empty() {
        raw
    } else {
        "no response body"
    };

    let hint = match code {
        Some(9) => Some("captcha expired or the request was rate-limited; retry in a moment"),
        Some(4126) => Some("the refresh token is dead; copy a new one from the browser"),
        _ if error.eq_ignore_ascii_case("invalid_grant") => {
            Some("the refresh token is dead; copy a new one from the browser")
        }
        _ if detail.contains("need_more_quota") => {
            Some("the file needs more quota; use a smaller quality or free drive space")
        }
        _ if detail.contains("file_space_not_enough") => {
            Some("not enough drive space to copy this share")
        }
        _ if status == StatusCode::TOO_MANY_REQUESTS => Some("too many requests; wait a moment"),
        _ if status.is_server_error() => Some("PikPak is having trouble; retry shortly"),
        _ => None,
    };

    match (code, hint) {
        (Some(code), Some(hint)) => {
            anyhow!("{context} failed (HTTP {status}, error_code {code}): {detail} — {hint}")
        }
        (None, Some(hint)) => anyhow!("{context} failed (HTTP {status}): {detail} — {hint}"),
        _ => anyhow!("{context} failed (HTTP {status}): {detail}"),
    }
}

fn is_captcha_error(value: &Value) -> bool {
    value.get("error_code").and_then(Value::as_u64) == Some(9)
}

fn is_auth_error(status: StatusCode, value: &Value) -> bool {
    if status == StatusCode::UNAUTHORIZED {
        return true;
    }
    let error = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default();
    error.eq_ignore_ascii_case("unauthenticated")
        || error.eq_ignore_ascii_case("invalid_grant")
        || value.get("error_code").and_then(Value::as_u64) == Some(4126)
}

fn is_grant_rejection(body: &str) -> bool {
    serde_json::from_str::<Value>(body)
        .ok()
        .is_some_and(|value| {
            value.get("error").and_then(Value::as_str) == Some("invalid_grant")
                || value.get("error_code").and_then(Value::as_u64) == Some(4126)
        })
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn redact(value: &str, secret: &str) -> String {
    if secret.is_empty() {
        value.to_string()
    } else {
        value.replace(secret, "[redacted]")
    }
}

/// A cheap handle that can be cloned into concurrent download workers.
pub type SharedClient = Arc<PikPak>;
