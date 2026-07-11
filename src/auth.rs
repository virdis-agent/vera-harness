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
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    token_type: Option<String>,
    id_token: Option<String>,
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
        let (issuer, client_id, endpoint) = oauth_config(provider);
        let mut form = vec![("client_id", client_id), ("scope", "openid offline_access")];
        if let Some(challenge) = challenge {
            form.push(("code_challenge", challenge));
        }
        let response = self.http.post(endpoint).form(&form).send().await?;
        ensure_origin(response.url(), issuer)?;
        if !response.status().is_success() {
            return Err(VeraError::Auth(safe_response(response).await).into());
        }
        Ok(response.json().await?)
    }

    pub async fn poll(
        &self,
        provider: AuthProvider,
        device: &DeviceAuthorization,
        verifier: Option<&str>,
    ) -> Result<TokenRecord> {
        let (issuer, client_id, endpoint) = oauth_config(provider);
        let mut form = vec![
            ("client_id", client_id),
            ("device_code", device.device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ];
        if let Some(verifier) = verifier {
            form.push(("code_verifier", verifier));
        }
        let response = self.http.post(endpoint).form(&form).send().await?;
        ensure_origin(response.url(), issuer)?;
        let status = response.status();
        if !status.is_success() {
            return Err(VeraError::Auth(safe_response(response).await).into());
        }
        let token: OAuthTokenResponse = response.json().await?;
        Ok(TokenRecord {
            provider,
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: token.expires_in.map(|seconds| now_seconds() + seconds),
            account_id: token.id_token.as_deref().and_then(account_id_from_jwt),
            token_type: token.token_type.unwrap_or_else(|| "Bearer".into()),
        })
    }

    pub async fn refresh(&self, existing: &TokenRecord) -> Result<TokenRecord> {
        let (issuer, client_id, endpoint) = oauth_config(existing.provider);
        let refresh_token = existing
            .refresh_token
            .as_deref()
            .ok_or_else(|| VeraError::Auth("provider did not return a refresh token".into()))?;
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
        let token: OAuthTokenResponse = response.json().await?;
        Ok(TokenRecord {
            provider: existing.provider,
            access_token: token.access_token,
            refresh_token: token
                .refresh_token
                .or_else(|| existing.refresh_token.clone()),
            expires_at: token.expires_in.map(|seconds| now_seconds() + seconds),
            account_id: token
                .id_token
                .as_deref()
                .and_then(account_id_from_jwt)
                .or_else(|| existing.account_id.clone()),
            token_type: token.token_type.unwrap_or_else(|| "Bearer".into()),
        })
    }
}

pub fn pkce_pair() -> (String, String) {
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = Sha256::digest(verifier.as_bytes());
    (verifier, URL_SAFE_NO_PAD.encode(digest))
}

fn oauth_config(provider: AuthProvider) -> (&'static str, &'static str, &'static str) {
    match provider {
        AuthProvider::OpenaiCodex => (
            OPENAI_ISSUER,
            "codex-cli",
            "https://auth.openai.com/oauth/device/code",
        ),
        AuthProvider::XaiOauth => (
            XAI_ISSUER,
            "grok-cli",
            "https://auth.x.ai/oauth/device/code",
        ),
    }
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
        if let Some(index) = value.to_ascii_lowercase().find(&key.to_ascii_lowercase()) {
            let tail = &value[index..];
            if let Some(colon) = tail.find(':') {
                let start = index + colon + 1;
                let end = value[start..]
                    .find([',', '}', '\n'])
                    .map_or(value.len(), |offset| start + offset);
                value.replace_range(start..end, " \"[REDACTED]\"");
            }
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
    json.get("chatgpt_account_id")
        .or_else(|| json.get("account_id"))
        .and_then(|value| value.as_str())
        .map(str::to_owned)
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
    }

    #[test]
    fn pkce_is_url_safe() {
        let (verifier, challenge) = pkce_pair();
        assert!(!verifier.is_empty());
        assert!(!challenge.contains('='));
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
