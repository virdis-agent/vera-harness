use std::io::{self, Write};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::OutputFormat;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TextDelta {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolCallDelta {
        id: String,
        name: String,
        arguments: String,
    },
    Citation {
        url: String,
        title: Option<String>,
    },
    Usage {
        input_tokens: usize,
        output_tokens: usize,
    },
    Completed,
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
    Status {
        message: String,
    },
    NeedsInput {
        question_id: String,
        prompt: String,
        choices: Vec<String>,
    },
}

#[async_trait]
pub trait EventSink: Send {
    async fn emit(&mut self, event: Event) -> anyhow::Result<()>;
}

pub struct TerminalEventSink {
    format: OutputFormat,
    stdout: io::Stdout,
}

impl TerminalEventSink {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            stdout: io::stdout(),
        }
    }
}

#[async_trait]
impl EventSink for TerminalEventSink {
    async fn emit(&mut self, event: Event) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Jsonl => {
                serde_json::to_writer(&mut self.stdout, &event)?;
                self.stdout.write_all(b"\n")?;
            }
            OutputFormat::Text => match event {
                Event::TextDelta { text } => {
                    self.stdout.write_all(text.as_bytes())?;
                    self.stdout.flush()?;
                }
                Event::ReasoningSummary { text } => eprintln!("\n[reasoning] {text}"),
                Event::ToolCallDelta { name, .. } => eprintln!("\n[tool] {name}"),
                Event::Citation { url, .. } => eprintln!("\n[source] {url}"),
                Event::Usage {
                    input_tokens,
                    output_tokens,
                } => eprintln!("\n[usage] input={input_tokens} output={output_tokens}"),
                Event::Completed => self.stdout.write_all(b"\n")?,
                Event::Error { message, .. } => eprintln!("\n[error] {message}"),
                Event::Status { message } => eprintln!("[vera] {message}"),
                Event::NeedsInput {
                    prompt, choices, ..
                } => {
                    eprintln!("\n[input required] {prompt}");
                    if !choices.is_empty() {
                        eprintln!("choices: {}", choices.join(" | "));
                    }
                }
            },
        }
        Ok(())
    }
}

pub fn event_to_value(event: &Event) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}
