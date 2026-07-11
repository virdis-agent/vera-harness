use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::time::Duration;

use crate::providers::ToolSchema;
use crate::safety::{ApprovalHandler, PathGuard, PermissionKind, PermissionPolicy, Sandbox};
use crate::sessions::Session;

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
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
    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn standard() -> Self {
        Self {
            tools: vec![
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
                Arc::new(Subagent),
            ],
        }
    }
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|tool| tool.schema()).collect()
    }
    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .cloned()
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
struct Subagent;

#[async_trait]
impl Tool for ListFiles {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_files".into(),
            description: "List repository files, respecting ignore rules.".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":[]}),
        }
    }
    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments.get("path").and_then(Value::as_str).unwrap_or("."),
        ))?;
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
        Ok(ToolResult {
            content: files.join("\n"),
            is_error: false,
        })
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
    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
        let content = fs::read_to_string(path).context("read file")?;
        Ok(ToolResult {
            content,
            is_error: false,
        })
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
    async fn call(&self, context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let query = arguments
            .get("query")
            .and_then(Value::as_str)
            .context("missing query")?;
        let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
        let root = context.guard.resolve(Path::new(path))?;
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
        Ok(ToolResult {
            content: format!("{}{}", output.stdout, output.stderr),
            is_error: output.status != 0,
        })
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
        context
            .policy
            .authorize(
                PermissionKind::Write,
                "write a repository file",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
        let content = arguments
            .get("content")
            .and_then(Value::as_str)
            .context("missing content")?;
        if let Some(session) = context.session.as_deref_mut() {
            session.record_preimage(path.clone(), fs::read_to_string(&path).ok())?;
        }
        atomic_write(&path, content)?;
        Ok(ToolResult {
            content: format!("wrote {}", path.display()),
            is_error: false,
        })
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
        context
            .policy
            .authorize(
                PermissionKind::Write,
                "apply a repository patch",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let path = context.guard.resolve(Path::new(
            arguments
                .get("path")
                .and_then(Value::as_str)
                .context("missing path")?,
        ))?;
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
        Ok(ToolResult {
            content: format!("patched {}", path.display()),
            is_error: false,
        })
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
    async fn call(&self, context: ToolContext<'_>, _arguments: Value) -> Result<ToolResult> {
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
        Ok(ToolResult {
            content: format!("{}\n{}", status.stdout, diff.stdout),
            is_error: status.status != 0 || diff.status != 0,
        })
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
        context
            .policy
            .authorize(
                PermissionKind::Shell,
                "run a shell command",
                context.approval,
                context.session.as_deref_mut(),
            )
            .await?;
        let command = arguments
            .get("command")
            .and_then(Value::as_str)
            .context("missing command")?;
        let network = arguments
            .get("network")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if network {
            context
                .policy
                .authorize(
                    PermissionKind::Network,
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
        Ok(ToolResult {
            content: format!("{}{}", output.stdout, output.stderr),
            is_error: output.status != 0,
        })
    }
}

#[async_trait]
impl Tool for Question {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "question".into(),
            description: "Ask the user for a missing decision before continuing.".into(),
            parameters: json!({"type":"object","properties":{"question":{"type":"string"}},"required":["question"]}),
        }
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let question = arguments
            .get("question")
            .and_then(Value::as_str)
            .context("missing question")?;
        Ok(ToolResult {
            content: format!("User input required: {question}"),
            is_error: false,
        })
    }
}

#[async_trait]
impl Tool for PlanManagement {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "plan".into(),
            description: "Record or revise a concise plan before making changes.".into(),
            parameters: json!({"type":"object","properties":{"steps":{"type":"array","items":{"type":"string"}}},"required":["steps"]}),
        }
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        let steps = arguments
            .get("steps")
            .and_then(Value::as_array)
            .context("missing plan steps")?;
        Ok(ToolResult {
            content: format!("plan accepted with {} step(s)", steps.len()),
            is_error: false,
        })
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
        .authorize(
            PermissionKind::Network,
            &format!("use provider-native {name} for {query}"),
            context.approval,
            context.session.as_deref_mut(),
        )
        .await?;
    Ok(ToolResult {
        content: format!("provider-native {name} search requested for {query}"),
        is_error: false,
    })
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
        provider_search("web", context, arguments).await
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
        provider_search("X", context, arguments).await
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
        Ok(ToolResult {
            content: format!("queued session task: {task}"),
            is_error: false,
        })
    }
}

#[async_trait]
impl Tool for Subagent {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "subagent".into(),
            description: "Delegate one depth-one read-oriented task to a bounded subagent.".into(),
            parameters: json!({"type":"object","properties":{"task":{"type":"string"},"writes":{"type":"boolean"}},"required":["task"]}),
        }
    }

    async fn call(&self, _context: ToolContext<'_>, arguments: Value) -> Result<ToolResult> {
        if arguments
            .get("writes")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(ToolResult {
                content: "subagent writes require serialized approval and a separate coordinator"
                    .into(),
                is_error: true,
            });
        }
        let task = arguments
            .get("task")
            .and_then(Value::as_str)
            .context("missing task")?;
        Ok(ToolResult {
            content: format!("bounded subagent task accepted: {task}"),
            is_error: false,
        })
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
