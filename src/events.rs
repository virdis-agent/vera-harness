use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::OutputFormat;
use crate::sessions::DisplayMode;

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
    display_mode: DisplayMode,
    tool_started: BTreeMap<String, Instant>,
    minimal_tools: BTreeMap<String, usize>,
}

impl TerminalEventSink {
    pub fn new(format: OutputFormat) -> Self {
        Self {
            format,
            stdout: io::stdout(),
            display_mode: DisplayMode::Detailed,
            tool_started: BTreeMap::new(),
            minimal_tools: BTreeMap::new(),
        }
    }

    pub fn with_display(format: OutputFormat, display_mode: DisplayMode) -> Self {
        Self {
            format,
            stdout: io::stdout(),
            display_mode,
            tool_started: BTreeMap::new(),
            minimal_tools: BTreeMap::new(),
        }
    }

    pub fn tool_started(&mut self, id: &str, name: &str, arguments: &Value) -> anyhow::Result<()> {
        if self.format != OutputFormat::Text || !self.stdout.is_terminal() {
            return Ok(());
        }
        self.tool_started.insert(id.to_owned(), Instant::now());
        let summary = summarize_arguments(name, arguments);
        match self.display_mode {
            DisplayMode::Grouped => eprintln!("\n● {}{}", tool_label(name), prefixed(&summary)),
            DisplayMode::Minimal => {
                *self.minimal_tools.entry(name.to_owned()).or_default() += 1;
            }
            DisplayMode::Detailed => eprintln!("\n⚡ {name}{}", prefixed(&summary)),
        }
        Ok(())
    }

    pub fn tool_finished(
        &mut self,
        id: &str,
        name: &str,
        output: &str,
        error: bool,
    ) -> anyhow::Result<()> {
        if self.format != OutputFormat::Text || !self.stdout.is_terminal() {
            return Ok(());
        }
        let elapsed = self
            .tool_started
            .remove(id)
            .map_or(0.0, |started| started.elapsed().as_secs_f64());
        match self.display_mode {
            DisplayMode::Grouped => {
                let preview = first_line(output, 72);
                eprintln!(
                    "  └ {}{} ({elapsed:.1}s)",
                    if error { "error: " } else { "" },
                    preview
                );
            }
            DisplayMode::Minimal => {}
            DisplayMode::Detailed => {
                eprintln!("{} {name} ({elapsed:.1}s)", if error { "✗" } else { "✓" });
            }
        }
        Ok(())
    }

    fn flush_minimal(&mut self) {
        if self.minimal_tools.is_empty() {
            return;
        }
        let summary = self
            .minimal_tools
            .iter()
            .map(|(name, count)| format!("{count} {name}"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("\n  tools: {summary}");
        self.minimal_tools.clear();
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
                    self.flush_minimal();
                    self.stdout.write_all(text.as_bytes())?;
                    self.stdout.flush()?;
                }
                Event::ReasoningSummary { text } => eprintln!("\n[reasoning] {text}"),
                Event::ToolCallDelta { .. } => {}
                Event::Citation { url, .. } => eprintln!("\n[source] {url}"),
                Event::Usage {
                    input_tokens,
                    output_tokens,
                } => eprintln!("\n[usage] input={input_tokens} output={output_tokens}"),
                Event::Completed => {
                    self.flush_minimal();
                    self.stdout.write_all(b"\n")?;
                }
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

fn tool_label(name: &str) -> &'static str {
    match name {
        "shell" => "Ran",
        "read_file" => "Read",
        "write_file" => "Wrote",
        "edit_file" => "Edited",
        "glob" => "Explored",
        "grep" => "Searched",
        "list_dir" => "Listed",
        "web_search" => "Fetched",
        _ => "Used",
    }
}

fn summarize_arguments(name: &str, arguments: &Value) -> String {
    let key = match name {
        "shell" => "command",
        "read_file" | "write_file" | "edit_file" | "list_dir" => "path",
        "glob" | "grep" => "pattern",
        "web_search" => "query",
        _ => arguments
            .as_object()
            .and_then(|object| object.keys().next())
            .map_or("", String::as_str),
    };
    arguments
        .get(key)
        .map(|value| {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        })
        .map(|value| first_line(&value, 64))
        .unwrap_or_default()
}

fn prefixed(value: &str) -> String {
    if value.is_empty() {
        String::new()
    } else {
        format!(" {value}")
    }
}

fn first_line(value: &str, limit: usize) -> String {
    let line = value.lines().next().unwrap_or_default();
    if line.chars().count() <= limit {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(limit).collect::<String>())
    }
}

pub fn event_to_value(event: &Event) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}
