use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Duration;
use url::Url;

use crate::browser::BrowserManager;
use crate::extensions::{McpClient, McpToolDescription, SkillCatalog, mcp_tool_is_read_only};
use crate::processes::{ProcessManager, ProcessStartRequest, canonical_process_cwd};
use crate::providers::ToolSchema;
use crate::safety::{
    ActionSignature, ApprovalHandler, PathGuard, PermissionKind, PermissionPolicy, Sandbox,
    normalize_command_prefix,
};
use crate::sessions::{PlanStep, PlanStepState, Session, SessionPlan};
use crate::subagents::{SubagentManager, SubagentRunner};
use crate::worktrees::WorktreeManager;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug)]
pub enum ToolResult {
    Complete {
        content: String,
        is_error: bool,
    },
    NeedsInput {
        question_id: String,
        prompt: String,
        choices: Vec<String>,
    },
    PlanUpdated {
        plan: SessionPlan,
    },
    BackgroundStarted {
        process_id: String,
    },
}

impl ToolResult {
    pub fn complete(content: impl Into<String>, is_error: bool) -> Self {
        Self::Complete {
            content: content.into(),
            is_error,
        }
    }

    pub fn content(&self) -> String {
        match self {
            Self::Complete { content, .. } => content.clone(),
            Self::NeedsInput {
                question_id,
                prompt,
                choices,
            } => serde_json::json!({
                "type": "needs_input",
                "question_id": question_id,
                "prompt": prompt,
                "choices": choices,
            })
            .to_string(),
            Self::PlanUpdated { plan } => {
                serde_json::to_string(plan).unwrap_or_else(|_| "plan updated".into())
            }
            Self::BackgroundStarted { process_id } => {
                format!("background process started: {process_id}")
            }
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Complete { is_error: true, .. })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::NeedsInput { .. })
    }
}

pub struct ToolContext<'a> {
    pub guard: &'a PathGuard,
    pub policy: &'a mut PermissionPolicy,
    pub approval: &'a mut dyn ApprovalHandler,
    pub session: Option<&'a mut Session>,
    pub shell_timeout: u64,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    fn read_only(&self) -> bool {
        false
    }
    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
    processes: Arc<ProcessManager>,
    browser: Arc<BrowserManager>,
    subagents: Arc<SubagentManager>,
}

impl ToolRegistry {
    pub fn standard() -> Self {
        Self::standard_with_skills(None)
    }

    pub fn standard_with_skills(skills: Option<Arc<Mutex<SkillCatalog>>>) -> Self {
        Self::standard_with_skills_and_processes(skills, Arc::new(ProcessManager::new()))
    }

    pub fn standard_with_skills_and_processes(
        skills: Option<Arc<Mutex<SkillCatalog>>>,
        processes: Arc<ProcessManager>,
    ) -> Self {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(ListFiles),
            Arc::new(ReadFile),
            Arc::new(Search),
            Arc::new(WriteFile),
            Arc::new(ApplyPatch),
            Arc::new(GitStatus),
            Arc::new(Shell),
            Arc::new(Question),
            Arc::new(PlanManagement),
            Arc::new(WebSearch),
            Arc::new(XSearch),
            Arc::new(SessionTask),
            Arc::new(ProcessStart {
                processes: processes.clone(),
            }),
            Arc::new(ProcessList {
                processes: processes.clone(),
            }),
            Arc::new(ProcessPoll {
                processes: processes.clone(),
            }),
            Arc::new(ProcessLog {
                processes: processes.clone(),
            }),
            Arc::new(ProcessWrite {
                processes: processes.clone(),
            }),
            Arc::new(ProcessWait {
                processes: processes.clone(),
            }),
            Arc::new(ProcessKill {
                processes: processes.clone(),
            }),
        ];
        let subagents = Arc::new(SubagentManager::new());
        tools.extend([
            Arc::new(SubagentSpawn {
                manager: subagents.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(SubagentStatus {
                manager: subagents.clone(),
            }),
            Arc::new(SubagentWait {
                manager: subagents.clone(),
            }),
            Arc::new(SubagentCancel {
                manager: subagents.clone(),
            }),
            Arc::new(SubagentDiscard {
                manager: subagents.clone(),
            }),
            Arc::new(SubagentResult {
                manager: subagents.clone(),
            }),
        ]);
        let browser = Arc::new(BrowserManager::new());
        tools.extend([
            Arc::new(BrowserConnect {
                browser: browser.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(BrowserStatus {
                browser: browser.clone(),
            }),
            Arc::new(BrowserNavigate {
                browser: browser.clone(),
            }),
            Arc::new(BrowserSnapshot {
                browser: browser.clone(),
            }),
            Arc::new(BrowserScreenshot {
                browser: browser.clone(),
            }),
            Arc::new(ImageInspect),
        ]);
        if let Some(skills) = skills {
            tools.push(Arc::new(LoadSkill { skills }));
        }
        Self {
            tools,
            processes,
            browser,
            subagents,
        }
    }

    pub fn processes(&self) -> Arc<ProcessManager> {
        self.processes.clone()
    }

    pub async fn shutdown_processes(&self, session_id: &str) {
        self.processes.shutdown_session(session_id).await;
    }

    pub fn subagents(&self) -> Arc<SubagentManager> {
        self.subagents.clone()
    }

    pub fn set_subagent_runner(&mut self, runner: Arc<dyn SubagentRunner>) {
        self.subagents.set_runner(runner);
    }

    pub async fn set_browser_endpoints(&self, endpoints: Vec<String>) {
        self.browser.set_approved_endpoints(endpoints).await;
    }
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|tool| tool.schema()).collect()
    }

    pub fn read_only_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|tool| tool.schema())
            .filter(|schema| {
                matches!(
                    schema.name.as_str(),
                    "list_files"
                        | "read_file"
                        | "search"
                        | "git_status_diff"
                        | "question"
                        | "plan"
                        | "web_search"
                        | "x_search"
                        | "load_skill"
                        | "process_list"
                        | "process_poll"
                        | "process_log"
                        | "process_wait"
                ) || self.find(&schema.name).is_some_and(|tool| tool.read_only())
            })
            .collect()
    }
    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .cloned()
    }
    pub fn add_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.schema().name;
        self.tools.retain(|entry| entry.schema().name != name);
        self.tools.push(tool);
    }
    pub fn remove_tools_starting_with(&mut self, prefix: &str) {
        self.tools
            .retain(|tool| !tool.schema().name.starts_with(prefix));
    }
}

struct ListFiles;
struct ReadFile;
struct Search;
struct WriteFile;
struct ApplyPatch;
struct GitStatus;
struct Shell;
struct Question;
struct PlanManagement;
struct WebSearch;
struct XSearch;
struct SessionTask;
struct SubagentSpawn {
    manager: Arc<SubagentManager>,
}
struct SubagentStatus {
    manager: Arc<SubagentManager>,
}
struct SubagentWait {
    manager: Arc<SubagentManager>,
}
struct SubagentCancel {
    manager: Arc<SubagentManager>,
}
struct SubagentResult {
    manager: Arc<SubagentManager>,
}
struct ProcessStart {
    processes: Arc<ProcessManager>,
}
struct ProcessList {
    processes: Arc<ProcessManager>,
}
struct ProcessPoll {
    processes: Arc<ProcessManager>,
}
struct ProcessLog {
    processes: Arc<ProcessManager>,
}
struct ProcessWrite {
    processes: Arc<ProcessManager>,
}
struct ProcessWait {
    processes: Arc<ProcessManager>,
}
struct ProcessKill {
    processes: Arc<ProcessManager>,
}
struct BrowserConnect {
    browser: Arc<BrowserManager>,
}
struct BrowserStatus {
    browser: Arc<BrowserManager>,
}
struct BrowserNavigate {
    browser: Arc<BrowserManager>,
}
struct BrowserSnapshot {
    browser: Arc<BrowserManager>,
}
struct BrowserScreenshot {
    browser: Arc<BrowserManager>,
}
struct ImageInspect;

fn browser_status_result(status: crate::browser::BrowserStatus) -> ToolResult {
    ToolResult::complete(serde_json::to_string(&status).unwrap_or_default(), false)
}

#[cfg(any())]
mod disabled_first_browser_tools {
    #[async_trait]
    impl Tool for BrowserConnect {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "browser_connect".into(),
                description: "Connect to an explicitly configured CDP browser endpoint.".into(),
                parameters: json!({"type":"object","properties":{"endpoint":{"type":"string"}},"required":["endpoint"]}),
            }
        }
        async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
            context
                .policy
                .authorize(
                    PermissionKind::Network,
                    "connect to the configured browser",
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
            let endpoint = arguments
                .get("endpoint")
                .and_then(Value::as_str)
                .context("missing browser endpoint")?;
            Ok(browser_status_result(self.browser.connect(endpoint).await?))
        }
    }

    #[cfg(any())]
    #[async_trait]
    impl Tool for BrowserStatus {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "browser_status".into(),
                description: "Return the connected browser target status.".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
            Ok(browser_status_result(self.browser.status().await?))
        }
    }

    #[cfg(any())]
    #[async_trait]
    impl Tool for BrowserNavigate {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "browser_navigate".into(),
                description: "Navigate the connected browser to an HTTP(S) URL.".into(),
                parameters: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
            }
        }
        async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
            context
                .policy
                .authorize(
                    PermissionKind::Network,
                    "navigate the connected browser",
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
            let url = arguments
                .get("url")
                .and_then(Value::as_str)
                .context("missing browser URL")?;
            Ok(browser_status_result(self.browser.navigate(url).await?))
        }
    }

    #[cfg(any())]
    #[async_trait]
    impl Tool for BrowserSnapshot {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "browser_snapshot".into(),
                description: "Read a bounded HTML snapshot from the connected browser.".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
            Ok(ToolResult::complete(self.browser.snapshot().await?, false))
        }
    }

    #[cfg(any())]
    #[async_trait]
    impl Tool for BrowserScreenshot {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "browser_screenshot".into(),
                description: "Capture a bounded screenshot from the connected browser.".into(),
                parameters: json!({"type":"object","properties":{}}),
            }
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
            Ok(ToolResult::complete(
                STANDARD.encode(self.browser.screenshot().await?),
                false,
            ))
        }
    }

    #[cfg(any())]
    #[async_trait]
    impl Tool for ImageInspect {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "image_inspect".into(),
                description: "Inspect a repository image within bounded size and pixel limits."
                    .into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            }
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
            let path = context.guard.resolve(Path::new(
                arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .context("missing image path")?,
            ))?;
            Ok(ToolResult::complete(
                serde_json::to_string(&inspect_image(&path, context.guard.root())?)?,
                false,
            ))
        }
    }
}

struct LoadSkill {
    skills: Arc<Mutex<SkillCatalog>>,
}

#[async_trait]
impl Tool for LoadSkill {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "load_skill".into(),
            description: "Load the complete body of an available skill for subsequent turns."
                .into(),
            parameters: json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let name = arguments
            .get("name")
            .and_then(Value::as_str)
            .context("missing skill name")?;
        authorize_read(&mut context, "load_skill", "load a skill body", None).await?;
        let mut skills = self.skills.lock().await;
        let body = skills.load_body(name)?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_skill_state(name, true)?;
        }
        Ok(ToolResult::complete(body, false))
    }
}

fn process_id(arguments: &Value) -> Result<&str> {
    arguments
        .get("process_id")
        .and_then(Value::as_str)
        .context("missing process_id")
}

async fn authorize_read(
    context: &mut ToolContext<'_>,
    tool_name: &str,
    description: &str,
    canonical_path: Option<PathBuf>,
) -> Result<()> {
    context
        .policy
        .authorize_action(
            ActionSignature {
                permission_kind: PermissionKind::Read,
                tool_name: Some(tool_name.into()),
                canonical_path,
                ..ActionSignature::default()
            },
            description,
            context.approval,
            context.session.as_deref_mut(),
        )
        .await
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessStart {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_start".into(),
            description: "Start a bounded background process in the repository.".into(),
            parameters: json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"environment":{"type":"object"},"network":{"type":"boolean"},"columns":{"type":"integer"},"rows":{"type":"integer"}},"required":["command"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize(
                PermissionKind::Shell,
                "start a background process",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .context("missing process command")?;
        let cwd = canonical_process_cwd(
            context.guard.root(),
            arguments.get("cwd").and_then(Value::as_str).map(Path::new),
        )?;
        let environment = arguments
            .get("environment")
            .and_then(Value::as_object)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect::<std::collections::BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let session_id = context
            .session
            .as_ref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "headless".into());
        let snapshot = self
            .processes
            .start(ProcessStartRequest {
                command: command.into(),
                cwd,
                environment,
                network: arguments
                    .get("network")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                columns: arguments
                    .get("columns")
                    .and_then(Value::as_u64)
                    .unwrap_or(120) as u16,
                rows: arguments.get("rows").and_then(Value::as_u64).unwrap_or(40) as u16,
                session_id,
            })
            .await?;
        Ok(ToolResult::BackgroundStarted {
            process_id: snapshot.process_id,
        })
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessList {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_list".into(),
            description: "List background processes owned by the current session.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "process_list",
            "list background processes",
            None,
        )
        .await?;
        let session_id = context
            .session
            .as_ref()
            .map(|session| session.header.id.as_str());
        let snapshots = self.processes.list(session_id).await;
        Ok(ToolResult::complete(
            serde_json::to_string(&snapshots)?,
            false,
        ))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessPoll {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_poll".into(),
            description: "Read new output from a background process.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"cursor":{"type":"integer"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let output = self
            .processes
            .poll(
                process_id(&arguments)?,
                arguments.get("cursor").and_then(Value::as_u64).unwrap_or(0),
            )
            .await?;
        Ok(ToolResult::complete(serde_json::to_string(&output)?, false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessLog {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_log".into(),
            description: "Read a bounded page of background process output.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let output = self
            .processes
            .log(
                process_id(&arguments)?,
                arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
                arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(64 * 1024) as usize,
            )
            .await?;
        Ok(ToolResult::complete(serde_json::to_string(&output)?, false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessWrite {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_write".into(),
            description: "Write bounded input to a background process.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"input":{"type":"string"}},"required":["process_id","input"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize(
                PermissionKind::Shell,
                "write to a background process",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let input = arguments
            .get("input")
            .and_then(Value::as_str)
            .context("missing process input")?;
        self.processes
            .write(process_id(&arguments)?, input.as_bytes())
            .await?;
        Ok(ToolResult::complete("input written", false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessWait {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_wait".into(),
            description: "Wait for a background process to exit.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let code = self.processes.wait(process_id(&arguments)?).await?;
        Ok(ToolResult::complete(
            format!("process exited with status {code}"),
            false,
        ))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ProcessKill {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_kill".into(),
            description: "Terminate a background process.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"}},"required":["process_id"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize(
                PermissionKind::Shell,
                "terminate a background process",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        self.processes.kill(process_id(&arguments)?).await?;
        Ok(ToolResult::complete("process terminated", false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for BrowserConnect {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_connect".into(),
            description: "Connect to an explicit local Chrome DevTools endpoint.".into(),
            parameters: json!({"type":"object","properties":{"endpoint":{"type":"string"}},"required":["endpoint"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize(
                PermissionKind::Browser,
                "connect to a browser debugging endpoint",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let endpoint = arguments
            .get("endpoint")
            .and_then(Value::as_str)
            .context("missing browser endpoint")?;
        let status = self.browser.connect(endpoint).await?;
        Ok(ToolResult::complete(serde_json::to_string(&status)?, false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for BrowserStatus {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_status".into(),
            description: "Show the connected browser target.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        Ok(ToolResult::complete(
            serde_json::to_string(&self.browser.status().await?)?,
            false,
        ))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for BrowserNavigate {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_navigate".into(),
            description: "Navigate the connected browser to an HTTP(S) URL.".into(),
            parameters: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize(
                PermissionKind::Browser,
                "navigate a browser",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .context("missing browser URL")?;
        let requested_host = Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        let status = self.browser.navigate(url).await?;
        let final_host = Url::parse(&status.target_url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        if final_host != requested_host {
            context
                .policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Browser,
                        tool_name: Some("browser_navigate".into()),
                        browser_action: Some("navigate".into()),
                        network_host: final_host,
                        ..ActionSignature::default()
                    },
                    &format!("allow browser redirect to {}", status.target_url),
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
        }
        Ok(ToolResult::complete(serde_json::to_string(&status)?, false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for BrowserSnapshot {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_snapshot".into(),
            description: "Read a bounded HTML snapshot from the connected browser.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        Ok(ToolResult::complete(self.browser.snapshot().await?, false))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for BrowserScreenshot {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_screenshot".into(),
            description: "Capture a bounded PNG screenshot from the connected browser.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        let bytes = self.browser.screenshot().await?;
        Ok(ToolResult::complete(
            json!({"mime_type":"image/png","bytes":bytes.len(),"data":base64::engine::general_purpose::STANDARD.encode(bytes)}).to_string(),
            false,
        ))
    }
}

#[cfg(any())]
#[async_trait]
impl Tool for ImageInspect {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "image_inspect".into(),
            description: "Inspect dimensions and type of a repository image.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing image path")?,
        ))?;
        Ok(ToolResult::complete(
            inspect_image(&path, context.guard.root())?.to_string(),
            false,
        ))
    }
}

pub struct McpToolAdapter {
    client: Arc<McpClient>,
    server: String,
    description: McpToolDescription,
}

impl McpToolAdapter {
    pub fn new(client: Arc<McpClient>, description: McpToolDescription) -> Self {
        Self {
            server: client.name().into(),
            client,
            description,
        }
    }
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: format!("mcp__{}__{}", self.server, self.description.name),
            description: if self.description.description.is_empty() {
                format!("MCP read-only tool {}", self.description.name)
            } else {
                self.description.description.clone()
            },
            parameters: self.description.input_schema.clone(),
        }
    }

    fn read_only(&self) -> bool {
        mcp_tool_is_read_only(&self.description)
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let mut action = ActionSignature {
            permission_kind: PermissionKind::Mcp,
            tool_name: Some(self.schema().name.clone()),
            ..ActionSignature::default()
        };
        action.mcp_server = Some(self.server.clone());
        action.mcp_tool = Some(self.description.name.clone());
        context
            .policy
            .authorize_action(
                action,
                &format!("call MCP tool {}", self.schema().name),
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let value = match self.client.call(&self.description.name, arguments).await {
            Ok(value) => value,
            Err(error) => {
                return Ok(ToolResult::complete(
                    crate::auth::redact(&format!("MCP tool call failed: {error}")),
                    true,
                ));
            }
        };
        let content = normalize_mcp_result(&value);
        Ok(ToolResult::complete(
            content,
            value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ))
    }
}

fn normalize_mcp_result(value: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = value.get("content").and_then(Value::as_array) {
        for item in items.iter().take(64) {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.chars().take(16_000).collect::<String>());
                    }
                }
                Some("image") => {
                    let mime = item
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let bytes = item.get("data").and_then(Value::as_str).map_or(0, str::len);
                    parts.push(format!("[MCP image: mime={mime}, encoded_bytes={bytes}]"));
                }
                Some("resource") | Some("resource_link") => {
                    parts.push(
                        serde_json::to_string(item).unwrap_or_else(|_| "[MCP resource]".into()),
                    );
                }
                _ => parts
                    .push(serde_json::to_string(item).unwrap_or_else(|_| "[MCP content]".into())),
            }
        }
    }
    if let Some(structured) = value.get("structuredContent") {
        parts.push(format!("structured: {}", structured));
    }
    let content = if parts.is_empty() {
        value.to_string()
    } else {
        parts.join("\n")
    };
    content.chars().take(64_000).collect()
}

#[async_trait]
impl Tool for ListFiles {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_files".into(),
            description: "List repository files, respecting ignore rules.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":[]}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments.get("path").and_then(Value::as_str).unwrap_or("."),
        ))?;
        authorize_read(
            &mut context,
            "list_files",
            "list repository files",
            Some(path.clone()),
        )
        .await?;
        let mut files = Vec::new();
        for entry in ignore::WalkBuilder::new(path)
            .hidden(false)
            .git_ignore(true)
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                files.push(
                    entry
                        .path()
                        .strip_prefix(context.guard.root())
                        .unwrap_or(entry.path())
                        .display()
                        .to_string(),
                );
            }
            if files.len() >= 2_000 {
                break;
            }
        }
        Ok(ToolResult::complete(files.join("\n"), false))
    }
}

#[async_trait]
impl Tool for ReadFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "Read a UTF-8 repository file.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
        authorize_read(
            &mut context,
            "read_file",
            "read a repository file",
            Some(path.clone()),
        )
        .await?;
        let content = fs::read_to_string(path).context("read file")?;
        Ok(ToolResult::complete(content, false))
    }
}

#[async_trait]
impl Tool for Search {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search".into(),
            description:
                "Search repository text using installed rg, git grep, or an ignore-aware walker."
                    .into(),
            parameters: json!({"type":"object","properties":{"query":{"type":"string"},"path":{"type":"string"}},"required":["query"]}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .context("missing query")?;
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = context.guard.resolve(Path::new(path))?;
        authorize_read(
            &mut context,
            "search",
            "search repository text",
            Some(root.clone()),
        )
        .await?;
        let (program, args) = if which::which("rg").is_ok() {
            (
                "rg",
                vec![
                    "--line-number".into(),
                    "--hidden".into(),
                    query.into(),
                    root.display().to_string(),
                ],
            )
        } else {
            (
                "git",
                vec![
                    "grep".into(),
                    "-n".into(),
                    query.into(),
                    "--".into(),
                    root.display().to_string(),
                ],
            )
        };
        let output = Sandbox::run(
            program,
            &args,
            context.guard.root(),
            false,
            Duration::from_secs(30),
        )
        .await?;
        Ok(ToolResult::complete(
            format!("{}{}", output.stdout, output.stderr),
            output.status != 0,
        ))
    }
}

#[async_trait]
impl Tool for WriteFile {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "Atomically write a repository file after approval.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Write,
                    tool_name: Some("write_file".into()),
                    canonical_path: Some(path.clone()),
                    ..ActionSignature::default()
                },
                "write a repository file",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .context("missing content")?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_binary_preimage(path.clone(), fs::read(&path).ok())?;
        }
        atomic_write(&path, content)?;
        Ok(ToolResult::complete(
            format!("wrote {}", path.display()),
            false,
        ))
    }
}

#[async_trait]
impl Tool for ApplyPatch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "apply_patch".into(),
            description: "Atomically replace one exact repository text span after approval.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["path","old","new"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Write,
                    tool_name: Some("apply_patch".into()),
                    canonical_path: Some(path.clone()),
                    ..ActionSignature::default()
                },
                "apply a repository patch",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let old = arguments
            .get("old")
            .and_then(Value::as_str)
            .context("missing old text")?;
        let new = arguments
            .get("new")
            .and_then(Value::as_str)
            .context("missing new text")?;
        let original = fs::read_to_string(&path).context("read patch target")?;
        if original.matches(old).count() != 1 {
            anyhow::bail!("patch text must match exactly once");
        }
        if let Some(session) = context.session.as_deref_mut() {
            session.record_preimage(path.clone(), Some(original.clone()))?;
        }
        atomic_write(&path, &original.replacen(old, new, 1))?;
        Ok(ToolResult::complete(
            format!("patched {}", path.display()),
            false,
        ))
    }
}

#[async_trait]
impl Tool for GitStatus {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "git_status_diff".into(),
            description: "Show git status and diff for the repository.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "git_status_diff",
            "read repository status and diff",
            None,
        )
        .await?;
        let status = Sandbox::run(
            "git",
            ["status".into(), "--short".into()].as_slice(),
            context.guard.root(),
            false,
            Duration::from_secs(30),
        )
        .await?;
        let diff = Sandbox::run(
            "git",
            ["diff".into(), "--".into()].as_slice(),
            context.guard.root(),
            false,
            Duration::from_secs(30),
        )
        .await?;
        Ok(ToolResult::complete(
            format!("{}\n{}", status.stdout, diff.stdout),
            status.status != 0 || diff.status != 0,
        ))
    }
}

#[async_trait]
impl Tool for Shell {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "shell".into(),
            description: "Run a command in the macOS Seatbelt sandbox.".into(),
            parameters: json!({"type":"object","properties":{"command":{"type":"string"},"network":{"type":"boolean"}},"required":["command"]}),
        }
    }
    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .context("missing command")?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Shell,
                    tool_name: Some("shell".into()),
                    command_prefix: Some(normalize_command_prefix(command)),
                    ..ActionSignature::default()
                },
                "run a shell command",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let network = arguments
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if network {
            context
                .policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Network,
                        tool_name: Some("shell".into()),
                        command_prefix: Some(normalize_command_prefix(command)),
                        ..ActionSignature::default()
                    },
                    "run a network-enabled shell command",
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
        }
        let output = Sandbox::run(
            "/bin/zsh",
            &["-lc".into(), command.into()],
            context.guard.root(),
            network,
            Duration::from_secs(context.shell_timeout),
        )
        .await?;
        Ok(ToolResult::complete(
            format!("{}{}", output.stdout, output.stderr),
            output.status != 0,
        ))
    }
}

#[async_trait]
impl Tool for Question {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "question".into(),
            description: "Ask the user for a missing decision before continuing.".into(),
            parameters: json!({"type":"object","properties":{"question":{"type":"string"},"choices":{"type":"array","items":{"type":"string"},"maxItems":4},"question_id":{"type":"string"}},"required":["question"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let question = arguments
            .get("question")
            .and_then(Value::as_str)
            .context("missing question")?;
        let question_id = arguments
            .get("question_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        let choices = arguments
            .get("choices")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .take(4)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(session) = context.session.as_deref_mut() {
            session.record_pending_question(
                question_id.clone(),
                question,
                choices.clone(),
                Vec::new(),
            )?;
        }
        Ok(ToolResult::NeedsInput {
            question_id,
            prompt: question.to_owned(),
            choices,
        })
    }
}

#[async_trait]
impl Tool for PlanManagement {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "plan".into(),
            description: "Record or revise a concise plan before making changes.".into(),
            parameters: json!({"type":"object","properties":{"steps":{"type":"array","items":{"oneOf":[{"type":"string"},{"type":"object","properties":{"id":{"type":"string"},"text":{"type":"string"},"state":{"enum":["pending","in_progress","completed"]}},"required":["text"]}]}}},"required":["steps"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let steps = arguments
            .get("steps")
            .and_then(Value::as_array)
            .context("missing plan steps")?;
        let parsed = steps
            .iter()
            .enumerate()
            .map(|(index, value)| {
                if let Some(text) = value.as_str() {
                    return Ok(PlanStep {
                        id: (index + 1).to_string(),
                        text: text.to_owned(),
                        state: PlanStepState::Pending,
                    });
                }
                let object = value
                    .as_object()
                    .context("plan steps must be strings or objects")?;
                let state = match object.get("state").and_then(Value::as_str) {
                    Some("in_progress") => PlanStepState::InProgress,
                    Some("completed") => PlanStepState::Completed,
                    Some("pending") | None => PlanStepState::Pending,
                    Some(other) => anyhow::bail!("unknown plan step state {other}"),
                };
                Ok(PlanStep {
                    id: object
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    text: object
                        .get("text")
                        .and_then(Value::as_str)
                        .context("plan step object is missing text")?
                        .to_owned(),
                    state,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = if let Some(session) = context.session.as_deref_mut() {
            session.record_plan(SessionPlan {
                version: 0,
                steps: parsed,
            })?
        } else {
            SessionPlan {
                version: 1,
                steps: parsed,
            }
        };
        Ok(ToolResult::PlanUpdated { plan })
    }
}

async fn provider_search(
    name: &str,
    mut context: ToolContext<'_>,
    arguments: Value,
) -> Result<ToolResult> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .context("missing query")?;
    context
        .policy
        .authorize_action(
            ActionSignature {
                permission_kind: PermissionKind::Network,
                tool_name: Some(name.into()),
                ..ActionSignature::default()
            },
            &format!("use provider-native {name} for {query}"),
            context.approval,
            context.session.as_deref_mut(),
        )
        .await?;
    Ok(ToolResult::complete(
        format!("provider-native {name} search requested for {query}"),
        false,
    ))
}

#[async_trait]
impl Tool for WebSearch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_search".into(),
            description: "Search the web through the active provider and return citations.".into(),
            parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        }
    }

    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        provider_search("web_search", context, arguments).await
    }
}

#[async_trait]
impl Tool for XSearch {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "x_search".into(),
            description: "Search public X posts through the xAI provider.".into(),
            parameters: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}),
        }
    }

    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        provider_search("x_search", context, arguments).await
    }
}

#[async_trait]
impl Tool for SessionTask {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "session_task".into(),
            description: "Create a bounded follow-up task in the current session.".into(),
            parameters: json!({"type":"object","properties":{"task":{"type":"string"}},"required":["task"]}),
        }
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let task = arguments
            .get("task")
            .and_then(Value::as_str)
            .context("missing task")?;
        Ok(ToolResult::complete(
            format!("queued session task: {task}"),
            false,
        ))
    }
}

#[async_trait]
impl Tool for ProcessStart {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_start".into(),
            description: "Start a bounded background development process.".into(),
            parameters: json!({"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string"},"environment":{"type":"object","additionalProperties":{"type":"string"}},"network":{"type":"boolean"},"pty_size":{"type":"object","properties":{"columns":{"type":"integer"},"rows":{"type":"integer"}}}},"required":["command"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .context("missing command")?;
        let cwd = canonical_process_cwd(
            context.guard.root(),
            arguments.get("cwd").and_then(Value::as_str).map(Path::new),
        )?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Shell,
                    tool_name: Some("process_start".into()),
                    command_prefix: Some(normalize_command_prefix(command)),
                    canonical_path: Some(cwd.clone()),
                    ..ActionSignature::default()
                },
                &format!("start background process {command}"),
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let network = arguments
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if network {
            context
                .policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Network,
                        tool_name: Some("process_start".into()),
                        command_prefix: Some(normalize_command_prefix(command)),
                        canonical_path: Some(cwd.clone()),
                        ..ActionSignature::default()
                    },
                    &format!("allow network for background process {command}"),
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
        }
        let environment = match arguments.get("environment") {
            None => std::collections::BTreeMap::new(),
            Some(Value::Object(values)) => {
                let mut environment = std::collections::BTreeMap::new();
                for (key, value) in values {
                    environment.insert(
                        key.clone(),
                        value
                            .as_str()
                            .context("process environment values must be strings")?
                            .to_owned(),
                    );
                }
                environment
            }
            Some(_) => anyhow::bail!("process environment must be an object"),
        };
        let requested_columns = arguments
            .pointer("/pty_size/columns")
            .and_then(Value::as_u64)
            .unwrap_or(120);
        let requested_rows = arguments
            .pointer("/pty_size/rows")
            .and_then(Value::as_u64)
            .unwrap_or(40);
        if requested_columns == 0
            || requested_rows == 0
            || requested_columns > u16::MAX as u64
            || requested_rows > u16::MAX as u64
        {
            anyhow::bail!("process PTY dimensions must fit in a non-zero u16");
        }
        let columns = requested_columns as u16;
        let rows = requested_rows as u16;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let snapshot = self
            .processes
            .start(ProcessStartRequest {
                command: command.into(),
                cwd,
                environment,
                network,
                columns,
                rows,
                session_id,
            })
            .await?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_process_lifecycle(
                snapshot.process_id.clone(),
                crate::sessions::LifecycleState {
                    state: "running".into(),
                    detail: Some(snapshot.command.clone()),
                },
            )?;
        }
        Ok(ToolResult::BackgroundStarted {
            process_id: snapshot.process_id,
        })
    }
}

#[async_trait]
impl Tool for ProcessList {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_list".into(),
            description: "List background processes attached to this session.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "process_list",
            "list background processes",
            None,
        )
        .await?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let values = self.processes.list(Some(&session_id)).await;
        Ok(ToolResult::complete(
            serde_json::to_string(
                &values
                    .iter()
                    .map(|value| {
                        json!({"process_id":value.process_id,"session_id":value.session_id,"command":value.command,"status":value.status,"cursor":value.cursor})
                    })
                    .collect::<Vec<_>>(),
            )?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for ProcessPoll {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_poll".into(),
            description: "Read incremental background-process output from a cursor.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"cursor":{"type":"integer"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "process_poll",
            "read background process output",
            None,
        )
        .await?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let process_id = arguments
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process id")?;
        let cursor = arguments.get("cursor").and_then(Value::as_u64).unwrap_or(0);
        let output = self
            .processes
            .poll_for_session(process_id, cursor, &session_id)
            .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(
                &json!({"cursor":output.cursor,"next_cursor":output.next_cursor,"output":output.output,"truncated":output.truncated}),
            )?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for ProcessLog {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_log".into(),
            description: "Read a bounded page of background-process output.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "process_log",
            "read background process log",
            None,
        )
        .await?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let process_id = arguments
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process id")?;
        let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(16 * 1024) as usize;
        let output = self
            .processes
            .log_for_session(process_id, offset, limit, &session_id)
            .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(
                &json!({"offset":output.cursor,"next_offset":output.next_cursor,"output":output.output,"truncated":output.truncated}),
            )?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for ProcessWrite {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_write".into(),
            description: "Write raw input or a control sequence to a background process.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"},"input":{"type":"string"},"control":{"enum":["interrupt","eof","up","down","left","right"]}},"required":["process_id"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Shell,
                    tool_name: Some("process_write".into()),
                    ..ActionSignature::default()
                },
                "write input to a background process",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let process_id = arguments
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process id")?;
        let input = arguments
            .get("input")
            .and_then(Value::as_str)
            .map(str::as_bytes)
            .map(<[u8]>::to_vec)
            .or_else(|| {
                Some(
                    match arguments
                        .get("control")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "interrupt" => vec![3],
                        "eof" => vec![4],
                        "up" => b"\x1b[A".to_vec(),
                        "down" => b"\x1b[B".to_vec(),
                        "left" => b"\x1b[D".to_vec(),
                        "right" => b"\x1b[C".to_vec(),
                        _ => Vec::new(),
                    },
                )
            })
            .unwrap_or_default();
        self.processes
            .write_for_session(process_id, &input, &session_id)
            .await?;
        Ok(ToolResult::complete("input written", false))
    }
}

#[async_trait]
impl Tool for ProcessWait {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_wait".into(),
            description: "Wait for a background process to exit.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"}},"required":["process_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let process_id = arguments
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process id")?;
        authorize_read(
            &mut context,
            "process_wait",
            "wait for a background process",
            None,
        )
        .await?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let code = self
            .processes
            .wait_for_session(process_id, &session_id)
            .await?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_process_lifecycle(
                process_id,
                crate::sessions::LifecycleState {
                    state: "exited".into(),
                    detail: Some(code.to_string()),
                },
            )?;
        }
        Ok(ToolResult::complete(code.to_string(), false))
    }
}

#[async_trait]
impl Tool for ProcessKill {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "process_kill".into(),
            description: "Terminate a background process.".into(),
            parameters: json!({"type":"object","properties":{"process_id":{"type":"string"}},"required":["process_id"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Shell,
                    tool_name: Some("process_kill".into()),
                    ..ActionSignature::default()
                },
                "terminate a background process",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let process_id = arguments
            .get("process_id")
            .and_then(Value::as_str)
            .context("missing process id")?;
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        self.processes
            .kill_for_session(process_id, &session_id)
            .await?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_process_lifecycle(
                process_id,
                crate::sessions::LifecycleState {
                    state: "killed".into(),
                    detail: None,
                },
            )?;
        }
        Ok(ToolResult::complete("process terminated", false))
    }
}

#[async_trait]
impl Tool for BrowserConnect {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_connect".into(),
            description: "Connect to an explicitly approved Chrome CDP endpoint.".into(),
            parameters: json!({"type":"object","properties":{"endpoint":{"type":"string"}},"required":["endpoint"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let endpoint = arguments
            .get("endpoint")
            .and_then(Value::as_str)
            .context("missing CDP endpoint")?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Browser,
                    tool_name: Some("browser_connect".into()),
                    browser_action: Some("connect".into()),
                    network_host: Url::parse(endpoint)
                        .ok()
                        .and_then(|url| url.host_str().map(str::to_owned)),
                    ..ActionSignature::default()
                },
                &format!("connect to browser CDP endpoint {endpoint}"),
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let status = self.browser.connect(endpoint).await?;
        Ok(ToolResult::complete(serde_json::to_string(&status)?, false))
    }
}

#[async_trait]
impl Tool for BrowserStatus {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_status".into(),
            description: "Show the connected browser and target status.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Browser,
                    tool_name: Some("browser_status".into()),
                    browser_action: Some("status".into()),
                    ..ActionSignature::default()
                },
                "read browser status",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(&self.browser.status().await?)?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for BrowserNavigate {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_navigate".into(),
            description: "Navigate the connected browser to an approved URL.".into(),
            parameters: json!({"type":"object","properties":{"url":{"type":"string"}},"required":["url"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let url = arguments
            .get("url")
            .and_then(Value::as_str)
            .context("missing browser URL")?;
        let requested_host = Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Browser,
                    tool_name: Some("browser_navigate".into()),
                    browser_action: Some("navigate".into()),
                    network_host: Url::parse(url)
                        .ok()
                        .and_then(|url| url.host_str().map(str::to_owned)),
                    ..ActionSignature::default()
                },
                &format!("navigate browser to {url}"),
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let status = self.browser.navigate(url).await?;
        let final_host = Url::parse(&status.target_url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_owned));
        if final_host != requested_host {
            context
                .policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Browser,
                        tool_name: Some("browser_navigate".into()),
                        browser_action: Some("navigate".into()),
                        network_host: final_host,
                        ..ActionSignature::default()
                    },
                    &format!("allow browser redirect to {}", status.target_url),
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
        }
        Ok(ToolResult::complete(serde_json::to_string(&status)?, false))
    }
}

#[async_trait]
impl Tool for BrowserSnapshot {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_snapshot".into(),
            description: "Capture a bounded DOM snapshot from the connected browser.".into(),
            parameters: json!({"type":"object","properties":{}}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Browser,
                    tool_name: Some("browser_snapshot".into()),
                    browser_action: Some("evaluate".into()),
                    ..ActionSignature::default()
                },
                "evaluate the browser DOM for a snapshot",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        Ok(ToolResult::complete(self.browser.snapshot().await?, false))
    }
}

#[async_trait]
impl Tool for BrowserScreenshot {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "browser_screenshot".into(),
            description: "Capture a screenshot and atomically write it inside the repository."
                .into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":[]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("vera-screenshot.png"),
        ))?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Browser,
                    tool_name: Some("browser_screenshot".into()),
                    browser_action: Some("screenshot".into()),
                    canonical_path: Some(path.clone()),
                    ..ActionSignature::default()
                },
                "capture a browser screenshot",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Write,
                    tool_name: Some("browser_screenshot".into()),
                    canonical_path: Some(path.clone()),
                    ..ActionSignature::default()
                },
                "write a browser screenshot",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let bytes = self.browser.screenshot().await?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_binary_preimage(path.clone(), fs::read(&path).ok())?;
        }
        crate::browser::atomic_screenshot(&path, &bytes)?;
        Ok(ToolResult::complete(
            format!("screenshot written to {}", path.display()),
            false,
        ))
    }
}

#[async_trait]
impl Tool for ImageInspect {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "image_inspect".into(),
            description: "Inspect a bounded local PNG or JPEG image inside the repository.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing image path")?,
        ))?;
        authorize_read(
            &mut context,
            "image_inspect",
            "inspect a repository image",
            Some(path.clone()),
        )
        .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(&crate::browser::inspect_image(&path, context.guard.root())?)?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for SubagentSpawn {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_spawn".into(),
            description: "Queue one or more bounded depth-one subagent tasks.".into(),
            parameters: json!({"type":"object","properties":{"tasks":{"type":"array","items":{"type":"string"}},"task":{"type":"string"},"writes":{"type":"boolean"}},"required":[]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let tasks = arguments
            .get("tasks")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .or_else(|| {
                arguments
                    .get("task")
                    .and_then(Value::as_str)
                    .map(|task| vec![task.to_owned()])
            })
            .context("subagent_spawn requires task or tasks")?;
        let writes = arguments
            .get("writes")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Subagent,
                    tool_name: Some("subagent_spawn".into()),
                    subagent_operation: Some(
                        if writes { "spawn_write" } else { "spawn_read" }.into(),
                    ),
                    ..ActionSignature::default()
                },
                if writes {
                    "spawn write-capable subagent task(s)"
                } else {
                    "spawn read-only subagent task(s)"
                },
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        if !self.manager.is_configured() {
            anyhow::bail!("subagent provider runner is not configured");
        }
        let session_id = context
            .session
            .as_deref()
            .map(|session| session.header.id.clone())
            .unwrap_or_else(|| "unattached".into());
        let root = context.guard.root().to_path_buf();
        let mut results = Vec::new();
        for task in tasks.into_iter().take(4) {
            if task.trim().is_empty() {
                anyhow::bail!("subagent task must not be empty");
            }
            let worktree_path = if writes {
                let session = context
                    .session
                    .as_deref_mut()
                    .context("write-capable subagents require a session")?;
                let manager = WorktreeManager::new(root.clone())?;
                Some(manager.create(session).await?)
            } else {
                None
            };
            let snapshot = self
                .manager
                .spawn(
                    task,
                    writes,
                    session_id.clone(),
                    root.clone(),
                    worktree_path,
                )
                .await?;
            if let Some(session) = context.session.as_deref_mut() {
                session.record_subagent_lifecycle(
                    snapshot.agent_id.clone(),
                    crate::sessions::LifecycleState {
                        state: snapshot.status.clone(),
                        detail: Some(snapshot.task.clone()),
                    },
                )?;
            }
            results.push(snapshot);
        }
        Ok(ToolResult::complete(
            serde_json::to_string(&results)?,
            false,
        ))
    }
}

fn subagent_id(arguments: &Value) -> Result<&str> {
    arguments
        .get("agent_id")
        .and_then(Value::as_str)
        .context("missing agent_id")
}

#[async_trait]
impl Tool for SubagentStatus {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_status".into(),
            description: "Read bounded subagent lifecycle state.".into(),
            parameters: json!({"type":"object","properties":{"agent_id":{"type":"string"}},"required":["agent_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        authorize_read(
            &mut context,
            "subagent_status",
            "read subagent status",
            None,
        )
        .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(&self.manager.status(subagent_id(&arguments)?).await?)?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for SubagentWait {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_wait".into(),
            description: "Wait for a bounded subagent result.".into(),
            parameters: json!({"type":"object","properties":{"agent_id":{"type":"string"}},"required":["agent_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        authorize_read(&mut context, "subagent_wait", "wait for a subagent", None).await?;
        Ok(ToolResult::complete(
            serde_json::to_string(&self.manager.wait(subagent_id(&arguments)?).await?)?,
            false,
        ))
    }
}

#[async_trait]
impl Tool for SubagentCancel {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_cancel".into(),
            description: "Cancel a queued subagent.".into(),
            parameters: json!({"type":"object","properties":{"agent_id":{"type":"string"}},"required":["agent_id"]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Subagent,
                    tool_name: Some("subagent_cancel".into()),
                    subagent_operation: Some("cancel".into()),
                    ..ActionSignature::default()
                },
                "cancel a subagent",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let snapshot = self.manager.cancel(subagent_id(&arguments)?).await?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_subagent_lifecycle(
                snapshot.agent_id.clone(),
                crate::sessions::LifecycleState {
                    state: snapshot.status.clone(),
                    detail: Some(snapshot.summary.clone()),
                },
            )?;
        }
        Ok(ToolResult::complete(
            serde_json::to_string(&snapshot)?,
            false,
        ))
    }
}

struct SubagentDiscard {
    manager: Arc<SubagentManager>,
}

#[async_trait]
impl Tool for SubagentDiscard {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_discard".into(),
            description: "Discard an explicitly reviewed write-agent worktree.".into(),
            parameters: json!({"type":"object","properties":{"agent_id":{"type":"string"},"worktree_id":{"type":"string"}},"required":[]}),
        }
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        context
            .policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Subagent,
                    tool_name: Some("subagent_discard".into()),
                    subagent_operation: Some("discard".into()),
                    ..ActionSignature::default()
                },
                "discard a subagent worktree",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let session = context
            .session
            .as_deref_mut()
            .context("worktree decisions require a session")?;
        if let Some(agent_id) = arguments.get("agent_id").and_then(Value::as_str) {
            let snapshot = self.manager.discard(agent_id, session).await?;
            session.record_subagent_lifecycle(
                snapshot.agent_id.clone(),
                crate::sessions::LifecycleState {
                    state: snapshot.status.clone(),
                    detail: Some(snapshot.summary.clone()),
                },
            )?;
            return Ok(ToolResult::complete(
                serde_json::to_string(&snapshot)?,
                false,
            ));
        }
        let worktree_id = arguments
            .get("worktree_id")
            .and_then(Value::as_str)
            .context("subagent_discard requires agent_id or worktree_id")?;
        let info = WorktreeManager::recover(session, worktree_id)?;
        WorktreeManager::new(context.guard.root().to_path_buf())?
            .discard(&info, session)
            .await?;
        Ok(ToolResult::complete(
            format!("discarded recovered worktree {worktree_id}"),
            false,
        ))
    }
}

#[async_trait]
impl Tool for SubagentResult {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent_result".into(),
            description: "Return bounded subagent results; optionally apply or discard a reviewed worktree diff.".into(),
            parameters: json!({"type":"object","properties":{"agent_id":{"type":"string"},"apply":{"type":"boolean"},"discard":{"type":"boolean"}},"required":["agent_id"]}),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, mut context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let agent_id = subagent_id(&arguments)?.to_owned();
        let apply = arguments
            .get("apply")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let discard = arguments
            .get("discard")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if apply && discard {
            anyhow::bail!("subagent_result cannot apply and discard the same worktree");
        }
        if apply || discard {
            context
                .policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Subagent,
                        tool_name: Some("subagent_result".into()),
                        subagent_operation: Some(if apply { "merge" } else { "discard" }.into()),
                        ..ActionSignature::default()
                    },
                    if apply {
                        "merge a reviewed subagent worktree diff"
                    } else {
                        "discard a subagent worktree"
                    },
                    context.approval,
                    context.session.as_deref_mut(),
                )
                .await?;
            let session = context
                .session
                .as_deref_mut()
                .context("worktree decisions require a session")?;
            let snapshot = if apply {
                self.manager.merge(&agent_id, session).await?
            } else {
                self.manager.discard(&agent_id, session).await?
            };
            session.record_subagent_lifecycle(
                snapshot.agent_id.clone(),
                crate::sessions::LifecycleState {
                    state: snapshot.status.clone(),
                    detail: Some(snapshot.summary.clone()),
                },
            )?;
            return Ok(ToolResult::complete(
                serde_json::to_string(&snapshot)?,
                false,
            ));
        }
        authorize_read(
            &mut context,
            "subagent_result",
            "read a subagent result",
            None,
        )
        .await?;
        Ok(ToolResult::complete(
            serde_json::to_string(&self.manager.wait(&agent_id).await?)?,
            false,
        ))
    }
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.vera-tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    Ok(())
}

pub async fn execute(
    registry: &ToolRegistry,
    call: ToolCall,
    context: ToolContext<'_>,
) -> Result<ToolResult> {
    let tool = registry.find(&call.name).context("unknown tool")?;
    tool.call(context, call.arguments).await
}

pub type SharedToolRegistry = Arc<Mutex<ToolRegistry>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_bounded_mcp_content_kinds() {
        let content = normalize_mcp_result(&json!({
            "content": [
                {"type":"text","text":"hello"},
                {"type":"resource_link","uri":"file:///fixture"},
                {"type":"image","mimeType":"image/png","data":"AAAA"}
            ],
            "structuredContent":{"ok":true}
        }));
        assert!(content.contains("hello"));
        assert!(content.contains("file:///fixture"));
        assert!(content.contains("MCP image"));
        assert!(content.contains("structured"));
        assert!(content.chars().count() <= 64_000);
    }
}
