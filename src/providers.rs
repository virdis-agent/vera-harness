use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{AuthProvider, TokenRecord};
use crate::error::VeraError;
use crate::events::{Event, EventSink};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug)]
pub struct ProviderRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSchema>,
    pub instructions: String,
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
    async fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResult>;
    async fn models(&self) -> Result<Vec<String>>;
}

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
            ProviderKind::OpenaiCodex => "https://chatgpt.com/backend-api/models",
            ProviderKind::XaiOauth => "https://api.x.ai/v1/models",
        }
    }

    fn body(&self, request: &ProviderRequest) -> Value {
        let input: Vec<Value> = request
            .messages
            .iter()
            .map(|message| json!({"role": message.role, "content": message.content}))
            .collect();
        let mut body = json!({"model": request.model, "instructions": request.instructions, "input": input, "stream": true});
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| match tool.name.as_str() {
                        "web_search" => json!({"type":"web_search_preview"}),
                        "x_search" => json!({"type":"x_search"}),
                        _ => json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.parameters}),
                    })
                    .collect(),
            );
        }
        body
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResult> {
        let mut builder = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.token.access_token)
            .header(CONTENT_TYPE, "application/json")
            .json(&self.body(&request));
        if let Some(account_id) = &self.token.account_id {
            builder = builder.header("chatgpt-account-id", account_id);
        }
        let response = builder.send().await.context("provider request")?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(VeraError::Provider(
                "provider rejected credentials; retry after refresh".into(),
            )
            .into());
        }
        if !response.status().is_success() {
            return Err(
                VeraError::Provider(format!("provider returned {}", response.status())).into(),
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

    async fn models(&self) -> Result<Vec<String>> {
        let response = self
            .http
            .get(self.model_endpoint())
            .bearer_auth(&self.token.access_token)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(VeraError::Provider(format!(
                "model discovery returned {}",
                response.status()
            ))
            .into());
        }
        let body: Value = response.json().await?;
        Ok(body
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect())
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

pub fn provider_catalog() -> BTreeMap<&'static str, Vec<&'static str>> {
    BTreeMap::from([
        ("openai-codex", vec!["gpt-5.6"]),
        ("xai-oauth", vec!["grok-4.5"]),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
