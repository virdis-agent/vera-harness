use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::{AuthProvider, TokenRecord, redact};
use crate::error::VeraError;
use crate::events::{Event, EventSink};
use crate::paths::{VeraPaths, set_private_file};

// The Codex models endpoint treats this as a client capability version, not
// Vera's release identity. Keep it aligned with the official Codex catalog
// schema Vera has validated and can safely consume.
const OPENAI_CODEX_MODEL_CLIENT_VERSION: &str = "0.144.1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    OpenaiCodex,
    XaiOauth,
}

impl ProviderKind {
    pub fn parse(value: &str) -> Result<Self, VeraError> {
        match value {
            "openai-codex" | "openai" | "codex" => Ok(Self::OpenaiCodex),
            "xai-oauth" | "xai" | "grok" => Ok(Self::XaiOauth),
            other => Err(VeraError::Provider(format!("unsupported provider {other}"))),
        }
    }

    pub fn auth_provider(self) -> AuthProvider {
        match self {
            Self::OpenaiCodex => AuthProvider::OpenaiCodex,
            Self::XaiOauth => AuthProvider::XaiOauth,
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
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderInput {
    Message {
        role: String,
        content: String,
    },
    ImageMessage {
        role: String,
        text: String,
        mime_type: String,
        data_base64: String,
    },
    FunctionCall {
        id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

impl ProviderInput {
    pub fn message(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Message {
            role: role.into(),
            content: content.into(),
        }
    }

    pub fn image_message(
        role: impl Into<String>,
        text: impl Into<String>,
        mime_type: impl Into<String>,
        data_base64: impl Into<String>,
    ) -> Self {
        Self::ImageMessage {
            role: role.into(),
            text: text.into(),
            mime_type: mime_type.into(),
            data_base64: data_base64.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct ReasoningEffortInfo {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub supported: Vec<String>,
}

impl ReasoningEffortInfo {
    pub fn fixed() -> Self {
        Self {
            default: None,
            supported: Vec::new(),
        }
    }

    pub fn configurable(default: impl Into<String>, supported: &[&str]) -> Self {
        Self {
            default: Some(default.into()),
            supported: supported.iter().map(|value| (*value).into()).collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    pub provider: String,
    #[serde(default)]
    pub order: i32,
    pub context_window: usize,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    pub source: String,
}

impl ModelInfo {
    pub fn effort_info(&self) -> ReasoningEffortInfo {
        ReasoningEffortInfo {
            default: self.default_effort.clone(),
            supported: self.supported_efforts.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: BTreeMap<String, Vec<ModelInfo>>,
}

impl ModelCatalog {
    pub fn for_provider(&self, provider: ProviderKind) -> &[ModelInfo] {
        self.models
            .get(provider.as_str())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn find(&self, provider: ProviderKind, id: &str) -> Option<&ModelInfo> {
        self.for_provider(provider)
            .iter()
            .find(|model| model.id == id)
    }

    pub fn default_for(&self, provider: ProviderKind) -> Option<&ModelInfo> {
        self.for_provider(provider)
            .iter()
            .min_by_key(|model| (model.order, model.id.as_str()))
    }

    pub fn merge(&mut self, models: impl IntoIterator<Item = ModelInfo>) {
        for model in models {
            let entries = self.models.entry(model.provider.clone()).or_default();
            if let Some(existing) = entries.iter_mut().find(|entry| entry.id == model.id) {
                *existing = model;
            } else {
                entries.push(model);
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub model: String,
    pub input: Vec<ProviderInput>,
    pub tools: Vec<ToolSchema>,
    pub instructions: String,
    pub effort: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProviderResult {
    pub text: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn supports_image_input(&self) -> bool {
        false
    }
    async fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResult>;
    async fn models(&self) -> Result<ModelCatalog>;
}

#[derive(Clone)]
pub struct ResponsesProvider {
    kind: ProviderKind,
    token: TokenRecord,
    http: reqwest::Client,
}

impl ResponsesProvider {
    pub fn new(kind: ProviderKind, token: TokenRecord) -> Result<Self> {
        Ok(Self {
            kind,
            token,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("vera-harness/0.1")
                .build()?,
        })
    }

    fn endpoint(&self) -> &'static str {
        match self.kind {
            ProviderKind::OpenaiCodex => "https://chatgpt.com/backend-api/codex/responses",
            ProviderKind::XaiOauth => "https://api.x.ai/v1/responses",
        }
    }

    fn model_endpoint(&self) -> &'static str {
        match self.kind {
            ProviderKind::OpenaiCodex => "https://chatgpt.com/backend-api/codex/models",
            ProviderKind::XaiOauth => "https://api.x.ai/v1/language-models",
        }
    }

    fn model_url(&self) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(self.model_endpoint())?;
        if self.kind == ProviderKind::OpenaiCodex {
            url.query_pairs_mut()
                .append_pair("client_version", OPENAI_CODEX_MODEL_CLIENT_VERSION);
        }
        Ok(url)
    }

    fn add_auth_headers(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder = builder.bearer_auth(&self.token.access_token);
        if let Some(account_id) = &self.token.account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
        if self.kind == ProviderKind::OpenaiCodex {
            builder = builder.header("originator", "codex_cli_rs");
        }
        builder
    }

    fn body(&self, request: &ProviderRequest) -> Value {
        let input: Vec<Value> = request
            .input
            .iter()
            .map(|item| match item {
                ProviderInput::Message { role, content } => {
                    json!({"role":role,"content":content})
                }
                ProviderInput::ImageMessage {
                    role,
                    text,
                    mime_type,
                    data_base64,
                } => json!({
                    "role": role,
                    "content": [
                        {"type":"input_text","text":text},
                        {"type":"input_image","image_url":format!("data:{mime_type};base64,{data_base64}")}
                    ]
                }),
                ProviderInput::FunctionCall {
                    id,
                    name,
                    arguments,
                } => {
                    json!({"type":"function_call","id":id,"name":name,"arguments":arguments})
                }
                ProviderInput::FunctionCallOutput { call_id, output } => {
                    json!({"type":"function_call_output","call_id":call_id,"output":output})
                }
            })
            .collect();
        let mut body = json!({
            "model": request.model,
            "instructions": request.instructions,
            "input": input,
            "stream": true,
            "store": false
        });
        if let Some(effort) = &request.effort {
            body["reasoning"] = json!({"effort": effort});
        }
        let tools = request
            .tools
            .iter()
            .filter_map(|tool| match (self.kind, tool.name.as_str()) {
                (ProviderKind::OpenaiCodex, "web_search") => {
                    Some(json!({"type":"web_search"}))
                }
                (ProviderKind::OpenaiCodex, "x_search") => None,
                (ProviderKind::XaiOauth, "web_search") => None,
                (ProviderKind::XaiOauth, "x_search") => Some(json!({"type":"x_search"})),
                _ => Some(json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters})),
            })
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        body
    }

    pub fn supports_image_input(&self) -> bool {
        matches!(
            self.kind,
            ProviderKind::OpenaiCodex | ProviderKind::XaiOauth
        )
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn supports_image_input(&self) -> bool {
        true
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResult> {
        if request
            .input
            .iter()
            .any(|input| matches!(input, ProviderInput::ImageMessage { .. }))
            && !Provider::supports_image_input(self)
        {
            return Err(VeraError::Provider("provider does not support image input".into()).into());
        }
        let builder = self
            .http
            .post(self.endpoint())
            .header(CONTENT_TYPE, "application/json")
            .json(&self.body(&request));
        let builder = self.add_auth_headers(builder);
        let response = builder.send().await.context("provider request")?;
        let expected_origin = match self.kind {
            ProviderKind::OpenaiCodex => "https://chatgpt.com",
            ProviderKind::XaiOauth => "https://api.x.ai",
        };
        ensure_provider_origin(response.url(), expected_origin)?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(VeraError::Provider(
                "provider rejected credentials; retry after refresh".into(),
            )
            .into());
        }
        if !response.status().is_success() {
            let status = response.status();
            let detail = safe_provider_detail(response).await;
            return Err(
                VeraError::Provider(format!("provider returned {status}: {detail}")).into(),
            );
        }
        let mut decoder = SseDecoder::default();
        let mut result = ProviderResult::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            for payload in decoder.push(&chunk) {
                for event in normalize_response_event(&payload) {
                    match &event {
                        Event::TextDelta { text } => result.text.push_str(text),
                        Event::Usage {
                            input_tokens,
                            output_tokens,
                        } => {
                            result.input_tokens = *input_tokens;
                            result.output_tokens = *output_tokens;
                        }
                        _ => {}
                    }
                    sink.emit(event).await?;
                }
            }
        }
        for payload in decoder.finish() {
            for event in normalize_response_event(&payload) {
                sink.emit(event).await?;
            }
        }
        sink.emit(Event::Completed).await?;
        Ok(result)
    }

    async fn models(&self) -> Result<ModelCatalog> {
        let response = self
            .add_auth_headers(self.http.get(self.model_url()?))
            .send()
            .await?;
        let expected_origin = match self.kind {
            ProviderKind::OpenaiCodex => "https://chatgpt.com",
            ProviderKind::XaiOauth => "https://api.x.ai",
        };
        ensure_provider_origin(response.url(), expected_origin)?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = safe_provider_detail(response).await;
            return Err(VeraError::Provider(format!(
                "model discovery returned {status}: {detail}"
            ))
            .into());
        }
        let body: Value = response.json().await?;
        let models = match self.kind {
            ProviderKind::OpenaiCodex => parse_openai_models(&body)?,
            ProviderKind::XaiOauth => parse_xai_models(&body)?,
        };
        Ok(ModelCatalog {
            models: BTreeMap::from([(self.kind.as_str().into(), models)]),
        })
    }
}

async fn safe_provider_detail(response: reqwest::Response) -> String {
    safe_provider_detail_text(&response.text().await.unwrap_or_default())
}

fn safe_provider_detail_text(text: &str) -> String {
    const MAX_CHARS: usize = 512;
    let redacted = redact(text);
    let mut chars = redacted.chars();
    let detail: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{detail}…")
    } else if detail.trim().is_empty() {
        "provider returned no error details".into()
    } else {
        detail
    }
}

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        while let Some(position) = self.buffer.windows(2).position(|window| window == b"\n\n") {
            let frame: Vec<u8> = self.buffer.drain(..position + 2).collect();
            if let Ok(text) = String::from_utf8(frame)
                && let Some(payload) = data_payload(&text)
            {
                messages.push(payload);
            }
        }
        messages
    }

    pub fn finish(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let frame = std::mem::take(&mut self.buffer);
        String::from_utf8(frame)
            .ok()
            .and_then(|text| data_payload(&text))
            .into_iter()
            .collect()
    }
}

fn data_payload(frame: &str) -> Option<String> {
    let data: Vec<&str> = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect();
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

pub fn normalize_response_event(payload: &str) -> Vec<Event> {
    if payload == "[DONE]" {
        return vec![Event::Completed];
    }
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return vec![Event::Error {
            code: "malformed_event".into(),
            message: "provider emitted malformed JSON".into(),
            retryable: false,
        }];
    };
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match event_type {
        "response.output_text.delta" | "response.text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| vec![Event::TextDelta { text: text.into() }])
            .unwrap_or_default(),
        "response.reasoning_summary_text.delta" => value
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| vec![Event::ReasoningSummary { text: text.into() }])
            .unwrap_or_default(),
        "response.completed" => vec![Event::Completed],
        "response.output_text.done" => Vec::new(),
        "response.web_search_call.completed" | "response.x_search_call.completed" => value
            .get("url")
            .and_then(Value::as_str)
            .map(|url| {
                vec![Event::Citation {
                    url: url.into(),
                    title: value
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }]
            })
            .unwrap_or_default(),
        "response.function_call_arguments.delta" => vec![Event::ToolCallDelta {
            id: value
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            arguments: value
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
        }],
        "response.usage" | "response.completed_with_usage" => vec![Event::Usage {
            input_tokens: value
                .pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize,
            output_tokens: value
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_default() as usize,
        }],
        _ if value.get("delta").and_then(Value::as_str).is_some() => vec![Event::TextDelta {
            text: value["delta"].as_str().unwrap_or_default().into(),
        }],
        _ => Vec::new(),
    }
}

/// Legacy callers can still ask for a catalog-shaped provider map, but Vera
/// no longer invents model IDs when the provider has not been discovered.
pub fn provider_catalog() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::new()
}

fn ensure_provider_origin(url: &reqwest::Url, expected: &str) -> Result<()> {
    let expected = reqwest::Url::parse(expected)?;
    if url.scheme() != expected.scheme()
        || url.host_str() != expected.host_str()
        || url.port_or_known_default() != expected.port_or_known_default()
    {
        anyhow::bail!("provider origin pinning rejected response")
    }
    Ok(())
}

fn model_array<'a>(body: &'a Value, key: &str) -> Result<&'a [Value]> {
    body.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow::anyhow!("model catalog response is missing {key}"))
}

pub fn parse_openai_models(body: &Value) -> Result<Vec<ModelInfo>> {
    let models = body
        .get("models")
        .and_then(Value::as_array)
        .or_else(|| body.get("data").and_then(Value::as_array))
        .or_else(|| body.as_array())
        .ok_or_else(|| anyhow::anyhow!("OpenAI model catalog response is missing models"))?;
    let mut parsed = models
        .iter()
        .filter_map(|item| {
            let id = item
                .get("slug")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)?
                .trim();
            if id.is_empty() || item.get("supported_in_api").and_then(Value::as_bool) == Some(false)
            {
                return None;
            }
            let supported = item
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| {
                            value
                                .as_str()
                                .or_else(|| value.get("effort").and_then(Value::as_str))
                        })
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let default_effort = item
                .get("default_reasoning_level")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some(ModelInfo {
                id: id.to_owned(),
                display_name: item
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_owned(),
                provider: ProviderKind::OpenaiCodex.as_str().into(),
                order: item.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32,
                context_window: item
                    .get("context_window")
                    .or_else(|| item.get("context_window_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(128_000) as usize,
                default_effort,
                supported_efforts: supported,
                source: "live".into(),
            })
        })
        .collect::<Vec<_>>();
    parsed.sort_by_key(|model| (model.order, model.id.clone()));
    Ok(parsed)
}

pub fn parse_xai_models(body: &Value) -> Result<Vec<ModelInfo>> {
    let models = model_array(body, "models")?;
    let mut parsed = models
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.trim();
            if id.is_empty() {
                return None;
            }
            let input_text = item
                .get("input_modalities")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("text")));
            let output_text = item
                .get("output_modalities")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some("text")));
            if !input_text || !output_text {
                return None;
            }
            let effort = xai_effort_info(id);
            Some(ModelInfo {
                id: id.to_owned(),
                display_name: id.to_owned(),
                provider: ProviderKind::XaiOauth.as_str().into(),
                order: item
                    .get("created")
                    .and_then(Value::as_i64)
                    .map(|v| -(v as i32))
                    .unwrap_or(0),
                context_window: item
                    .get("context_length")
                    .or_else(|| item.get("context_window"))
                    .and_then(Value::as_u64)
                    .unwrap_or(128_000) as usize,
                default_effort: effort.default,
                supported_efforts: effort.supported,
                source: "live".into(),
            })
        })
        .collect::<Vec<_>>();
    parsed.sort_by_key(|model| (model.order, model.id.clone()));
    Ok(parsed)
}

/// xAI's language-model endpoint advertises IDs and modalities, but not the
/// adjustable reasoning capability. Keep this small, explicit map in sync
/// with the documented Responses API contract; unknown IDs omit reasoning.
pub fn xai_effort_info(model_id: &str) -> ReasoningEffortInfo {
    let id = model_id.to_ascii_lowercase();
    if [
        "grok-4.5",
        "grok-4.5-fast",
        "grok-4.3",
        "grok-4.3-fast",
        "grok-4.1",
        "grok-4.1-fast",
        "grok-4-1-fast-reasoning",
        "grok-3-mini",
        "grok-3-mini-fast",
    ]
    .iter()
    .any(|known| id == *known)
    {
        return ReasoningEffortInfo::configurable("high", &["low", "medium", "high"]);
    }
    ReasoningEffortInfo::fixed()
}

pub fn load_cached_models(paths: &VeraPaths, provider: ProviderKind) -> Result<Vec<ModelInfo>> {
    let path = paths
        .root
        .join(format!("models-{}.json", provider.as_str()));
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path)?;
    if let Ok(catalog) = serde_json::from_slice::<ModelCatalog>(&bytes) {
        return Ok(catalog
            .models
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|mut model| {
                model.source = "cached".into();
                model
            })
            .collect());
    }
    Ok(serde_json::from_slice::<Vec<ModelInfo>>(&bytes)?
        .into_iter()
        .map(|mut model| {
            model.source = "cached".into();
            model
        })
        .collect())
}

pub fn cache_models(paths: &VeraPaths, provider: ProviderKind, models: &[ModelInfo]) -> Result<()> {
    paths.ensure_runtime_dirs()?;
    let path = paths
        .root
        .join(format!("models-{}.json", provider.as_str()));
    let temporary = paths.root.join(format!(
        "models-{}.{}.tmp",
        provider.as_str(),
        Uuid::new_v4()
    ));
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    set_private_file(&temporary)?;
    output.write_all(&serde_json::to_vec_pretty(models)?)?;
    output.sync_all()?;
    fs::rename(temporary, &path)?;
    set_private_file(&path)?;
    Ok(())
}

pub fn cache_catalog(
    paths: &VeraPaths,
    provider: ProviderKind,
    catalog: &ModelCatalog,
) -> Result<()> {
    cache_models(paths, provider, catalog.for_provider(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TextOnlyFixture;

    #[async_trait]
    impl Provider for TextOnlyFixture {
        fn kind(&self) -> ProviderKind {
            ProviderKind::XaiOauth
        }

        async fn stream(
            &self,
            _request: ProviderRequest,
            _sink: &mut dyn EventSink,
        ) -> Result<ProviderResult> {
            Ok(ProviderResult::default())
        }

        async fn models(&self) -> Result<ModelCatalog> {
            Ok(ModelCatalog::default())
        }
    }

    #[test]
    fn fragmented_sse_frames_are_reassembled() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"data: {\"type\":\"response.output_text.delta\",\"")
                .is_empty()
        );
        let frames = decoder.push(b"delta\":\"hi\"}\n\n");
        assert_eq!(
            frames,
            vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#]
        );
    }

    #[test]
    fn interleaved_tool_and_reasoning_events_normalize() {
        let tool = normalize_response_event(
            r#"{"type":"response.function_call_arguments.delta","item_id":"1","name":"read_file","delta":"{}"}"#,
        );
        assert!(
            matches!(tool.first(), Some(Event::ToolCallDelta { name, .. }) if name == "read_file")
        );
        let reasoning = normalize_response_event(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"checking"}"#,
        );
        assert!(
            matches!(reasoning.first(), Some(Event::ReasoningSummary { text }) if text == "checking")
        );
    }

    #[test]
    fn malformed_event_is_typed_error() {
        assert!(matches!(
            normalize_response_event("not-json").first(),
            Some(Event::Error { code, .. }) if code == "malformed_event"
        ));
    }

    #[test]
    fn parses_openai_reasoning_metadata_and_priority() {
        let body = serde_json::json!({
            "models": [
                {"slug":"slow","display_name":"Slow","priority":20,"default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"low","description":"Fast"},{"effort":"high","description":"Deep"}]},
                {"slug":"fast","display_name":"Fast","priority":1,"context_window":64000,"default_reasoning_level":"minimal","supported_reasoning_levels":["minimal",{"effort":"low"}]}
            ]
        });
        let models = parse_openai_models(&body).unwrap();
        assert_eq!(models[0].id, "fast");
        assert_eq!(models[0].display_name, "Fast");
        assert_eq!(models[0].context_window, 64_000);
        assert_eq!(models[0].default_effort.as_deref(), Some("minimal"));
        assert_eq!(models[0].supported_efforts, ["minimal", "low"]);
    }

    #[test]
    fn filters_xai_to_text_models_and_omits_unknown_effort() {
        let body = serde_json::json!({
            "models": [
                {"id":"grok-4.5","input_modalities":["text"],"output_modalities":["text"]},
                {"id":"grok-imagine-image","input_modalities":["text"],"output_modalities":["image"]},
                {"id":"future-grok","input_modalities":["text"],"output_modalities":["text"]}
            ]
        });
        let models = parse_xai_models(&body).unwrap();
        assert_eq!(models.len(), 2);
        let known = models.iter().find(|model| model.id == "grok-4.5").unwrap();
        assert_eq!(known.supported_efforts, ["low", "medium", "high"]);
        let unknown = models
            .iter()
            .find(|model| model.id == "future-grok")
            .unwrap();
        assert!(unknown.supported_efforts.is_empty());
        assert!(unknown.default_effort.is_none());
    }

    #[test]
    fn request_body_sets_store_false_and_only_explicit_reasoning() {
        let provider = ResponsesProvider::new(
            ProviderKind::XaiOauth,
            TokenRecord {
                provider: AuthProvider::XaiOauth,
                access_token: "access".into(),
                refresh_token: None,
                expires_at: None,
                account_id: None,
                token_type: "Bearer".into(),
                xai_token_endpoint: None,
            },
        )
        .unwrap();
        let request = ProviderRequest {
            model: "grok-4.5".into(),
            input: vec![ProviderInput::message("user", "hello")],
            tools: Vec::new(),
            instructions: "instructions".into(),
            effort: Some("low".into()),
        };
        let body = provider.body(&request);
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "low");

        let mut fixed = request;
        fixed.model = "future-grok".into();
        fixed.effort = None;
        assert!(fixed.effort.is_none());
        assert!(provider.body(&fixed).get("reasoning").is_none());
    }

    #[test]
    fn request_body_filters_provider_native_tools() {
        fn provider(kind: ProviderKind) -> ResponsesProvider {
            ResponsesProvider::new(
                kind,
                TokenRecord {
                    provider: kind.auth_provider(),
                    access_token: "access".into(),
                    refresh_token: None,
                    expires_at: None,
                    account_id: None,
                    token_type: "Bearer".into(),
                    xai_token_endpoint: None,
                },
            )
            .unwrap()
        }

        let request = ProviderRequest {
            model: "model".into(),
            input: vec![ProviderInput::message("user", "hello")],
            tools: vec![
                ToolSchema {
                    name: "web_search".into(),
                    description: "web".into(),
                    parameters: json!({"type":"object"}),
                },
                ToolSchema {
                    name: "x_search".into(),
                    description: "x".into(),
                    parameters: json!({"type":"object"}),
                },
                ToolSchema {
                    name: "read_file".into(),
                    description: "read".into(),
                    parameters: json!({"type":"object"}),
                },
            ],
            instructions: "instructions".into(),
            effort: None,
        };

        let openai = provider(ProviderKind::OpenaiCodex).body(&request);
        assert_eq!(openai["tools"].as_array().unwrap().len(), 2);
        assert_eq!(
            openai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|tool| tool["type"] == "web_search")
                .count(),
            1
        );
        assert!(
            !openai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "web_search_preview" })
        );
        assert!(
            !openai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "x_search" })
        );
        assert!(
            openai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "read_file" })
        );

        let xai = provider(ProviderKind::XaiOauth).body(&request);
        assert_eq!(xai["tools"].as_array().unwrap().len(), 2);
        assert!(
            xai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "x_search" })
        );
        assert!(
            !xai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "web_search_preview" })
        );
        assert!(
            xai["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "read_file" })
        );
    }

    #[test]
    fn provider_error_details_are_redacted_bounded_and_never_empty() {
        assert_eq!(
            safe_provider_detail_text("   "),
            "provider returned no error details"
        );

        let redacted =
            safe_provider_detail_text(r#"{"error":"bad request","access_token":"super-secret"}"#);
        assert!(redacted.contains("bad request"));
        assert!(!redacted.contains("super-secret"));

        let long = safe_provider_detail_text(&"x".repeat(600));
        assert_eq!(long.chars().count(), 513);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn openai_model_discovery_uses_account_scoped_auth_headers() {
        let provider = ResponsesProvider::new(
            ProviderKind::OpenaiCodex,
            TokenRecord {
                provider: AuthProvider::OpenaiCodex,
                access_token: "access".into(),
                refresh_token: None,
                expires_at: None,
                account_id: Some("account".into()),
                token_type: "Bearer".into(),
                xai_token_endpoint: None,
            },
        )
        .unwrap();
        let url = provider.model_url().unwrap();
        assert_eq!(url.path(), "/backend-api/codex/models");
        assert_eq!(
            url.query(),
            Some(format!("client_version={OPENAI_CODEX_MODEL_CLIENT_VERSION}").as_str())
        );
        let request = provider
            .add_auth_headers(provider.http.get(url))
            .build()
            .unwrap();
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .unwrap(),
            "Bearer access"
        );
        assert_eq!(
            request.headers().get("chatgpt-account-id").unwrap(),
            "account"
        );
        assert_eq!(request.headers().get("originator").unwrap(), "codex_cli_rs");
    }

    #[test]
    fn cached_catalog_is_atomic_and_labeled() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let model = ModelInfo {
            id: "grok-4.5".into(),
            display_name: "Grok 4.5".into(),
            provider: "xai-oauth".into(),
            order: 0,
            context_window: 128_000,
            default_effort: Some("high".into()),
            supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
            source: "live".into(),
        };
        cache_models(&paths, ProviderKind::XaiOauth, &[model]).unwrap();
        let cached = load_cached_models(&paths, ProviderKind::XaiOauth).unwrap();
        assert_eq!(cached[0].source, "cached");
        assert!(paths.root.join("models-xai-oauth.json").is_file());
    }

    #[test]
    fn image_inputs_use_provider_neutral_parts_and_data_urls() {
        let provider = ResponsesProvider::new(
            ProviderKind::XaiOauth,
            TokenRecord {
                provider: AuthProvider::XaiOauth,
                access_token: "access".into(),
                refresh_token: None,
                expires_at: None,
                account_id: None,
                token_type: "Bearer".into(),
                xai_token_endpoint: None,
            },
        )
        .unwrap();
        let request = ProviderRequest {
            model: "grok-4.5".into(),
            input: vec![ProviderInput::image_message(
                "user",
                "inspect",
                "image/png",
                "AA==",
            )],
            tools: Vec::new(),
            instructions: String::new(),
            effort: None,
        };
        let body = provider.body(&request);
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AA=="
        );
        assert!(provider.supports_image_input());
    }

    #[test]
    fn text_only_provider_declares_image_capability_false() {
        assert!(!TextOnlyFixture.supports_image_input());
    }
}
