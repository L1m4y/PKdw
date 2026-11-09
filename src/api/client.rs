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

/// Typed failures of the single-use refresh-token rotation.
#[derive(Debug)]
pub enum RefreshError {
    /// The server explicitly rejected the token before consuming it. Rolling
    /// back to the previously stored token is safe in this case.
    RejectedBeforeRotation { message: String },
    /// The server accepted the token and returned a new one, but it could not
    /// be written to disk. The previous token may already be dead, so it must
    /// never be restored.
    RotatedButPersistenceFailed {
        session: Box<Session>,
        source: anyhow::Error,
    },
    /// A transient exchange failure (network, 5xx, malformed response). There
    /// is no proof that the server rejected or rotated the token.
    Failed { source: anyhow::Error },
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RejectedBeforeRotation { message } => f.write_str(message),
            Self::RotatedButPersistenceFailed { source, .. } => write!(
                f,
                "PikPak rotated the refresh token but it could not be saved locally: {source:#}. \
                 The new session is kept in memory; fix the configuration path and retry saving it."
            ),
            Self::Failed { source } => write!(f, "{source:#}"),
        }
    }
}

impl std::error::Error for RefreshError {}

#[derive(Clone)]
pub struct PikPak {
    http: reqwest::Client,
    http_range: reqwest::Client,
    paths: Paths,
    config: Arc<Mutex<ConfigFile>>,
    device_id: String,
    session: Arc<Mutex<Option<Session>>>,
    captcha: Arc<Mutex<HashMap<String, CachedCaptcha>>>,
    auth_base: String,
    drive_base: String,
    auth_lock: Arc<Mutex<()>>,
    config_lock_path: std::path::PathBuf,
    persistence_pending: Arc<Mutex<bool>>,
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

        fn build_client(proxy: Option<String>, no_gzip: bool) -> Result<reqwest::Client> {
            let mut builder = reqwest::Client::builder()
                .user_agent("ANDROID-com.pikcloud.pikpak/1.21.0")
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(300))
                .pool_max_idle_per_host(64)
                .redirect(reqwest::redirect::Policy::limited(10));
            if no_gzip {
                builder = builder.no_gzip();
            }
            if let Some(proxy) = proxy
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
            {
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy.trim())
                        .with_context(|| format!("invalid proxy URL in {KEY_PROXY}"))?,
                );
            }
            builder.build().context("cannot build the HTTP client")
        }

        let http = build_client(proxy.clone(), false)?;
        let http_range = build_client(proxy, true)?;
        let auth_base =
            std::env::var("PIKPAK_AUTH_BASE_URL").unwrap_or_else(|_| DEFAULT_AUTH_BASE.to_string());
        let drive_base = std::env::var("PIKPAK_DRIVE_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_DRIVE_BASE.to_string());

        let config_lock_path = crate::fsutil::lock_path(&paths.env_file);
        Ok(Self {
            http,
            http_range,
            paths,
            config: Arc::new(Mutex::new(config)),
            device_id,
            session: Arc::new(Mutex::new(None)),
            captcha: Arc::new(Mutex::new(HashMap::new())),
            auth_base,
            drive_base,
            auth_lock: Arc::new(Mutex::new(())),
            config_lock_path,
            persistence_pending: Arc::new(Mutex::new(false)),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn http_range(&self) -> &reqwest::Client {
        &self.http_range
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
        // Lock order: auth_lock -> config file lock -> config mutex. Never
        // hold the session mutex while awaiting a refresh.
        let _auth = self.auth_lock.lock().await;

        if let Some(session) = self.valid_session().await {
            if !*self.persistence_pending.lock().await {
                return Ok(Some(session));
            }
        }

        let _file_lock = crate::fsutil::FileLock::acquire(&self.config_lock_path)?;

        if *self.persistence_pending.lock().await {
            if let Err(error) = self.persist_pending_session().await {
                if let Some(session) = self.valid_session().await {
                    // The access token is still usable. Keep serving requests
                    // while the rotated refresh token awaits persistence.
                    return Ok(Some(session));
                }
                return Err(error);
            }
        }

        if let Some(session) = self.valid_session().await {
            return Ok(Some(session));
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

        match self.exchange_refresh_token(&refresh_token).await {
            Ok(session) => {
                *self.session.lock().await = Some(session.clone());
                Ok(Some(session))
            }
            Err(error @ RefreshError::RotatedButPersistenceFailed { .. }) => {
                if let RefreshError::RotatedButPersistenceFailed { session, .. } = &error {
                    *self.session.lock().await = Some(session.as_ref().clone());
                }
                *self.persistence_pending.lock().await = true;
                Err(anyhow::Error::new(error))
            }
            Err(error) => Err(anyhow::Error::new(error)),
        }
    }

    async fn valid_session(&self) -> Option<Session> {
        let guard = self.session.lock().await;
        guard
            .as_ref()
            .filter(|session| Instant::now() + ACCESS_REFRESH_MARGIN < session.expires_at)
            .cloned()
    }

    async fn persist_pending_session(&self) -> Result<()> {
        let token = self
            .session
            .lock()
            .await
            .as_ref()
            .map(|session| session.refresh_token.clone())
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            *self.persistence_pending.lock().await = false;
            return Ok(());
        };
        {
            let mut config = self.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, &token);
            config
                .save()
                .context("cannot persist the rotated refresh token")?;
        }
        *self.persistence_pending.lock().await = false;
        Ok(())
    }

    /// Retry writing a rotated token that could not be persisted earlier.
    pub async fn retry_persist_rotated_token(&self) -> Result<()> {
        let _auth = self.auth_lock.lock().await;
        let _file_lock = crate::fsutil::FileLock::acquire(&self.config_lock_path)?;
        self.persist_pending_session().await
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

        let _auth = self.auth_lock.lock().await;
        let _file_lock = crate::fsutil::FileLock::acquire(&self.config_lock_path)?;

        // Never rotate again while a previous rotation is still unsaved.
        if *self.persistence_pending.lock().await {
            self.persist_pending_session().await?;
        }

        let previous = {
            let config = self.config.lock().await;
            config.get(KEY_REFRESH_TOKEN).map(str::to_string)
        };
        {
            let mut config = self.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, token);
            if let Err(error) = config.save() {
                match &previous {
                    Some(previous) => config.set(KEY_REFRESH_TOKEN, previous),
                    None => config.remove(KEY_REFRESH_TOKEN),
                }
                return Err(error).context("cannot save the refresh token before login");
            }
        }
        *self.session.lock().await = None;

        match self.exchange_refresh_token(token).await {
            Ok(session) => {
                *self.session.lock().await = Some(session.clone());
                Ok(session)
            }
            Err(RefreshError::RejectedBeforeRotation { message }) => {
                // The server did not consume this token. Restoring the previous
                // one is safe and keeps a working login intact.
                let mut config = self.config.lock().await;
                match previous {
                    Some(previous) => {
                        config.set(KEY_REFRESH_TOKEN, &previous);
                        config.save().map_err(|save_error| {
                            anyhow!(
                                "could not restore the previous refresh token after a failed login: {save_error:#}"
                            )
                        })?;
                    }
                    None => {
                        config.remove(KEY_REFRESH_TOKEN);
                        config.save().map_err(|save_error| {
                            anyhow!(
                                "could not remove the rejected refresh token after a failed login: {save_error:#}"
                            )
                        })?;
                    }
                }
                Err(anyhow!(message))
            }
            Err(error @ RefreshError::RotatedButPersistenceFailed { .. }) => {
                if let RefreshError::RotatedButPersistenceFailed { session, .. } = &error {
                    // The new token is valid and lives in memory. Keep it so a
                    // later retry can persist it; never restore the old token.
                    *self.session.lock().await = Some(session.as_ref().clone());
                }
                *self.persistence_pending.lock().await = true;
                Err(anyhow::Error::new(error))
            }
            Err(error @ RefreshError::Failed { .. }) => {
                // A transient failure is not proof of rejection. Keep the
                // supplied token instead of resurrecting a possibly dead one.
                Err(anyhow::Error::new(error))
            }
        }
    }

    pub async fn logout(&self) -> Result<()> {
        let _auth = self.auth_lock.lock().await;
        let _file_lock = crate::fsutil::FileLock::acquire(&self.config_lock_path)?;
        // Lock order: auth -> file -> session -> config.
        let mut session = self.session.lock().await;
        let mut config = self.config.lock().await;
        config.remove(KEY_REFRESH_TOKEN);
        config.save()?;
        *session = None;
        drop(config);
        drop(session);
        *self.persistence_pending.lock().await = false;
        Ok(())
    }

    async fn exchange_refresh_token(
        &self,
        refresh_token: &str,
    ) -> std::result::Result<Session, RefreshError> {
        let mut last_rejection: Option<anyhow::Error> = None;
        let mut last_failure: Option<anyhow::Error> = None;

        for profile in PROFILES {
            match self.try_exchange(refresh_token, *profile).await {
                Ok(mut session) => {
                    if !session.refresh_token.is_empty() {
                        let mut config = self.config.lock().await;
                        config.set(KEY_REFRESH_TOKEN, &session.refresh_token);
                        // A rotated token must be persisted before any other
                        // request can use or replace it.
                        if let Err(source) = config.save() {
                            return Err(RefreshError::RotatedButPersistenceFailed {
                                session: Box::new(session),
                                source,
                            });
                        }
                    }
                    if let Some(aud) = jwt_aud(&session.access_token) {
                        if let Some(profile) = profile_by_client_id(&aud) {
                            session.profile = profile;
                        }
                    }
                    return Ok(session);
                }
                Err(ExchangeFailure::Rejected(error)) => {
                    last_rejection = Some(error);
                }
                Err(ExchangeFailure::Failed(error)) => {
                    last_failure = Some(error);
                }
            }
        }

        // A transient failure means the token may have been consumed without
        // us seeing the response. Do not claim it was rejected.
        if let Some(source) = last_failure {
            return Err(RefreshError::Failed { source });
        }
        if let Some(error) = last_rejection {
            return Err(RefreshError::RejectedBeforeRotation {
                message: format!("{error:#}"),
            });
        }
        Err(RefreshError::RejectedBeforeRotation {
            message: "token refresh failed".to_string(),
        })
    }

    async fn try_exchange(
        &self,
        refresh_token: &str,
        profile: Profile,
    ) -> std::result::Result<Session, ExchangeFailure> {
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
            .map_err(|error| {
                ExchangeFailure::Failed(anyhow!(
                    "token refresh request failed ({}): {error}",
                    profile.name
                ))
            })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = redact(&truncate(&text, 500), refresh_token);
            if is_grant_rejection(&text) || status == StatusCode::UNAUTHORIZED {
                return Err(ExchangeFailure::Rejected(anyhow!(
                    "PikPak rejected the refresh token for the {} client (invalid_grant). \
                     Refresh tokens are single-use: every login rotates them, so the web app \
                     or an older copy of pkdw may have already replaced this one. Copy a fresh \
                     token from the browser and run `pkdw auth --from-clipboard` again.",
                    profile.name
                )));
            }
            if status == StatusCode::FORBIDDEN {
                return Err(ExchangeFailure::Rejected(anyhow!(
                    "token refresh was denied for the {} client (HTTP {status}): {detail}",
                    profile.name
                )));
            }
            return Err(ExchangeFailure::Failed(anyhow!(
                "token refresh failed for the {} client (HTTP {status}): {detail}",
                profile.name
            )));
        }

        let token: TokenResponse =
            serde_json::from_str(&text).map_err(|error| ExchangeFailure::Failed(error.into()))?;
        if token.access_token.is_empty() {
            return Err(ExchangeFailure::Failed(anyhow!(
                "token refresh returned an empty access token"
            )));
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

enum ExchangeFailure {
    Rejected(anyhow::Error),
    Failed(anyhow::Error),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_oauth(responses: Vec<String>) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 8192];
                let _ = socket.read(&mut buffer).await;
                let _ = socket.write_all(response.as_bytes()).await;
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        (format!("http://{address}"), requests)
    }

    fn http_json(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn token_body(refresh: &str) -> String {
        serde_json::json!({
            "access_token": "access-token",
            "refresh_token": refresh,
            "expires_in": 3600,
            "sub": "user-1"
        })
        .to_string()
    }

    fn test_client(dir: &std::path::Path, auth_base: String) -> PikPak {
        let env_file = dir.join(".env");
        let config = ConfigFile::load(env_file.clone()).unwrap();
        let paths = Paths {
            program_dir: dir.to_path_buf(),
            downloads_dir: dir.join("downloads"),
            env_file,
        };
        let config_lock_path = crate::fsutil::lock_path(&paths.env_file);
        PikPak {
            http: reqwest::Client::new(),
            http_range: reqwest::Client::builder().no_gzip().build().unwrap(),
            paths,
            config: Arc::new(Mutex::new(config)),
            device_id: "test-device".to_string(),
            session: Arc::new(Mutex::new(None)),
            captcha: Arc::new(Mutex::new(HashMap::new())),
            auth_base,
            drive_base: "http://127.0.0.1:9".to_string(),
            auth_lock: Arc::new(Mutex::new(())),
            config_lock_path,
            persistence_pending: Arc::new(Mutex::new(false)),
        }
    }

    #[tokio::test]
    async fn rejected_token_restores_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let (auth_base, requests) = spawn_oauth(vec![
            http_json(
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_code":4126}"#,
            ),
            http_json(
                "400 Bad Request",
                r#"{"error":"invalid_grant","error_code":4126}"#,
            ),
        ])
        .await;
        let client = test_client(dir.path(), auth_base);
        {
            let mut config = client.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, "previous");
            config.save().unwrap();
        }

        let error = client.login_with_token("rejected").await.unwrap_err();
        assert!(format!("{error:#}").contains("invalid_grant"));
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.config_file().await.get(KEY_REFRESH_TOKEN),
            Some("previous")
        );
    }

    #[tokio::test]
    async fn rotated_token_is_persisted_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let (auth_base, requests) =
            spawn_oauth(vec![http_json("200 OK", &token_body("rotated"))]).await;
        let client = test_client(dir.path(), auth_base);
        {
            let mut config = client.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, "old");
            config.save().unwrap();
        }

        let session = client.login_with_token("old").await.unwrap();
        assert_eq!(session.refresh_token, "rotated");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            client.config_file().await.get(KEY_REFRESH_TOKEN),
            Some("rotated")
        );
        let disk = ConfigFile::load(dir.path().join(".env")).unwrap();
        assert_eq!(disk.get(KEY_REFRESH_TOKEN), Some("rotated"));
    }

    #[tokio::test]
    async fn persistence_failure_keeps_the_rotated_session_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let (auth_base, _) = spawn_oauth(vec![http_json("200 OK", &token_body("rotated"))]).await;
        let client = test_client(dir.path(), auth_base);
        {
            let mut config = client.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, "old");
            config.save().unwrap();
            // The login save succeeds; the rotated-token save fails.
            config.fail_save_after(2);
        }

        let error = client.login_with_token("old").await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("rotated the refresh token"), "{text}");
        assert_eq!(
            client.config_file().await.get(KEY_REFRESH_TOKEN),
            Some("rotated")
        );
        assert!(client.session.lock().await.is_some());
        assert!(*client.persistence_pending.lock().await);
        let disk = ConfigFile::load(dir.path().join(".env")).unwrap();
        assert_eq!(disk.get(KEY_REFRESH_TOKEN), Some("old"));

        client.retry_persist_rotated_token().await.unwrap();
        assert!(!*client.persistence_pending.lock().await);
        let disk = ConfigFile::load(dir.path().join(".env")).unwrap();
        assert_eq!(disk.get(KEY_REFRESH_TOKEN), Some("rotated"));
    }

    #[tokio::test]
    async fn concurrent_session_calls_consume_one_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let (auth_base, requests) =
            spawn_oauth(vec![http_json("200 OK", &token_body("rotated"))]).await;
        let client = test_client(dir.path(), auth_base);
        {
            let mut config = client.config.lock().await;
            config.set(KEY_REFRESH_TOKEN, "old");
            config.save().unwrap();
        }

        let first = client.clone();
        let second = client.clone();
        let (a, b) = tokio::join!(first.session(), second.session());
        assert!(a.unwrap().is_some());
        assert!(b.unwrap().is_some());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            client.config_file().await.get(KEY_REFRESH_TOKEN),
            Some("rotated")
        );
    }
}
