use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::paths::VeraPaths;
use crate::prompt::approximate_tokens;
use crate::providers::ProviderInput;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHeader {
    #[serde(default = "default_session_version")]
    pub version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub root: PathBuf,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default = "default_context_capacity")]
    pub context_capacity: usize,
}

fn default_session_version() -> u32 {
    1
}

fn default_context_capacity() -> usize {
    128_000
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSettings {
    #[serde(default)]
    pub provider: String,
    #[serde(default = "default_session_model")]
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

fn default_session_model() -> String {
    "auto".into()
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: default_session_model(),
            effort: None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitySelection {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
    pub model_context_window: usize,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub enabled_plugins: Vec<String>,
    #[serde(default)]
    pub enabled_mcp: Vec<String>,
    #[serde(default)]
    pub loaded_skills: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageState {
    pub latest_input_tokens: Option<usize>,
    pub latest_output_tokens: Option<usize>,
    pub local_estimate: usize,
    pub authoritative: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingQuestion {
    pub question_id: String,
    pub prompt: String,
    #[serde(default)]
    pub choices: Vec<String>,
    /// The provider continuation is persisted so resuming an answered
    /// question does not repeat already-completed tool calls.
    #[serde(default)]
    pub continuation: Vec<ProviderInput>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepState {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanStep {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub state: PlanStepState,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPlan {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LifecycleState {
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Header(SessionHeader),
    #[serde(alias = "SessionSettings")]
    Settings(SessionSettings),
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
        #[serde(default)]
        content_base64: Option<String>,
    },
    Compaction {
        summary: String,
        preserved_messages: usize,
        context_tokens: usize,
    },
    Selection(CapabilitySelection),
    Usage(UsageState),
    SkillState {
        name: String,
        loaded: bool,
    },
    McpState {
        name: String,
        active: bool,
    },
    PendingQuestion(PendingQuestion),
    QuestionAnswered {
        question_id: String,
        answer: String,
    },
    Plan(SessionPlan),
    ProcessLifecycle {
        process_id: String,
        state: LifecycleState,
    },
    McpLifecycle {
        server: String,
        state: LifecycleState,
    },
    SubagentLifecycle {
        agent_id: String,
        state: LifecycleState,
    },
    WorktreeState {
        worktree_id: String,
        state: LifecycleState,
    },
    MergeDecision {
        worktree_id: String,
        decision: String,
        detail: String,
    },
    Hook {
        name: String,
        event: String,
        output: String,
        success: bool,
    },
    Event {
        event: serde_json::Value,
    },
}

pub struct Session {
    pub header: SessionHeader,
    pub path: PathBuf,
    pub messages: Vec<Message>,
    pub selection: CapabilitySelection,
    pub settings: SessionSettings,
    pub usage: UsageState,
    pub pending_question: Option<PendingQuestion>,
    pub plan: SessionPlan,
    pub process_lifecycle: BTreeMap<String, LifecycleState>,
    pub mcp_lifecycle: BTreeMap<String, LifecycleState>,
    pub subagent_lifecycle: BTreeMap<String, LifecycleState>,
    pub worktree_lifecycle: BTreeMap<String, LifecycleState>,
    pub merge_decisions: BTreeMap<String, String>,
    preimages: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

pub struct SessionStore {
    paths: VeraPaths,
}

impl SessionStore {
    pub fn new(paths: VeraPaths) -> Self {
        Self { paths }
    }

    pub fn create(&self, root: PathBuf, provider: String, model: String) -> Result<Session> {
        self.create_with_selection(
            root,
            CapabilitySelection {
                provider,
                model,
                model_context_window: default_context_capacity(),
                ..CapabilitySelection::default()
            },
        )
    }

    pub fn create_with_selection(
        &self,
        root: PathBuf,
        selection: CapabilitySelection,
    ) -> Result<Session> {
        self.paths.ensure_runtime_dirs()?;
        let header = SessionHeader {
            version: 2,
            id: Uuid::new_v4().simple().to_string(),
            created_at: Utc::now(),
            root,
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            effort: selection.effort.clone(),
            context_capacity: selection.model_context_window,
        };
        let path = self.paths.session_file(&header.id);
        let mut session = Session {
            header,
            path,
            messages: Vec::new(),
            selection: selection.clone(),
            settings: SessionSettings {
                provider: selection.provider.clone(),
                model: selection.model.clone(),
                effort: selection.effort.clone(),
            },
            usage: UsageState::default(),
            pending_question: None,
            plan: SessionPlan::default(),
            process_lifecycle: BTreeMap::new(),
            mcp_lifecycle: BTreeMap::new(),
            subagent_lifecycle: BTreeMap::new(),
            worktree_lifecycle: BTreeMap::new(),
            merge_decisions: BTreeMap::new(),
            preimages: BTreeMap::new(),
        };
        session.append(SessionRecord::Header(session.header.clone()))?;
        session.append(SessionRecord::Selection(selection))?;
        session.append(SessionRecord::Settings(session.settings.clone()))?;
        Ok(session)
    }

    pub fn open(&self, id: &str) -> Result<Session> {
        validate_session_id(id)?;
        let path = self.paths.session_file(id);
        if !path.exists() {
            anyhow::bail!("session {id} does not exist");
        }
        let file = File::open(&path)?;
        let mut header = None;
        let mut messages = Vec::new();
        let mut preimages = BTreeMap::new();
        let mut selection = None;
        let mut settings = None;
        let mut usage = UsageState::default();
        let mut pending_question = None;
        let mut plan = SessionPlan::default();
        let mut process_lifecycle = BTreeMap::new();
        let mut mcp_lifecycle = BTreeMap::new();
        let mut subagent_lifecycle = BTreeMap::new();
        let mut worktree_lifecycle = BTreeMap::new();
        let mut merge_decisions = BTreeMap::new();
        for line in BufReader::new(file).lines() {
            let record: SessionRecord = serde_json::from_str(&line?)?;
            match record {
                SessionRecord::Header(value) => header = Some(value),
                SessionRecord::Settings(value) => settings = Some(value),
                SessionRecord::Message(message) => messages.push(message),
                SessionRecord::FilePreimage {
                    path,
                    content,
                    content_base64,
                    ..
                } => {
                    let bytes = match content_base64 {
                        Some(encoded) => Some(
                            base64::engine::general_purpose::STANDARD
                                .decode(encoded)
                                .context("invalid binary preimage")?,
                        ),
                        None => content.map(String::into_bytes),
                    };
                    preimages.insert(path, bytes);
                }
                SessionRecord::Selection(value) => selection = Some(value),
                SessionRecord::Usage(value) => usage = value,
                SessionRecord::PendingQuestion(value) => pending_question = Some(value),
                SessionRecord::QuestionAnswered { .. } => pending_question = None,
                SessionRecord::Plan(value) => plan = value,
                SessionRecord::ProcessLifecycle { process_id, state } => {
                    process_lifecycle.insert(process_id, state);
                }
                SessionRecord::McpLifecycle { server, state } => {
                    mcp_lifecycle.insert(server, state);
                }
                SessionRecord::SubagentLifecycle { agent_id, state } => {
                    subagent_lifecycle.insert(agent_id, state);
                }
                SessionRecord::WorktreeState { worktree_id, state } => {
                    worktree_lifecycle.insert(worktree_id, state);
                }
                SessionRecord::MergeDecision {
                    worktree_id,
                    decision,
                    ..
                } => {
                    merge_decisions.insert(worktree_id, decision);
                }
                _ => {}
            }
        }
        let header = header.context("session has no header")?;
        let selection = selection.unwrap_or(CapabilitySelection {
            provider: header.provider.clone(),
            model: header.model.clone(),
            model_context_window: header.context_capacity,
            ..CapabilitySelection::default()
        });
        let settings = settings.unwrap_or_else(|| SessionSettings {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            effort: selection.effort.clone(),
        });
        Ok(Session {
            header,
            path,
            messages,
            selection,
            settings,
            usage,
            pending_question,
            plan,
            process_lifecycle,
            mcp_lifecycle,
            subagent_lifecycle,
            worktree_lifecycle,
            merge_decisions,
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

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        anyhow::bail!("invalid session id");
    }
    Ok(())
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

    pub fn set_selection(&mut self, selection: CapabilitySelection) -> Result<()> {
        self.header.provider = selection.provider.clone();
        self.header.model = selection.model.clone();
        self.header.effort = selection.effort.clone();
        self.header.context_capacity = selection.model_context_window;
        self.selection = selection.clone();
        self.settings = SessionSettings {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            effort: selection.effort.clone(),
        };
        self.append(SessionRecord::Selection(selection))?;
        self.append(SessionRecord::Settings(self.settings.clone()))
    }

    pub fn record_skill_state(&mut self, name: &str, loaded: bool) -> Result<()> {
        if loaded {
            if !self.selection.loaded_skills.contains(&name.to_owned()) {
                self.selection.loaded_skills.push(name.to_owned());
            }
        } else {
            self.selection.loaded_skills.retain(|value| value != name);
        }
        self.append(SessionRecord::SkillState {
            name: name.into(),
            loaded,
        })?;
        self.append(SessionRecord::Selection(self.selection.clone()))
    }

    pub fn record_mcp_state(&mut self, name: &str, active: bool) -> Result<()> {
        if active {
            if !self.selection.enabled_mcp.contains(&name.to_owned()) {
                self.selection.enabled_mcp.push(name.to_owned());
            }
        } else {
            self.selection.enabled_mcp.retain(|value| value != name);
        }
        self.append(SessionRecord::McpState {
            name: name.into(),
            active,
        })?;
        self.append(SessionRecord::Selection(self.selection.clone()))
    }

    pub fn record_pending_question(
        &mut self,
        question_id: impl Into<String>,
        prompt: impl Into<String>,
        choices: Vec<String>,
        continuation: Vec<ProviderInput>,
    ) -> Result<()> {
        let question = PendingQuestion {
            question_id: question_id.into(),
            prompt: prompt.into().chars().take(4_000).collect(),
            choices: choices
                .into_iter()
                .take(4)
                .map(|choice| choice.chars().take(200).collect())
                .collect(),
            continuation,
        };
        self.pending_question = Some(question.clone());
        self.append(SessionRecord::PendingQuestion(question))
    }

    pub fn answer_pending_question(
        &mut self,
        answer: impl Into<String>,
    ) -> Result<Vec<ProviderInput>> {
        let question = self
            .pending_question
            .take()
            .context("no pending question")?;
        let answer: String = answer.into().chars().take(4_000).collect();
        self.append(SessionRecord::QuestionAnswered {
            question_id: question.question_id,
            answer,
        })?;
        Ok(question.continuation)
    }

    pub fn record_plan(&mut self, mut plan: SessionPlan) -> Result<SessionPlan> {
        if plan.steps.len() > 32 {
            anyhow::bail!("plan cannot contain more than 32 steps");
        }
        for (index, step) in plan.steps.iter_mut().enumerate() {
            step.id = if step.id.trim().is_empty() {
                (index + 1).to_string()
            } else {
                step.id.chars().take(64).collect()
            };
            step.text = step.text.chars().take(1_000).collect();
        }
        plan.version = self.plan.version.saturating_add(1).max(1);
        self.plan = plan.clone();
        self.append(SessionRecord::Plan(plan.clone()))?;
        Ok(plan)
    }

    pub fn plan_context(&self) -> String {
        if self.plan.steps.is_empty() {
            return "No active plan.".into();
        }
        let context = self
            .plan
            .steps
            .iter()
            .map(|step| format!("- [{}] {}", format_step_state(&step.state), step.text))
            .collect::<Vec<_>>()
            .join("\n");
        context.chars().take(16_000).collect()
    }

    pub fn record_process_lifecycle(
        &mut self,
        process_id: impl Into<String>,
        state: LifecycleState,
    ) -> Result<()> {
        let process_id = process_id.into();
        self.process_lifecycle
            .insert(process_id.clone(), state.clone());
        self.append(SessionRecord::ProcessLifecycle { process_id, state })
    }

    pub fn record_mcp_lifecycle(
        &mut self,
        server: impl Into<String>,
        state: LifecycleState,
    ) -> Result<()> {
        let server = server.into();
        self.mcp_lifecycle.insert(server.clone(), state.clone());
        self.append(SessionRecord::McpLifecycle { server, state })
    }

    pub fn record_subagent_lifecycle(
        &mut self,
        agent_id: impl Into<String>,
        state: LifecycleState,
    ) -> Result<()> {
        let agent_id = agent_id.into();
        self.subagent_lifecycle
            .insert(agent_id.clone(), state.clone());
        self.append(SessionRecord::SubagentLifecycle { agent_id, state })
    }

    pub fn record_worktree_state(
        &mut self,
        worktree_id: impl Into<String>,
        state: LifecycleState,
    ) -> Result<()> {
        let worktree_id = worktree_id.into();
        self.worktree_lifecycle
            .insert(worktree_id.clone(), state.clone());
        self.append(SessionRecord::WorktreeState { worktree_id, state })
    }

    pub fn record_merge_decision(
        &mut self,
        worktree_id: impl Into<String>,
        decision: impl Into<String>,
        detail: impl Into<String>,
    ) -> Result<()> {
        let worktree_id = worktree_id.into();
        let decision = decision.into();
        self.merge_decisions
            .insert(worktree_id.clone(), decision.clone());
        self.append(SessionRecord::MergeDecision {
            worktree_id,
            decision,
            detail: detail.into(),
        })
    }

    pub fn record_usage(
        &mut self,
        input_tokens: usize,
        output_tokens: usize,
        local_estimate: usize,
    ) -> Result<()> {
        self.usage = UsageState {
            latest_input_tokens: Some(input_tokens),
            latest_output_tokens: Some(output_tokens),
            local_estimate,
            authoritative: true,
        };
        self.append(SessionRecord::Usage(self.usage.clone()))
    }

    pub fn record_estimate(&mut self, estimate: usize) -> Result<()> {
        self.usage = UsageState {
            latest_input_tokens: None,
            latest_output_tokens: None,
            local_estimate: estimate,
            authoritative: false,
        };
        self.append(SessionRecord::Usage(self.usage.clone()))
    }

    pub fn record_preimage(&mut self, path: PathBuf, content: Option<String>) -> Result<()> {
        self.record_binary_preimage(path, content.map(String::into_bytes))
    }

    pub fn record_binary_preimage(
        &mut self,
        path: PathBuf,
        content: Option<Vec<u8>>,
    ) -> Result<()> {
        if self.preimages.contains_key(&path) {
            return Ok(());
        }
        let existed = content.is_some();
        let text = content
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok().map(ToOwned::to_owned));
        let content_base64 = content
            .as_ref()
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes));
        self.preimages.insert(path.clone(), content);
        self.append(SessionRecord::FilePreimage {
            path,
            content: text,
            existed,
            content_base64,
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

    pub fn context_display(&self) -> (usize, bool) {
        (
            self.usage
                .latest_input_tokens
                .unwrap_or_else(|| self.usage.local_estimate.max(self.context_tokens())),
            self.usage.authoritative,
        )
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

    pub fn undo(&self, root: &Path) -> Result<usize> {
        let mut restored = 0;
        for (recorded_path, content) in &self.preimages {
            let path = crate::paths::safe_join(root, recorded_path)?;
            match content {
                Some(content) => {
                    fs::write(&path, content)?;
                }
                None => {
                    if path.exists() {
                        fs::remove_file(&path)?;
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

fn format_step_state(state: &PlanStepState) -> &'static str {
    match state {
        PlanStepState::Pending => "pending",
        PlanStepState::InProgress => "in_progress",
        PlanStepState::Completed => "completed",
    }
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

    #[test]
    fn resume_restores_capability_selection_and_usage() {
        let directory = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let store = SessionStore::new(paths);
        let mut session = store
            .create_with_selection(
                directory.path().to_path_buf(),
                CapabilitySelection {
                    provider: "openai-codex".into(),
                    model: "gpt-5.6".into(),
                    model_context_window: 128_000,
                    loaded_skills: vec!["review".into()],
                    ..CapabilitySelection::default()
                },
            )
            .unwrap();
        session.record_usage(42, 7, 100).unwrap();
        let resumed = store.open(&session.header.id).unwrap();
        assert_eq!(resumed.selection.loaded_skills, vec!["review"]);
        assert_eq!(resumed.usage.latest_input_tokens, Some(42));
        assert_eq!(resumed.context_display(), (42, true));
    }

    #[test]
    fn session_settings_round_trip_and_legacy_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let store = SessionStore::new(paths);
        let mut session = store
            .create_with_selection(
                directory.path().to_path_buf(),
                CapabilitySelection {
                    provider: "xai-oauth".into(),
                    model: "grok-4.5".into(),
                    model_context_window: 128_000,
                    effort: Some("low".into()),
                    ..CapabilitySelection::default()
                },
            )
            .unwrap();
        session
            .set_selection(CapabilitySelection {
                effort: Some("high".into()),
                ..session.selection.clone()
            })
            .unwrap();
        let resumed = store.open(&session.header.id).unwrap();
        assert_eq!(resumed.settings.effort.as_deref(), Some("high"));
        assert_eq!(SessionSettings::default().model, "auto");
    }

    #[test]
    fn pending_question_and_versioned_plan_resume_without_repeating_calls() {
        let directory = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let store = SessionStore::new(paths);
        let mut session = store
            .create(
                directory.path().to_path_buf(),
                "test".into(),
                "model".into(),
            )
            .unwrap();
        session
            .record_plan(SessionPlan {
                version: 0,
                steps: vec![PlanStep {
                    id: String::new(),
                    text: "inspect fixture".into(),
                    state: PlanStepState::InProgress,
                }],
            })
            .unwrap();
        session
            .record_pending_question(
                "q1",
                "Which fixture?",
                vec!["one".into(), "two".into()],
                vec![ProviderInput::message("assistant", "tool call complete")],
            )
            .unwrap();
        let resumed = store.open(&session.header.id).unwrap();
        assert_eq!(resumed.plan.version, 1);
        assert_eq!(resumed.pending_question.as_ref().unwrap().question_id, "q1");
        assert_eq!(
            resumed
                .pending_question
                .as_ref()
                .unwrap()
                .continuation
                .len(),
            1
        );
    }

    #[test]
    fn undo_rejects_preimages_outside_the_session_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let outside = directory.path().join("outside.txt");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, "do not touch\n").unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let mut session = SessionStore::new(paths)
            .create(root.clone(), "test".into(), "model".into())
            .unwrap();
        session
            .record_preimage(outside.clone(), Some("changed\n".into()))
            .unwrap();

        assert!(session.undo(&root).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "do not touch\n");
    }

    #[test]
    fn binary_preimages_round_trip_through_session_records() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let target = root.join("fixture.png");
        std::fs::create_dir_all(&root).unwrap();
        let original = vec![0, 159, 146, 150, 255];
        std::fs::write(&target, &original).unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let store = SessionStore::new(paths);
        let mut session = store
            .create(root.clone(), "test".into(), "model".into())
            .unwrap();
        session
            .record_binary_preimage(target.clone(), Some(original.clone()))
            .unwrap();
        std::fs::write(&target, b"replacement").unwrap();

        let resumed = store.open(&session.header.id).unwrap();
        resumed.undo(&root).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), original);
    }

    #[test]
    fn session_open_rejects_path_traversal_ids() {
        let directory = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(directory.path().join("home")).unwrap();
        let store = SessionStore::new(paths);
        assert!(store.open("../auth").is_err());
    }
}
