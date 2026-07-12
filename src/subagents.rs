use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify, Semaphore};
use uuid::Uuid;

use crate::events::{Event, EventSink};
use crate::paths::VeraPaths;
use crate::providers::{
    CodexRequestMetadata, ModelInfo, Provider, ProviderInput, ProviderRequest, ToolSchema,
};
use crate::safety::{
    ApprovalChoice, ApprovalHandler, PathGuard, PermissionKind, PermissionMode, PermissionPolicy,
};
use crate::sessions::{CapabilitySelection, LifecycleState, Session, SessionRecord, SessionStore};
use crate::tools::{ToolCall, ToolContext, ToolRegistry, ToolResult, execute};
use crate::worktrees::{WorktreeInfo, WorktreeManager, WorktreeReview};

const MAX_DEPTH: u8 = 1;
const MAX_CONCURRENCY: usize = 4;
const MAX_TURNS: usize = 8;
const MAX_SUMMARY_CHARS: usize = 12_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub agent_id: String,
    pub task: String,
    pub status: String,
    pub summary: String,
    pub writes: bool,
    pub session_id: String,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub worktree: Option<WorktreeInfo>,
    #[serde(default)]
    pub diff: Option<WorktreeReview>,
}

#[derive(Clone)]
pub struct SubagentExecutionRequest {
    pub agent_id: String,
    pub task: String,
    pub writes: bool,
    pub session_id: String,
    pub root: PathBuf,
    pub worktree: Option<WorktreeInfo>,
    pub depth: u8,
    pub cancellation: Arc<AtomicBool>,
    pub child_session_id: Arc<Mutex<Option<String>>>,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(&self, request: SubagentExecutionRequest) -> Result<String>;

    async fn cleanup(&self, _request: &SubagentExecutionRequest) -> Result<()> {
        Ok(())
    }

    async fn record_lifecycle(
        &self,
        _request: &SubagentExecutionRequest,
        _state: &str,
        _detail: &str,
    ) -> Result<()> {
        Ok(())
    }
}

struct AgentRecord {
    snapshot: SubagentSnapshot,
    cancellation: Arc<AtomicBool>,
    completion: Arc<Notify>,
    root: PathBuf,
    worktree: Option<WorktreeInfo>,
}

#[derive(Clone)]
pub struct SubagentManager {
    agents: Arc<Mutex<BTreeMap<String, AgentRecord>>>,
    runner: Arc<RwLock<Option<Arc<dyn SubagentRunner>>>>,
    permits: Arc<Semaphore>,
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self {
            agents: Arc::new(Mutex::new(BTreeMap::new())),
            runner: Arc::new(RwLock::new(None)),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENCY)),
        }
    }
}

impl SubagentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_runner(&self, runner: Arc<dyn SubagentRunner>) {
        if let Ok(mut current) = self.runner.write() {
            *current = Some(runner);
        }
    }

    pub fn is_configured(&self) -> bool {
        self.runner
            .read()
            .ok()
            .and_then(|runner| runner.as_ref().map(|_| ()))
            .is_some()
    }

    pub async fn spawn(
        &self,
        task: String,
        writes: bool,
        session_id: String,
        root: PathBuf,
        worktree: Option<WorktreeInfo>,
    ) -> Result<SubagentSnapshot> {
        if task.trim().is_empty() {
            anyhow::bail!("subagent task must not be empty");
        }
        let runner = self
            .runner
            .read()
            .map_err(|_| anyhow::anyhow!("subagent runner lock poisoned"))?
            .clone()
            .context("subagent provider runner is not configured")?;
        let task = bound_summary(&task);
        let agent_id = Uuid::new_v4().simple().to_string();
        let cancellation = Arc::new(AtomicBool::new(false));
        let completion = Arc::new(Notify::new());
        let snapshot = SubagentSnapshot {
            agent_id: agent_id.clone(),
            task: task.chars().take(4_000).collect(),
            status: "queued".into(),
            summary: "queued for an in-process provider turn".into(),
            writes,
            session_id: session_id.clone(),
            worktree_path: worktree
                .as_ref()
                .map(|info| info.path.display().to_string()),
            worktree: worktree.clone(),
            diff: None,
        };
        self.agents.lock().await.insert(
            agent_id.clone(),
            AgentRecord {
                snapshot: snapshot.clone(),
                cancellation: cancellation.clone(),
                completion: completion.clone(),
                root: root.clone(),
                worktree: worktree.clone(),
            },
        );

        let agents = self.agents.clone();
        let permits = self.permits.clone();
        let completion_for_task = completion.clone();
        let request = SubagentExecutionRequest {
            agent_id: agent_id.clone(),
            task: snapshot.task.clone(),
            writes,
            session_id,
            root,
            worktree,
            depth: 0,
            cancellation,
            child_session_id: Arc::new(Mutex::new(None)),
        };
        tokio::spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                completion_for_task.notify_one();
                return;
            };
            update_snapshot(&agents, &agent_id, |snapshot| {
                snapshot.status = "running".into();
                snapshot.summary = "running an in-process provider conversation".into();
            })
            .await;
            let cancellation = request.cancellation.clone();
            let result = tokio::select! {
                result = runner.run(request.clone()) => result,
                _ = wait_for_cancellation(cancellation.clone()) => {
                    Err(anyhow::anyhow!("subagent cancelled"))
                }
            };
            let _ = runner.cleanup(&request).await;
            let diff = if result.is_ok() {
                if let Some(worktree) = request.worktree.as_ref() {
                    match WorktreeManager::new(request.root.clone()) {
                        Ok(manager) => manager.review(worktree).await.ok(),
                        Err(_) => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let state = if cancellation.load(Ordering::Relaxed) {
                "cancelled"
            } else if result.is_ok() {
                "completed"
            } else {
                "failed"
            };
            let detail = result
                .as_ref()
                .map(|summary| format_summary(summary, diff.as_ref()))
                .unwrap_or_else(|error| crate::auth::redact(&error.to_string()));
            let _ = runner.record_lifecycle(&request, state, &detail).await;
            update_snapshot(&agents, &agent_id, |snapshot| {
                if cancellation.load(Ordering::Relaxed) {
                    snapshot.status = "cancelled".into();
                    snapshot.summary = "cancelled by the parent session".into();
                } else {
                    match result {
                        Ok(summary) => {
                            snapshot.status = "completed".into();
                            snapshot.summary =
                                bound_summary(&format_summary(&summary, diff.as_ref()));
                            snapshot.diff = diff.clone();
                        }
                        Err(error) => {
                            snapshot.status = "failed".into();
                            snapshot.summary =
                                bound_summary(&crate::auth::redact(&error.to_string()));
                        }
                    }
                }
            })
            .await;
            completion_for_task.notify_one();
        });
        Ok(snapshot)
    }

    pub async fn status(&self, agent_id: &str) -> Result<SubagentSnapshot> {
        self.agents
            .lock()
            .await
            .get(agent_id)
            .map(|record| record.snapshot.clone())
            .context("subagent not found")
    }

    pub async fn wait(&self, agent_id: &str) -> Result<SubagentSnapshot> {
        loop {
            let snapshot = self.status(agent_id).await?;
            if matches!(
                snapshot.status.as_str(),
                "completed" | "failed" | "cancelled"
            ) {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn cancel(&self, agent_id: &str) -> Result<SubagentSnapshot> {
        let completion = {
            let mut agents = self.agents.lock().await;
            let agent = agents.get_mut(agent_id).context("subagent not found")?;
            if matches!(
                agent.snapshot.status.as_str(),
                "completed" | "failed" | "cancelled" | "merged" | "discarded"
            ) {
                return Ok(agent.snapshot.clone());
            }
            agent.cancellation.store(true, Ordering::Relaxed);
            agent.snapshot.status = "cancellation_requested".into();
            agent.snapshot.summary = "cancellation requested by the parent session".into();
            agent.completion.clone()
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), completion.notified()).await;
        self.status(agent_id).await
    }

    pub async fn result(&self, agent_id: &str) -> Result<SubagentSnapshot> {
        self.status(agent_id).await
    }

    /// Cancel all active children for a closing parent session. Write
    /// worktrees are intentionally left in place for explicit review or
    /// discard after cancellation.
    pub async fn shutdown_session(&self, session_id: &str) -> Vec<SubagentSnapshot> {
        let active = {
            let mut agents = self.agents.lock().await;
            agents
                .values_mut()
                .filter(|agent| {
                    agent.snapshot.session_id == session_id
                        && !matches!(
                            agent.snapshot.status.as_str(),
                            "completed" | "failed" | "cancelled" | "merged" | "discarded"
                        )
                })
                .map(|agent| {
                    agent.cancellation.store(true, Ordering::Relaxed);
                    agent.snapshot.status = "cancellation_requested".into();
                    agent.snapshot.summary = "parent session is closing".into();
                    (agent.snapshot.agent_id.clone(), agent.completion.clone())
                })
                .collect::<Vec<_>>()
        };
        for (_, completion) in &active {
            let _ = tokio::time::timeout(Duration::from_secs(2), completion.notified()).await;
        }
        let agents = self.agents.lock().await;
        active
            .into_iter()
            .filter_map(|(agent_id, _)| agents.get(&agent_id).map(|agent| agent.snapshot.clone()))
            .collect()
    }

    pub async fn merge(&self, agent_id: &str, session: &mut Session) -> Result<SubagentSnapshot> {
        let (root, worktree) = {
            let agents = self.agents.lock().await;
            let agent = agents.get(agent_id).context("subagent not found")?;
            (
                agent.root.clone(),
                agent
                    .worktree
                    .clone()
                    .context("subagent has no write worktree")?,
            )
        };
        let manager = WorktreeManager::new(root)?;
        manager.apply_review(&worktree, session).await?;
        manager.discard(&worktree, session).await?;
        update_snapshot(&self.agents, agent_id, |snapshot| {
            snapshot.status = "merged".into();
            snapshot.summary = bound_summary(&format!(
                "guarded worktree diff applied and worktree discarded; {}",
                snapshot.summary
            ));
        })
        .await;
        self.status(agent_id).await
    }

    pub async fn discard(&self, agent_id: &str, session: &mut Session) -> Result<SubagentSnapshot> {
        let (root, worktree) = {
            let agents = self.agents.lock().await;
            let agent = agents.get(agent_id).context("subagent not found")?;
            (
                agent.root.clone(),
                agent
                    .worktree
                    .clone()
                    .context("subagent has no write worktree")?,
            )
        };
        WorktreeManager::new(root)?
            .discard(&worktree, session)
            .await?;
        update_snapshot(&self.agents, agent_id, |snapshot| {
            snapshot.status = "discarded".into();
            snapshot.summary = bound_summary(&format!("worktree discarded; {}", snapshot.summary));
        })
        .await;
        self.status(agent_id).await
    }

    pub async fn list(&self, session_id: &str) -> Vec<SubagentSnapshot> {
        self.agents
            .lock()
            .await
            .values()
            .filter(|agent| agent.snapshot.session_id == session_id)
            .map(|agent| agent.snapshot.clone())
            .collect()
    }
}

async fn update_snapshot(
    agents: &Mutex<BTreeMap<String, AgentRecord>>,
    agent_id: &str,
    update: impl FnOnce(&mut SubagentSnapshot),
) {
    if let Some(agent) = agents.lock().await.get_mut(agent_id) {
        update(&mut agent.snapshot);
    }
}

async fn wait_for_cancellation(cancellation: Arc<AtomicBool>) {
    while !cancellation.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn bound_summary(summary: &str) -> String {
    crate::auth::redact(summary)
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect()
}

fn format_summary(summary: &str, diff: Option<&WorktreeReview>) -> String {
    let Some(diff) = diff else {
        return summary.to_owned();
    };
    format!(
        "{summary}\nworktree {} base={} changed_paths={}\n{}",
        diff.worktree_id,
        diff.base_revision,
        diff.changed_paths.len(),
        diff.diff_stat.trim()
    )
}

/// Provider-backed runner used by the CLI. The provider and token remain in
/// this process; the child session is only a JSONL record and no child OS
/// process receives credentials.
#[derive(Clone)]
pub struct InProcessSubagentRunner {
    provider: Arc<dyn Provider>,
    paths: VeraPaths,
    model: String,
    context_window: usize,
    use_responses_lite: bool,
    registry: ToolRegistry,
    policy: PermissionPolicy,
    shell_timeout: u64,
}

impl InProcessSubagentRunner {
    pub fn new(
        provider: Arc<dyn Provider>,
        paths: VeraPaths,
        model: ModelInfo,
        registry: ToolRegistry,
        policy: PermissionPolicy,
        shell_timeout: u64,
    ) -> Self {
        Self {
            provider,
            paths,
            model: model.id,
            context_window: model.context_window,
            use_responses_lite: model.use_responses_lite,
            registry,
            policy,
            shell_timeout,
        }
    }
}

#[async_trait]
impl SubagentRunner for InProcessSubagentRunner {
    async fn run(&self, request: SubagentExecutionRequest) -> Result<String> {
        if request.depth >= MAX_DEPTH {
            // Depth zero is the parent-delegated child. A child may not spawn
            // another agent, and the tool schema is filtered below as well.
            if request.depth > 0 {
                anyhow::bail!("subagent depth limit exceeded");
            }
        }
        if request.cancellation.load(Ordering::Relaxed) {
            anyhow::bail!("subagent cancelled before start");
        }
        let child_root = request
            .worktree
            .as_ref()
            .map(|info| info.path.clone())
            .unwrap_or_else(|| request.root.clone());
        let guard = PathGuard::new(child_root.clone())?;
        let kind = self.provider.kind();
        let mut session = SessionStore::new(self.paths.clone()).create_with_selection(
            child_root.clone(),
            CapabilitySelection {
                provider: kind.as_str().into(),
                model: self.model.clone(),
                model_context_window: self.context_window,
                ..CapabilitySelection::default()
            },
        )?;
        *request.child_session_id.lock().await = Some(session.header.id.clone());
        session.add_message("user", &request.task)?;

        let mut policy = self.policy.clone();
        // Each child has its own permission view; neither ephemeral nor
        // session-scoped parent grants cross the delegation boundary.
        policy.clear_grants();
        if !request.writes {
            policy.set_mode(PermissionMode::Plan);
        }
        let mut approval = DelegatedApproval {
            allow_writes: request.writes,
        };
        let registry = self.registry.clone();
        let tools = subagent_tools(&registry, request.writes);
        let instructions = format!(
            "You are Vera's depth-one subagent. Work only on this delegated task: {}\nRepository root: {}\nReturn a bounded factual summary. Do not delegate to another agent. {}",
            request.task,
            child_root.display(),
            if request.writes {
                "Write only inside the assigned worktree and leave a concise summary of changed paths."
            } else {
                "Do not modify files or run mutating commands."
            }
        );
        let mut input = vec![ProviderInput::message("user", request.task.clone())];
        let codex_metadata = CodexRequestMetadata::for_session(&session.header.id);
        let mut answer = String::new();
        for _ in 0..MAX_TURNS {
            if request.cancellation.load(Ordering::Relaxed) {
                anyhow::bail!("subagent cancelled");
            }
            let mut sink = ChildEventSink::default();
            let response = self
                .provider
                .stream(
                    ProviderRequest {
                        model: self.model.clone(),
                        input: input.clone(),
                        tools: tools.clone(),
                        instructions: instructions.clone(),
                        effort: None,
                        use_responses_lite: self.use_responses_lite,
                        codex_metadata: Some(codex_metadata.clone()),
                    },
                    &mut sink,
                )
                .await?;
            answer.push_str(&response.text);
            if sink.calls.is_empty() {
                break;
            }
            for (id, call) in sink.calls {
                if request.cancellation.load(Ordering::Relaxed) {
                    anyhow::bail!("subagent cancelled");
                }
                let arguments =
                    serde_json::from_str(&call.arguments).unwrap_or_else(|_| serde_json::json!({}));
                let tool_name = if call.name.is_empty() {
                    "unknown"
                } else {
                    &call.name
                };
                let result = if tool_name.starts_with("subagent") {
                    ToolResult::complete("nested subagent delegation is not allowed", true)
                } else {
                    match execute(
                        &registry,
                        ToolCall {
                            name: tool_name.into(),
                            arguments: arguments.clone(),
                        },
                        ToolContext {
                            guard: &guard,
                            policy: &mut policy,
                            approval: &mut approval,
                            session: Some(&mut session),
                            shell_timeout: self.shell_timeout,
                        },
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => ToolResult::complete(
                            crate::auth::redact(&format!("tool {tool_name} failed: {error}")),
                            true,
                        ),
                    }
                };
                session.append(SessionRecord::ToolCall {
                    id: id.clone(),
                    name: tool_name.into(),
                    arguments,
                    result: Some(result.content()),
                })?;
                input.push(ProviderInput::FunctionCall {
                    id: id.clone(),
                    name: tool_name.into(),
                    arguments: call.arguments,
                });
                input.push(ProviderInput::FunctionCallOutput {
                    call_id: id,
                    output: result.content(),
                });
                if result.is_terminal() {
                    anyhow::bail!("subagent requested interactive input");
                }
            }
        }
        if answer.is_empty() {
            answer = "subagent completed without a textual summary".into();
        }
        session.add_message("assistant", bound_summary(&answer))?;
        Ok(bound_summary(&answer))
    }

    async fn cleanup(&self, request: &SubagentExecutionRequest) -> Result<()> {
        if let Some(session_id) = request.child_session_id.lock().await.clone() {
            self.registry.shutdown_processes(&session_id).await;
        }
        Ok(())
    }

    async fn record_lifecycle(
        &self,
        request: &SubagentExecutionRequest,
        state: &str,
        detail: &str,
    ) -> Result<()> {
        let Ok(mut parent) = SessionStore::new(self.paths.clone()).open(&request.session_id) else {
            return Ok(());
        };
        let child_session_id = request.child_session_id.lock().await.clone();
        let detail = child_session_id.map_or_else(
            || bound_summary(detail),
            |child_session_id| {
                serde_json::json!({
                    "child_session_id": child_session_id,
                    "detail": bound_summary(detail),
                })
                .to_string()
            },
        );
        parent.record_subagent_lifecycle(
            request.agent_id.clone(),
            LifecycleState {
                state: state.into(),
                detail: Some(detail),
            },
        )?;
        Ok(())
    }
}

fn subagent_tools(registry: &ToolRegistry, writes: bool) -> Vec<ToolSchema> {
    registry
        .schemas()
        .into_iter()
        .filter(|schema| !schema.name.starts_with("subagent"))
        .filter(|schema| {
            writes
                || registry
                    .read_only_schemas()
                    .iter()
                    .any(|item| item.name == schema.name)
        })
        .collect()
}

struct DelegatedApproval {
    allow_writes: bool,
}

#[async_trait]
impl ApprovalHandler for DelegatedApproval {
    async fn ask(&mut self, kind: PermissionKind, _description: &str) -> Result<ApprovalChoice> {
        if self.allow_writes && kind == PermissionKind::Write {
            Ok(ApprovalChoice::Once)
        } else {
            Ok(ApprovalChoice::Deny)
        }
    }
}

#[derive(Default)]
struct ChildEventSink {
    calls: BTreeMap<String, PendingCall>,
}

#[derive(Default)]
struct PendingCall {
    name: String,
    arguments: String,
}

#[async_trait]
impl EventSink for ChildEventSink {
    async fn emit(&mut self, event: Event) -> Result<()> {
        if let Event::ToolCallDelta {
            id,
            name,
            arguments,
        } = event
        {
            let call = self.calls.entry(id).or_default();
            if !name.is_empty() {
                call.name = name;
            }
            call.arguments.push_str(&arguments);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ModelCatalog, ProviderResult};
    use std::sync::atomic::AtomicUsize;

    struct FixtureRunner {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SubagentRunner for FixtureRunner {
        async fn run(&self, request: SubagentExecutionRequest) -> Result<String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            if request.cancellation.load(Ordering::Relaxed) {
                anyhow::bail!("cancelled")
            }
            Ok(format!("completed {}", request.task))
        }
    }

    #[tokio::test]
    async fn runs_tasks_and_enforces_four_agent_concurrency() {
        let manager = SubagentManager::new();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        manager.set_runner(Arc::new(FixtureRunner {
            active: active.clone(),
            max_active: max_active.clone(),
        }));
        let mut ids = Vec::new();
        for index in 0..6 {
            ids.push(
                manager
                    .spawn(
                        format!("task {index}"),
                        false,
                        "session".into(),
                        PathBuf::from("."),
                        None,
                    )
                    .await
                    .unwrap()
                    .agent_id,
            );
        }
        for id in ids {
            assert_eq!(manager.wait(&id).await.unwrap().status, "completed");
        }
        assert!(max_active.load(Ordering::SeqCst) <= MAX_CONCURRENCY);
    }

    #[tokio::test]
    async fn cancellation_is_visible_without_running_a_child_process() {
        let manager = SubagentManager::new();
        manager.set_runner(Arc::new(FixtureRunner {
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::new(AtomicUsize::new(0)),
        }));
        let snapshot = manager
            .spawn(
                "cancel me".into(),
                false,
                "session".into(),
                PathBuf::from("."),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            manager.cancel(&snapshot.agent_id).await.unwrap().status,
            "cancelled"
        );
        assert_eq!(
            manager.status(&snapshot.agent_id).await.unwrap().status,
            "cancelled"
        );
    }

    struct CancellationRunner;

    #[async_trait]
    impl SubagentRunner for CancellationRunner {
        async fn run(&self, request: SubagentExecutionRequest) -> Result<String> {
            while !request.cancellation.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            anyhow::bail!("cancelled")
        }
    }

    #[tokio::test]
    async fn shutdown_session_cancels_active_children_and_keeps_result_visible() {
        let manager = SubagentManager::new();
        manager.set_runner(Arc::new(CancellationRunner));
        let snapshot = manager
            .spawn(
                "long task".into(),
                false,
                "closing-session".into(),
                PathBuf::from("."),
                None,
            )
            .await
            .unwrap();
        let stopped = manager.shutdown_session("closing-session").await;
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].agent_id, snapshot.agent_id);
        assert_eq!(stopped[0].status, "cancelled");
    }

    struct MockProvider;

    fn mock_model() -> ModelInfo {
        ModelInfo {
            id: "mock-model".into(),
            display_name: "Mock model".into(),
            provider: "xai-oauth".into(),
            order: 0,
            context_window: 10_000,
            default_effort: None,
            supported_efforts: Vec::new(),
            use_responses_lite: false,
            source: "fixture".into(),
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn kind(&self) -> crate::providers::ProviderKind {
            crate::providers::ProviderKind::XaiOauth
        }

        async fn stream(
            &self,
            _request: ProviderRequest,
            sink: &mut dyn EventSink,
        ) -> Result<ProviderResult> {
            sink.emit(Event::TextDelta {
                text: "mock subagent summary".into(),
            })
            .await?;
            Ok(ProviderResult {
                text: "mock subagent summary".into(),
                input_tokens: 3,
                output_tokens: 4,
            })
        }

        async fn models(&self) -> Result<ModelCatalog> {
            Ok(ModelCatalog::default())
        }
    }

    #[tokio::test]
    async fn provider_runner_keeps_child_conversation_in_a_session_record() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let runner = InProcessSubagentRunner::new(
            Arc::new(MockProvider),
            paths.clone(),
            mock_model(),
            ToolRegistry::standard(),
            PermissionPolicy::default(),
            10,
        );
        let result = runner
            .run(SubagentExecutionRequest {
                agent_id: "agent".into(),
                task: "inspect fixture".into(),
                writes: false,
                session_id: "parent".into(),
                root,
                worktree: None,
                depth: 0,
                cancellation: Arc::new(AtomicBool::new(false)),
                child_session_id: Arc::new(Mutex::new(None)),
            })
            .await
            .unwrap();
        assert!(result.contains("mock subagent summary"));
        assert_eq!(SessionStore::new(paths).list().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn child_cleanup_uses_the_child_session_id() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let processes = Arc::new(crate::processes::ProcessManager::for_fixture());
        let registry = ToolRegistry::standard_with_skills_and_processes(None, processes.clone());
        let runner = InProcessSubagentRunner::new(
            Arc::new(MockProvider),
            paths,
            mock_model(),
            registry,
            PermissionPolicy::default(),
            10,
        );
        let process = processes
            .start(crate::processes::ProcessStartRequest {
                command: "sleep 10".into(),
                cwd: root.clone(),
                environment: BTreeMap::new(),
                network: false,
                columns: 80,
                rows: 24,
                session_id: "child-session".into(),
            })
            .await
            .unwrap();
        let request = SubagentExecutionRequest {
            agent_id: "agent".into(),
            task: "cleanup".into(),
            writes: false,
            session_id: "parent-session".into(),
            root,
            worktree: None,
            depth: 0,
            cancellation: Arc::new(AtomicBool::new(false)),
            child_session_id: Arc::new(Mutex::new(Some("child-session".into()))),
        };
        runner.cleanup(&request).await.unwrap();
        for _ in 0..20 {
            if processes
                .list(Some("child-session"))
                .await
                .iter()
                .any(|value| value.process_id == process.process_id && value.status != "running")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("child process was not shut down");
    }
}
