use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::VeraPaths;
use crate::prompt::approximate_tokens;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    pub version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub root: PathBuf,
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Header(SessionHeader),
    Message(Message),
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        result: Option<String>,
    },
    Approval {
        action: String,
        scope: String,
        granted: bool,
    },
    FilePreimage {
        path: PathBuf,
        content: Option<String>,
        existed: bool,
    },
    Compaction {
        summary: String,
        preserved_messages: usize,
        context_tokens: usize,
    },
    Event {
        event: serde_json::Value,
    },
}

pub struct Session {
    pub header: SessionHeader,
    pub path: PathBuf,
    pub messages: Vec<Message>,
    preimages: BTreeMap<PathBuf, Option<String>>,
}

pub struct SessionStore {
    paths: VeraPaths,
}

impl SessionStore {
    pub fn new(paths: VeraPaths) -> Self {
        Self { paths }
    }

    pub fn create(&self, root: PathBuf, provider: String, model: String) -> Result<Session> {
        self.paths.ensure_runtime_dirs()?;
        let header = SessionHeader {
            version: 1,
            id: Uuid::new_v4().simple().to_string(),
            created_at: Utc::now(),
            root,
            provider,
            model,
        };
        let path = self.paths.session_file(&header.id);
        let mut session = Session {
            header,
            path,
            messages: Vec::new(),
            preimages: BTreeMap::new(),
        };
        session.append(SessionRecord::Header(session.header.clone()))?;
        Ok(session)
    }

    pub fn open(&self, id: &str) -> Result<Session> {
        let path = self.paths.session_file(id);
        if !path.exists() {
            anyhow::bail!("session {id} does not exist");
        }
        let file = File::open(&path)?;
        let mut header = None;
        let mut messages = Vec::new();
        let mut preimages = BTreeMap::new();
        for line in BufReader::new(file).lines() {
            let record: SessionRecord = serde_json::from_str(&line?)?;
            match record {
                SessionRecord::Header(value) => header = Some(value),
                SessionRecord::Message(message) => messages.push(message),
                SessionRecord::FilePreimage { path, content, .. } => {
                    preimages.insert(path, content);
                }
                _ => {}
            }
        }
        let header = header.context("session has no header")?;
        Ok(Session {
            header,
            path,
            messages,
            preimages,
        })
    }

    pub fn list(&self) -> Result<Vec<SessionHeader>> {
        if !self.paths.sessions.exists() {
            return Ok(Vec::new());
        }
        let mut headers = Vec::new();
        for entry in fs::read_dir(&self.paths.sessions)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "jsonl")
                && let Ok(session) = self.open(
                    entry
                        .path()
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                )
            {
                headers.push(session.header);
            }
        }
        headers.sort_by_key(|header| std::cmp::Reverse(header.created_at));
        Ok(headers)
    }
}

impl Session {
    pub fn append(&mut self, record: SessionRecord) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        if let SessionRecord::Message(message) = record {
            self.messages.push(message);
        }
        Ok(())
    }

    pub fn add_message(
        &mut self,
        role: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<()> {
        self.append(SessionRecord::Message(Message {
            role: role.into(),
            content: content.into(),
        }))
    }

    pub fn record_preimage(&mut self, path: PathBuf, content: Option<String>) -> Result<()> {
        if self.preimages.contains_key(&path) {
            return Ok(());
        }
        let existed = content.is_some();
        self.preimages.insert(path.clone(), content.clone());
        self.append(SessionRecord::FilePreimage {
            path,
            content,
            existed,
        })
    }

    pub fn compact_if_needed(&mut self, context_limit: usize) -> Result<bool> {
        if self.context_tokens() < context_limit.saturating_mul(80) / 100 {
            return Ok(false);
        }
        self.compact(context_limit)
    }

    pub fn context_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|message| approximate_tokens(&message.content) + 4)
            .sum()
    }

    pub fn compact(&mut self, context_limit: usize) -> Result<bool> {
        if self.messages.len() <= 6 {
            return Ok(false);
        }
        let preserve_from = self.messages.len().saturating_sub(6);
        let old = self.messages[..preserve_from].to_vec();
        let summary = summarize(&old);
        let recent = self.messages[preserve_from..].to_vec();
        let mut next = vec![Message {
            role: "system".into(),
            content: format!("Compacted context (retain these facts):\n{summary}"),
        }];
        next.extend(recent);
        self.messages = next;
        let tokens = self.context_tokens();
        self.append(SessionRecord::Compaction {
            summary,
            preserved_messages: self.messages.len(),
            context_tokens: tokens.min(context_limit),
        })?;
        Ok(true)
    }

    pub fn undo(&self) -> Result<usize> {
        let mut restored = 0;
        for (path, content) in &self.preimages {
            match content {
                Some(content) => {
                    fs::write(path, content)?;
                }
                None => {
                    if path.exists() {
                        fs::remove_file(path)?;
                    }
                }
            }
            restored += 1;
        }
        Ok(restored)
    }
}

fn summarize(messages: &[Message]) -> String {
    let mut summary = String::new();
    for message in messages {
        let content = message.content.replace('\n', " ");
        let clipped = content.chars().take(500).collect::<String>();
        summary.push_str(&format!("{}: {}\n", message.role, clipped));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_keeps_six_recent_exchanges() {
        let directory = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let mut session = SessionStore::new(paths)
            .create(
                directory.path().to_path_buf(),
                "test".into(),
                "model".into(),
            )
            .unwrap();
        for index in 0..12 {
            session
                .add_message("user", format!("message {index}"))
                .unwrap();
        }
        assert!(session.compact(10).unwrap());
        assert!(session.messages.len() <= 7);
    }
}
