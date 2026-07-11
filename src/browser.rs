use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 40_000_000;
const MAX_SNAPSHOT_CHARS: usize = 80 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserStatus {
    pub endpoint: String,
    pub browser: String,
    pub protocol_version: String,
    pub target_id: String,
    pub target_url: String,
    pub target_title: String,
    #[serde(default)]
    pub console_errors: Vec<BrowserConsoleMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserConsoleMessage {
    pub level: String,
    pub message: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub line: Option<i64>,
}

#[derive(Clone, Debug)]
struct BrowserSession {
    status: BrowserStatus,
    websocket_url: String,
}

#[derive(Clone, Default)]
pub struct BrowserManager {
    session: Arc<Mutex<Option<BrowserSession>>>,
    approved_endpoints: Arc<Mutex<Vec<String>>>,
}

impl BrowserManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn set_approved_endpoints(&self, endpoints: Vec<String>) {
        *self.approved_endpoints.lock().await = endpoints;
    }

    pub async fn connect(&self, endpoint: &str) -> Result<BrowserStatus> {
        let base = validate_endpoint(endpoint)?;
        let approved = self.approved_endpoints.lock().await.clone();
        if !approved.is_empty()
            && !approved
                .iter()
                .any(|value| value.trim_end_matches('/') == base)
        {
            anyhow::bail!("CDP endpoint is not configured for this project");
        }
        let version_url = format!("{}/json/version", base.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        let version: Value = client
            .get(&version_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let list_url = format!("{}/json/list", base.trim_end_matches('/'));
        let targets: Vec<Value> = client
            .get(&list_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let target = targets
            .into_iter()
            .find(|value| value.get("type").and_then(Value::as_str) == Some("page"))
            .context("CDP endpoint has no page target")?;
        let websocket_url = target
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .context("CDP page target has no websocket URL")?
            .to_owned();
        let websocket = Url::parse(&websocket_url).context("invalid CDP websocket URL")?;
        if websocket.scheme() != "ws"
            || websocket.host_str().is_none()
            || !same_cdp_authority(&Url::parse(&base)?, &websocket)
        {
            anyhow::bail!("only ws:// CDP targets are supported in this native slice");
        }
        let status = BrowserStatus {
            endpoint: base,
            browser: version
                .get("Browser")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            protocol_version: version
                .get("Protocol-Version")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            target_id: target
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            target_url: target
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            target_title: target
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            console_errors: Vec::new(),
        };
        *self.session.lock().await = Some(BrowserSession {
            status: status.clone(),
            websocket_url,
        });
        self.command("Runtime.enable", json!({})).await?;
        self.command("Log.enable", json!({})).await?;
        self.status().await
    }

    pub async fn status(&self) -> Result<BrowserStatus> {
        self.session
            .lock()
            .await
            .as_ref()
            .map(|session| session.status.clone())
            .context("browser is not connected")
    }

    pub async fn navigate(&self, url: &str) -> Result<BrowserStatus> {
        let parsed = Url::parse(url).context("invalid browser URL")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            anyhow::bail!("browser URL must be http or https");
        }
        let target_id = self.status().await?.target_id;
        let response = self.command("Page.navigate", json!({"url": url})).await?;
        if response.get("errorText").and_then(Value::as_str).is_some() {
            anyhow::bail!("browser navigation failed: {}", response["errorText"]);
        }
        let target_info = self
            .command("Target.getTargetInfo", json!({"targetId": target_id}))
            .await
            .unwrap_or(Value::Null);
        let final_url = target_info
            .pointer("/targetInfo/url")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(url);
        let mut status = self.status().await?;
        status.target_url = final_url.to_owned();
        if let Some(session) = self.session.lock().await.as_mut() {
            session.status = status.clone();
        }
        Ok(status)
    }

    pub async fn snapshot(&self) -> Result<String> {
        let dom = self
            .command(
                "Runtime.evaluate",
                json!({"expression":"document.documentElement ? document.documentElement.outerHTML : ''","returnByValue":true}),
            )
            .await?;
        let dom = dom
            .pointer("/result/result/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(MAX_SNAPSHOT_CHARS / 2)
            .collect::<String>();
        let accessibility = self
            .command("Accessibility.getFullAXTree", json!({}))
            .await
            .unwrap_or_else(|_| json!({"unavailable": true}));
        let accessibility = bound_snapshot_value(&accessibility, MAX_SNAPSHOT_CHARS / 4);
        let console_errors = self
            .status()
            .await?
            .console_errors
            .into_iter()
            .rev()
            .take(16)
            .map(|mut error| {
                error.message = error.message.chars().take(500).collect();
                error.url = error.url.chars().take(500).collect();
                error
            })
            .collect::<Vec<_>>();
        let snapshot = json!({
            "dom": dom,
            "accessibility": accessibility,
            "console_errors": console_errors,
        });
        Ok(serde_json::to_string(&snapshot)?)
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let value = self
            .command("Page.captureScreenshot", json!({"format":"png"}))
            .await?;
        let encoded = value
            .get("data")
            .and_then(Value::as_str)
            .context("CDP screenshot did not return data")?;
        let bytes = STANDARD.decode(encoded).context("decode CDP screenshot")?;
        validate_image_bytes(&bytes).context("invalid or oversized CDP screenshot")?;
        Ok(bytes)
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value> {
        let session = self
            .session
            .lock()
            .await
            .clone()
            .context("browser is not connected")?;
        let (result, events) = cdp_command(&session.websocket_url, method, params).await?;
        if !events.is_empty()
            && let Some(session) = self.session.lock().await.as_mut()
        {
            for event in events {
                record_console_event(&mut session.status, &event);
            }
        }
        Ok(result)
    }
}

fn bound_snapshot_value(value: &Value, limit: usize) -> Value {
    let encoded = serde_json::to_vec(value).unwrap_or_default();
    if encoded.len() <= limit {
        return value.clone();
    }
    json!({"truncated":true,"original_bytes":encoded.len()})
}

fn record_console_event(status: &mut BrowserStatus, event: &Value) {
    let method = event.get("method").and_then(Value::as_str);
    let params = event.get("params").unwrap_or(&Value::Null);
    let (level, message, url, line) = match method {
        Some("Runtime.consoleAPICalled")
            if matches!(params.get("type").and_then(Value::as_str), Some("error")) =>
        {
            let message = params
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .map(|arg| {
                            arg.pointer("/value")
                                .or_else(|| arg.get("description"))
                                .or_else(|| arg.get("unserializableValue"))
                                .and_then(Value::as_str)
                                .unwrap_or("[console error]")
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| "[console error]".into());
            (
                "error".to_owned(),
                message,
                params
                    .get("stackTrace")
                    .and_then(|trace| trace.get("callFrames"))
                    .and_then(Value::as_array)
                    .and_then(|frames| frames.first())
                    .and_then(|frame| frame.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                params
                    .get("stackTrace")
                    .and_then(|trace| trace.get("callFrames"))
                    .and_then(Value::as_array)
                    .and_then(|frames| frames.first())
                    .and_then(|frame| frame.get("lineNumber"))
                    .and_then(Value::as_i64),
            )
        }
        Some("Log.entryAdded")
            if matches!(
                params.pointer("/entry/level").and_then(Value::as_str),
                Some("error")
            ) =>
        {
            (
                "error".into(),
                params
                    .pointer("/entry/text")
                    .and_then(Value::as_str)
                    .unwrap_or("[browser log error]")
                    .to_owned(),
                params
                    .pointer("/entry/url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                params.pointer("/entry/lineNumber").and_then(Value::as_i64),
            )
        }
        _ => return,
    };
    status.console_errors.push(BrowserConsoleMessage {
        level,
        message: message.chars().take(4_000).collect(),
        url,
        line,
    });
    if status.console_errors.len() > 128 {
        let excess = status.console_errors.len() - 128;
        status.console_errors.drain(..excess);
    }
}

pub fn inspect_image(path: &Path, root: &Path) -> Result<Value> {
    let root = std::fs::canonicalize(root)?;
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(&root) {
        anyhow::bail!("image path escapes repository root");
    }
    let metadata = std::fs::metadata(&canonical)?;
    if metadata.len() as usize > MAX_IMAGE_BYTES {
        anyhow::bail!("image exceeds {} byte limit", MAX_IMAGE_BYTES);
    }
    let bytes = std::fs::read(&canonical)?;
    let (mime, width, height) = validate_image_bytes(&bytes)?;
    Ok(json!({
        "path": canonical,
        "mime_type": mime,
        "bytes": bytes.len(),
        "width": width,
        "height": height,
    }))
}

fn validate_image_bytes(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("image exceeds {} byte limit", MAX_IMAGE_BYTES);
    }
    let dimensions = image_dimensions(bytes).context("unsupported or malformed image")?;
    if dimensions.1 == 0 || dimensions.2 == 0 {
        anyhow::bail!("image dimensions must be non-zero");
    }
    if dimensions.1 as u64 * dimensions.2 as u64 > MAX_IMAGE_PIXELS {
        anyhow::bail!("image exceeds pixel limit");
    }
    Ok(dimensions)
}

fn image_dimensions(bytes: &[u8]) -> Option<(&'static str, u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 {
        return Some((
            "image/png",
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        ));
    }
    if bytes.starts_with(&[0xff, 0xd8]) {
        let mut index = 2;
        while index + 9 < bytes.len() {
            if bytes[index] != 0xff {
                index += 1;
                continue;
            }
            let marker = bytes[index + 1];
            let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
            if (0xc0..=0xc3).contains(&marker) && index + 8 < bytes.len() {
                return Some((
                    "image/jpeg",
                    u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32,
                    u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32,
                ));
            }
            index = index.saturating_add(2 + length);
        }
    }
    None
}

fn validate_endpoint(endpoint: &str) -> Result<String> {
    let parsed = Url::parse(endpoint).context("invalid CDP endpoint")?;
    if parsed.scheme() != "http" || parsed.host_str().is_none() {
        anyhow::bail!("CDP endpoint must be an explicit http:// endpoint");
    }
    Ok(endpoint.trim_end_matches('/').to_owned())
}

fn same_cdp_authority(http: &Url, websocket: &Url) -> bool {
    if http.port_or_known_default() != websocket.port_or_known_default() {
        return false;
    }
    let Some(http_host) = http.host_str() else {
        return false;
    };
    let Some(websocket_host) = websocket.host_str() else {
        return false;
    };
    http_host.eq_ignore_ascii_case(websocket_host)
        || (is_loopback_host(http_host) && is_loopback_host(websocket_host))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

async fn cdp_command(
    websocket_url: &str,
    method: &str,
    params: Value,
) -> Result<(Value, Vec<Value>)> {
    let url = Url::parse(websocket_url)?;
    let host = url.host_str().context("CDP websocket host missing")?;
    let port = url
        .port_or_known_default()
        .context("CDP websocket port missing")?;
    let mut stream = timeout(Duration::from_secs(10), TcpStream::connect((host, port))).await??;
    let key = STANDARD.encode(Uuid::new_v4().as_bytes());
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut handshake = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).await?;
        handshake.push(byte[0]);
        if handshake.ends_with(b"\r\n\r\n") {
            break;
        }
        if handshake.len() > 16 * 1024 {
            anyhow::bail!("CDP websocket handshake is too large");
        }
    }
    if !handshake.starts_with(b"HTTP/1.1 101") {
        anyhow::bail!("CDP websocket upgrade failed");
    }
    let id = 1_u64;
    let request = json!({"id": id, "method": method, "params": params});
    write_frame(&mut stream, request.to_string().as_bytes()).await?;
    let mut events = Vec::new();
    loop {
        let payload = timeout(Duration::from_secs(10), read_frame(&mut stream)).await??;
        let value: Value = serde_json::from_slice(&payload).context("invalid CDP response")?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(error) = value.get("error") {
                anyhow::bail!("CDP command failed: {error}");
            }
            let result = value.get("result").cloned().unwrap_or(Value::Null);
            while let Ok(Ok(payload)) =
                timeout(Duration::from_millis(50), read_frame(&mut stream)).await
            {
                let Ok(event) = serde_json::from_slice::<Value>(&payload) else {
                    continue;
                };
                if event.get("method").is_some() {
                    events.push(event);
                }
            }
            return Ok((result, events));
        } else if value.get("method").is_some() {
            events.push(value);
        }
    }
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    let mut frame = vec![0x81_u8];
    let length = payload.len();
    if length < 126 {
        frame.push(0x80 | length as u8);
    } else if length <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(length as u64).to_be_bytes());
    }
    let mask_id = Uuid::new_v4();
    let mask = &mask_id.as_bytes()[..4];
    frame.extend_from_slice(mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).await?;
    Ok(())
}

async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).await?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut length = (header[1] & 0x7f) as usize;
    if length == 126 {
        let mut bytes = [0_u8; 2];
        stream.read_exact(&mut bytes).await?;
        length = u16::from_be_bytes(bytes) as usize;
    } else if length == 127 {
        let mut bytes = [0_u8; 8];
        stream.read_exact(&mut bytes).await?;
        length = u64::from_be_bytes(bytes) as usize;
    }
    if length > 8 * 1024 * 1024 {
        anyhow::bail!("CDP websocket frame is too large");
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    if opcode == 0x8 {
        anyhow::bail!("CDP websocket closed");
    }
    Ok(payload)
}

pub fn atomic_screenshot(path: &Path, bytes: &[u8]) -> Result<()> {
    validate_image_bytes(bytes).context("invalid or oversized screenshot")?;
    let parent = path.parent().context("screenshot has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.vera-screenshot", Uuid::new_v4().simple()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub fn image_limit_bytes() -> usize {
    MAX_IMAGE_BYTES
}

pub fn image_root_path(root: &Path, relative: &str) -> PathBuf {
    root.join(relative)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_bounded_pngs_and_rejects_path_escape() {
        let root = tempfile::tempdir().unwrap();
        let image = root.path().join("fixture.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&800_u32.to_be_bytes());
        bytes.extend_from_slice(&600_u32.to_be_bytes());
        std::fs::write(&image, bytes).unwrap();
        let inspected = inspect_image(&image, root.path()).unwrap();
        assert_eq!(inspected["mime_type"], "image/png");
        assert_eq!(inspected["width"], 800);
        assert!(inspect_image(Path::new("/tmp/not-in-root.png"), root.path()).is_err());
    }

    #[test]
    fn records_runtime_console_errors_with_bounded_history() {
        let mut status = BrowserStatus {
            endpoint: "http://127.0.0.1:9222".into(),
            browser: "fixture".into(),
            protocol_version: "1".into(),
            target_id: "page".into(),
            target_url: "http://127.0.0.1".into(),
            target_title: "fixture".into(),
            console_errors: Vec::new(),
        };
        record_console_event(
            &mut status,
            &json!({
                "method":"Runtime.consoleAPICalled",
                "params":{"type":"error","args":[{"value":"boom"}],"stackTrace":{"callFrames":[{"url":"fixture.js","lineNumber":12}]}}
            }),
        );
        assert_eq!(status.console_errors.len(), 1);
        assert_eq!(status.console_errors[0].message, "boom");
        assert_eq!(status.console_errors[0].line, Some(12));
    }

    #[test]
    fn only_explicit_http_cdp_endpoints_are_accepted() {
        assert!(validate_endpoint("http://127.0.0.1:9222").is_ok());
        assert!(validate_endpoint("ws://127.0.0.1:9222/devtools/page").is_err());
        assert!(validate_endpoint("file:///tmp/browser").is_err());
    }

    #[test]
    fn websocket_target_must_stay_on_the_approved_cdp_authority() {
        let http = Url::parse("http://127.0.0.1:9222").unwrap();
        assert!(same_cdp_authority(
            &http,
            &Url::parse("ws://localhost:9222/devtools/page").unwrap()
        ));
        assert!(!same_cdp_authority(
            &http,
            &Url::parse("ws://example.test:9222/devtools/page").unwrap()
        ));
        assert!(!same_cdp_authority(
            &http,
            &Url::parse("ws://127.0.0.1:9223/devtools/page").unwrap()
        ));
    }

    #[tokio::test]
    async fn configured_endpoint_allowlist_rejects_other_hosts_before_connecting() {
        let manager = BrowserManager::new();
        manager
            .set_approved_endpoints(vec!["http://127.0.0.1:9222".into()])
            .await;
        let error = manager.connect("http://localhost:9222").await.unwrap_err();
        assert!(error.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn cdp_fixture_supports_status_navigation_snapshot_console_and_screenshot() {
        let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
            // Some managed CI sandboxes prohibit local socket binds; the
            // deterministic protocol test still runs on supported hosts.
            return;
        };
        let address = listener.local_addr().unwrap();
        let websocket_url = format!("ws://{address}/devtools/page");
        let server = tokio::spawn(async move {
            for _ in 0..9 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0_u8; 1];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                if request_text.starts_with("GET /json/version") {
                    write_http_json(
                        &mut stream,
                        &json!({"Browser":"Fixture/1","Protocol-Version":"1.3"}),
                    )
                    .await;
                    continue;
                }
                if request_text.starts_with("GET /json/list") {
                    write_http_json(
                        &mut stream,
                        &json!([{"type":"page","id":"fixture-page","url":"http://fixture.test/","title":"Fixture","webSocketDebuggerUrl":websocket_url}]),
                    )
                    .await;
                    continue;
                }
                stream
                    .write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
                    .await
                    .unwrap();
                let payload = read_ws_payload(&mut stream).await;
                let request: Value = serde_json::from_slice(&payload).unwrap_or_else(|error| {
                    panic!(
                        "invalid fixture websocket request {error}: bytes={:02x?}",
                        payload
                    )
                });
                let id = request["id"].as_u64().unwrap();
                let method = request["method"].as_str().unwrap();
                if method == "Page.navigate" {
                    write_ws_json(
                        &mut stream,
                        &json!({"method":"Runtime.consoleAPICalled","params":{"type":"error","args":[{"value":"fixture console error"}],"stackTrace":{"callFrames":[{"url":"fixture.js","lineNumber":7}]}}}),
                    )
                    .await;
                }
                let result = match method {
                    "Runtime.evaluate" => {
                        json!({"result":{"type":"string","value":"<html><body>fixture</body></html>"}})
                    }
                    "Accessibility.getFullAXTree" => {
                        json!({"nodes":[{"role":{"value":"RootWebArea"}}]})
                    }
                    "Page.captureScreenshot" => {
                        let mut screenshot = b"\x89PNG\r\n\x1a\n".to_vec();
                        screenshot.extend_from_slice(&[0, 0, 0, 13]);
                        screenshot.extend_from_slice(b"IHDR");
                        screenshot.extend_from_slice(&1_u32.to_be_bytes());
                        screenshot.extend_from_slice(&1_u32.to_be_bytes());
                        json!({"data":STANDARD.encode(screenshot)})
                    }
                    _ => json!({}),
                };
                write_ws_json(&mut stream, &json!({"id":id,"result":result})).await;
            }
        });

        let manager = BrowserManager::new();
        let status = manager.connect(&format!("http://{address}")).await.unwrap();
        assert_eq!(status.target_id, "fixture-page");
        let status = manager.navigate("http://fixture.test/next").await.unwrap();
        assert_eq!(status.target_url, "http://fixture.test/next");
        assert_eq!(status.console_errors[0].message, "fixture console error");
        let snapshot = manager.snapshot().await.unwrap();
        assert!(snapshot.contains("RootWebArea"));
        assert!(snapshot.contains("fixture"));
        let screenshot = manager.screenshot().await.unwrap();
        assert!(screenshot.starts_with(b"\x89PNG\r\n\x1a\n"));
        server.await.unwrap();
    }

    async fn write_http_json(stream: &mut TcpStream, value: &Value) {
        let body = value.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    async fn read_ws_payload(stream: &mut TcpStream) -> Vec<u8> {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        let mut length = (header[1] & 0x7f) as usize;
        if length == 126 {
            let mut bytes = [0_u8; 2];
            stream.read_exact(&mut bytes).await.unwrap();
            length = u16::from_be_bytes(bytes) as usize;
        } else if length == 127 {
            let mut bytes = [0_u8; 8];
            stream.read_exact(&mut bytes).await.unwrap();
            length = u64::from_be_bytes(bytes) as usize;
        }
        let mut mask = [0_u8; 4];
        stream.read_exact(&mut mask).await.unwrap();
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await.unwrap();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        payload
    }

    async fn write_ws_json(stream: &mut TcpStream, value: &Value) {
        let payload = value.to_string();
        let mut frame = vec![0x81];
        if payload.len() < 126 {
            frame.push(payload.len() as u8);
        } else {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(payload.as_bytes());
        stream.write_all(&frame).await.unwrap();
    }
}
