use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use fs2::FileExt;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::VeraError;
use crate::paths::{VeraPaths, set_private_file};

pub const OPENAI_ISSUER: &str = "https://auth.openai.com";
pub const XAI_ISSUER: &str = "https://auth.x.ai";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_DEVICE_USERCODE_URL: &str =
    "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const CODEX_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const XAI_DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
pub const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthProvider {
    OpenaiCodex,
    XaiOauth,
}

impl AuthProvider {
    pub fn parse(value: &str) -> Result<Self, VeraError> {
        match value {
            "openai-codex" | "openai" | "codex" => Ok(Self::OpenaiCodex),
            "xai-oauth" | "xai" | "grok" => Ok(Self::XaiOauth),
            other => Err(VeraError::Auth(format!("unsupported provider {other}"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenaiCodex => "openai-codex",
            Self::XaiOauth => "xai-oauth",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenRecord {
    pub provider: AuthProvider,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
    pub account_id: Option<String>,
    pub token_type: String,
    /// The OIDC-discovered xAI token endpoint. Older auth records omit this
    /// field and rediscover it before the next xAI refresh.
    #[serde(default)]
    pub xai_token_endpoint: Option<String>,
}

impl TokenRecord {
    pub fn is_valid(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|expiry| match self.provider {
            AuthProvider::OpenaiCodex => expiry > now + 120,
            AuthProvider::XaiOauth => expiry > now + 3_600,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    version: u32,
    tokens: BTreeMap<AuthProvider, TokenRecord>,
}

pub struct TokenStore {
    paths: VeraPaths,
}

impl TokenStore {
    pub fn new(paths: VeraPaths) -> Self {
        Self { paths }
    }

    pub fn load(&self) -> Result<Vec<TokenRecord>> {
        if !self.paths.auth_file.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.paths.auth_file).context("read auth store")?;
        let parsed: AuthFile = serde_json::from_slice(&bytes).context("parse auth store")?;
        Ok(parsed.tokens.into_values().collect())
    }

    pub fn get(&self, provider: AuthProvider) -> Result<Option<TokenRecord>> {
        Ok(self
            .load()?
            .into_iter()
            .find(|token| token.provider == provider))
    }

    pub fn put(&self, record: TokenRecord) -> Result<()> {
        self.paths.ensure_runtime_dirs()?;
        let _lock = self.lock()?;
        let mut file = self.read_file()?;
        file.version = 1;
        file.tokens.insert(record.provider, record);
        self.atomic_write(&file)
    }

    pub fn remove(&self, provider: Option<AuthProvider>) -> Result<()> {
        if !self.paths.auth_file.exists() {
            return Ok(());
        }
        let _lock = self.lock()?;
        let mut file = self.read_file()?;
        match provider {
            Some(provider) => {
                file.tokens.remove(&provider);
            }
            None => file.tokens.clear(),
        }
        self.atomic_write(&file)
    }

    pub fn status(&self) -> Result<Vec<(AuthProvider, bool, Option<i64>)>> {
        let now = now_seconds();
        Ok(self
            .load()?
            .into_iter()
            .map(|token| (token.provider, token.is_valid(now), token.expires_at))
            .collect())
    }

    fn lock(&self) -> Result<File> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.paths.auth_lock)?;
        set_private_file(&self.paths.auth_lock)?;
        lock.lock_exclusive()?;
        Ok(lock)
    }

    fn read_file(&self) -> Result<AuthFile> {
        if !self.paths.auth_file.exists() {
            return Ok(AuthFile {
                version: 1,
                tokens: BTreeMap::new(),
            });
        }
        Ok(serde_json::from_slice(&fs::read(&self.paths.auth_file)?)?)
    }

    fn atomic_write(&self, file: &AuthFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(file)?;
        let temporary = self
            .paths
            .root
            .join(format!("auth.json.{}.tmp", Uuid::new_v4()));
        {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            set_private_file(&temporary)?;
            output.write_all(&bytes)?;
            output.sync_all()?;
        }
        fs::rename(&temporary, &self.paths.auth_file)?;
        set_private_file(&self.paths.auth_file)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct OAuthClient {
    http: reqwest::Client,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    #[serde(alias = "verification_url", alias = "verification_uri_complete")]
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    #[serde(default)]
    pub token_endpoint: Option<String>,
    #[serde(default)]
    pub provider_data: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    token_type: Option<String>,
    id_token: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct OpenaiDeviceAuthorization {
    #[serde(alias = "device_code")]
    device_auth_id: String,
    user_code: String,
    #[serde(
        default,
        alias = "verification_url",
        alias = "verification_uri_complete"
    )]
    verification_uri: Option<String>,
    #[serde(
        default = "default_device_expiry",
        deserialize_with = "deserialize_u64"
    )]
    expires_in: u64,
    #[serde(
        default = "default_poll_interval",
        deserialize_with = "deserialize_u64"
    )]
    interval: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct OpenaiDeviceToken {
    authorization_code: String,
    #[serde(alias = "code_verifier")]
    code_verifier: String,
}

#[derive(Clone, Debug, Deserialize)]
struct OidcDiscovery {
    token_endpoint: String,
}

fn default_device_expiry() -> u64 {
    900
}

fn default_poll_interval() -> u64 {
    5
}

fn deserialize_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .ok_or_else(|| serde::de::Error::custom("expected an integer or numeric string"))
}

impl OAuthClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("vera-harness/0.1")
                .build()?,
        })
    }

    pub async fn device_authorize(
        &self,
        provider: AuthProvider,
        challenge: Option<&str>,
    ) -> Result<DeviceAuthorization> {
        match provider {
            AuthProvider::OpenaiCodex => {
                let response = self
                    .http
                    .post(CODEX_DEVICE_USERCODE_URL)
                    .json(&serde_json::json!({"client_id": CODEX_CLIENT_ID}))
                    .send()
                    .await?;
                ensure_origin(response.url(), OPENAI_ISSUER)?;
                if !response.status().is_success() {
                    return Err(VeraError::Auth(safe_response(response).await).into());
                }
                let device: OpenaiDeviceAuthorization = response.json().await?;
                Ok(DeviceAuthorization {
                    device_code: device.device_auth_id,
                    user_code: device.user_code,
                    verification_uri: device
                        .verification_uri
                        .unwrap_or_else(|| format!("{OPENAI_ISSUER}/codex/device")),
                    expires_in: device.expires_in,
                    interval: device.interval,
                    token_endpoint: None,
                    provider_data: None,
                })
            }
            AuthProvider::XaiOauth => {
                let token_endpoint = self.discover_xai_token_endpoint().await?;
                let mut form = vec![("client_id", XAI_CLIENT_ID), ("scope", XAI_SCOPE)];
                if let Some(challenge) = challenge {
                    form.push(("code_challenge", challenge));
                    form.push(("code_challenge_method", "S256"));
                }
                let response = self
                    .http
                    .post(XAI_DEVICE_CODE_URL)
                    .form(&form)
                    .send()
                    .await?;
                ensure_origin(response.url(), XAI_ISSUER)?;
                if !response.status().is_success() {
                    return Err(VeraError::Auth(safe_response(response).await).into());
                }
                let mut device: DeviceAuthorization = response.json().await?;
                device.token_endpoint = Some(token_endpoint);
                Ok(device)
            }
        }
    }

    pub async fn poll(
        &self,
        provider: AuthProvider,
        device: &DeviceAuthorization,
        verifier: Option<&str>,
    ) -> Result<TokenRecord> {
        match provider {
            AuthProvider::OpenaiCodex => {
                let response = self
                    .http
                    .post(CODEX_DEVICE_TOKEN_URL)
                    .json(&serde_json::json!({
                        "device_auth_id": device.device_code,
                        "user_code": device.user_code,
                    }))
                    .send()
                    .await?;
                ensure_origin(response.url(), OPENAI_ISSUER)?;
                if !response.status().is_success() {
                    let error = oauth_poll_error(response)
                        .await?
                        .unwrap_or_else(|| "OAuth device authorization failed".into());
                    return Err(VeraError::Auth(error).into());
                }
                let device_token: OpenaiDeviceToken = response.json().await?;
                let response = self
                    .http
                    .post(CODEX_TOKEN_URL)
                    .form(&[
                        ("grant_type", "authorization_code"),
                        ("client_id", CODEX_CLIENT_ID),
                        ("code", device_token.authorization_code.as_str()),
                        ("code_verifier", device_token.code_verifier.as_str()),
                        (
                            "redirect_uri",
                            "https://auth.openai.com/deviceauth/callback",
                        ),
                    ])
                    .send()
                    .await?;
                ensure_origin(response.url(), OPENAI_ISSUER)?;
                if !response.status().is_success() {
                    return Err(VeraError::Auth(safe_response(response).await).into());
                }
                token_record_from_response(provider, response.json().await?, None)
            }
            AuthProvider::XaiOauth => {
                let endpoint = device.token_endpoint.as_deref().ok_or_else(|| {
                    VeraError::Auth("xAI token endpoint was not discovered".into())
                })?;
                ensure_origin_url(endpoint, XAI_ISSUER)?;
                let mut form = vec![
                    ("client_id", XAI_CLIENT_ID),
                    ("device_code", device.device_code.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ];
                if let Some(verifier) = verifier {
                    form.push(("code_verifier", verifier));
                }
                let response = self.http.post(endpoint).form(&form).send().await?;
                ensure_origin(response.url(), XAI_ISSUER)?;
                if !response.status().is_success() {
                    let error = oauth_poll_error(response)
                        .await?
                        .unwrap_or_else(|| "OAuth device authorization failed".into());
                    return Err(VeraError::Auth(error).into());
                }
                token_record_from_response(provider, response.json().await?, Some(endpoint.into()))
            }
        }
    }

    pub async fn refresh(&self, existing: &TokenRecord) -> Result<TokenRecord> {
        let refresh_token = existing
            .refresh_token
            .as_deref()
            .ok_or_else(|| VeraError::Auth("provider did not return a refresh token".into()))?;
        let (issuer, endpoint) = match existing.provider {
            AuthProvider::OpenaiCodex => (OPENAI_ISSUER, CODEX_TOKEN_URL.to_owned()),
            AuthProvider::XaiOauth => {
                let endpoint = match existing.xai_token_endpoint.as_deref() {
                    Some(endpoint) => endpoint.to_owned(),
                    None => self.discover_xai_token_endpoint().await?,
                };
                ensure_origin_url(&endpoint, XAI_ISSUER)?;
                (XAI_ISSUER, endpoint)
            }
        };
        let client_id = match existing.provider {
            AuthProvider::OpenaiCodex => CODEX_CLIENT_ID,
            AuthProvider::XaiOauth => XAI_CLIENT_ID,
        };
        let form = [
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        let response = self.http.post(endpoint).form(&form).send().await?;
        ensure_origin(response.url(), issuer)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::BAD_REQUEST {
            return Err(VeraError::Auth(
                "refresh token rejected; reauthentication required".into(),
            )
            .into());
        }
        if !status.is_success() {
            return Err(VeraError::Auth(safe_response(response).await).into());
        }
        token_record_from_response(
            existing.provider,
            response.json().await?,
            existing.xai_token_endpoint.clone(),
        )
        .map(|mut token| {
            token.refresh_token = token
                .refresh_token
                .or_else(|| existing.refresh_token.clone());
            token.account_id = token.account_id.or_else(|| existing.account_id.clone());
            token.expires_at = token.expires_at.or(existing.expires_at);
            token.xai_token_endpoint = token
                .xai_token_endpoint
                .or_else(|| existing.xai_token_endpoint.clone());
            token
        })
    }

    async fn discover_xai_token_endpoint(&self) -> Result<String> {
        let response = self.http.get(XAI_DISCOVERY_URL).send().await?;
        ensure_origin(response.url(), XAI_ISSUER)?;
        if !response.status().is_success() {
            return Err(VeraError::Auth(safe_response(response).await).into());
        }
        let discovery: OidcDiscovery = response.json().await?;
        ensure_origin_url(&discovery.token_endpoint, XAI_ISSUER)?;
        Ok(discovery.token_endpoint)
    }
}

pub fn pkce_pair() -> (String, String) {
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, URL_SAFE_NO_PAD.encode(digest))
}

fn ensure_origin(url: &reqwest::Url, issuer: &str) -> Result<()> {
    let expected = reqwest::Url::parse(issuer)?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
    {
        return Err(VeraError::Auth("OAuth origin pinning rejected response".into()).into());
    }
    Ok(())
}

fn ensure_origin_url(url: &str, issuer: &str) -> Result<()> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| VeraError::Auth("OAuth discovery returned an invalid endpoint".into()))?;
    ensure_origin(&url, issuer)
}

async fn oauth_poll_error(response: reqwest::Response) -> Result<Option<String>> {
    if response.status().is_success() {
        return Ok(None);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let parsed = serde_json::from_str::<serde_json::Value>(&body).ok();
    let error = parsed
        .as_ref()
        .and_then(|value| value.get("error"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let description = parsed
        .as_ref()
        .and_then(|value| value.get("error_description"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let label = match error {
        "authorization_pending" => "authorization pending",
        "slow_down" => "slow_down",
        "expired_token" | "expired_device_code" => "device authorization expired",
        _ if status == StatusCode::TOO_MANY_REQUESTS => "rate limited",
        _ if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND => {
            "authorization pending"
        }
        _ => "OAuth device authorization failed",
    };
    if matches!(error, "authorization_pending" | "slow_down")
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::FORBIDDEN
        || status == StatusCode::NOT_FOUND
    {
        return Ok(Some(label.into()));
    }
    let detail = if description.is_empty() {
        label.to_owned()
    } else {
        format!("{label}: {}", redact(description))
    };
    Ok(Some(detail))
}

fn token_record_from_response(
    provider: AuthProvider,
    token: OAuthTokenResponse,
    xai_token_endpoint: Option<String>,
) -> Result<TokenRecord> {
    Ok(TokenRecord {
        provider,
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token,
        expires_at: token.expires_in.map(|seconds| now_seconds() + seconds),
        account_id: token
            .id_token
            .as_deref()
            .and_then(account_id_from_jwt)
            .or_else(|| account_id_from_jwt(&token.access_token)),
        token_type: token.token_type.unwrap_or_else(|| "Bearer".into()),
        xai_token_endpoint,
    })
}

async fn safe_response(response: reqwest::Response) -> String {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    format!("OAuth request failed ({status}): {}", redact(&text))
}

pub fn redact(text: &str) -> String {
    let mut value = text.to_string();
    for key in [
        "access_token",
        "refresh_token",
        "id_token",
        "client_secret",
        "Authorization",
    ] {
        let key = key.to_ascii_lowercase();
        let mut cursor = 0;
        while cursor < value.len() {
            let lower = value.to_ascii_lowercase();
            let Some(relative) = lower[cursor..].find(&key) else {
                break;
            };
            let index = cursor + relative;
            let tail = &value[index..];
            let Some(colon) = tail.find(':') else {
                cursor = index.saturating_add(key.len());
                continue;
            };
            let start = index + colon + 1;
            let end = value[start..]
                .find([',', '}', '\n'])
                .map_or(value.len(), |offset| start + offset);
            value.replace_range(start..end, " \"[REDACTED]\"");
            cursor = start + " \"[REDACTED]\"".len();
        }
    }
    value
}

fn account_id_from_jwt(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .or_else(|| STANDARD.decode(payload).ok())?;
    let json: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    fn find(value: &serde_json::Value) -> Option<String> {
        if let Some(object) = value.as_object() {
            for key in ["chatgpt_account_id", "account_id"] {
                if let Some(value) = object.get(key).and_then(serde_json::Value::as_str) {
                    return Some(value.to_owned());
                }
            }
            for value in object.values() {
                if let Some(found) = find(value) {
                    return Some(found);
                }
            }
        } else if let Some(values) = value.as_array() {
            for value in values {
                if let Some(found) = find(value) {
                    return Some(found);
                }
            }
        }
        None
    }
    find(&json)
}

pub fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        assert!(!redact(r#"{"access_token":"secret"}"#).contains("secret"));
        let repeated = redact(r#"{"access_token":"first","nested":{"access_token":"second"}}"#);
        assert!(!repeated.contains("first"));
        assert!(!repeated.contains("second"));
    }

    #[test]
    fn pkce_is_url_safe() {
        let (verifier, challenge) = pkce_pair();
        assert!(!verifier.is_empty());
        assert!(!challenge.contains('='));
    }

    #[test]
    fn extracts_nested_chatgpt_account_id_claim() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_nested"}}"#);
        let jwt = format!("{header}.{payload}.");
        assert_eq!(account_id_from_jwt(&jwt).as_deref(), Some("acct_nested"));
    }

    #[test]
    fn legacy_token_records_default_discovered_endpoint_to_none() {
        let record: TokenRecord = serde_json::from_str(
            r#"{
                "provider":"openai-codex",
                "access_token":"access",
                "refresh_token":"refresh",
                "expires_at":null,
                "account_id":null,
                "token_type":"Bearer"
            }"#,
        )
        .unwrap();
        assert!(record.xai_token_endpoint.is_none());
    }

    #[test]
    fn token_store_rotates_atomically_and_keeps_private_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let store = TokenStore::new(paths.clone());
        store
            .put(TokenRecord {
                provider: AuthProvider::OpenaiCodex,
                access_token: "access".into(),
                refresh_token: Some("refresh".into()),
                expires_at: Some(now_seconds() + 10_000),
                account_id: Some("account".into()),
                token_type: "Bearer".into(),
                xai_token_endpoint: None,
            })
            .unwrap();
        assert_eq!(
            store
                .get(AuthProvider::OpenaiCodex)
                .unwrap()
                .unwrap()
                .access_token,
            "access"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&paths.auth_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
