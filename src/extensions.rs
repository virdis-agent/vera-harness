use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::AtomicU64};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio::time::{Duration, timeout};

use crate::auth::redact;
use crate::paths::VeraPaths;
use crate::safety::{ActionSignature, ApprovalHandler, PermissionKind, PermissionPolicy, Sandbox};
use crate::sessions::{Session, SessionRecord};

pub fn discover_agents(project: &Path, global: Option<&Path>) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    if let Some(global) = global
        && global.exists()
    {
        paths.push(global.to_path_buf());
    }
    let home_agent = dirs::home_dir().map(|home| home.join(".vera/AGENTS.md"));
    if let Some(path) = home_agent
        && path.exists()
        && !paths.contains(&path)
    {
        paths.push(path);
    }
    let project = fs::canonicalize(project)?;
    let mut ancestors = project.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        let path = ancestor.join("AGENTS.md");
        if path.exists() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
    loaded: std::collections::BTreeSet<String>,
}

impl SkillCatalog {
    pub fn load(paths: &VeraPaths, project: &Path) -> Result<Self> {
        Self::load_with_plugins(paths, project, &[], &[])
    }

    pub fn load_with_plugins(
        paths: &VeraPaths,
        project: &Path,
        plugins: &[PluginManifest],
        allowed: &[String],
    ) -> Result<Self> {
        let mut catalog = Self::default();
        // Insert from lowest to highest precedence. Project skills override
        // global skills; duplicate entries inside one source are errors.
        catalog.add_root(&paths.skills, "global", allowed)?;
        for plugin in plugins {
            let plugin_root = paths.plugins.join(&plugin.name);
            for relative in &plugin.skills {
                let root = safe_plugin_path(&plugin_root, relative)?;
                catalog.add_root(&root, &format!("plugin:{}", plugin.name), allowed)?;
            }
        }
        catalog.add_root(&project.join(".vera/skills"), "project", allowed)?;
        Ok(catalog)
    }

    fn add_root(&mut self, root: &Path, source: &str, allowed: &[String]) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        if root.is_file() {
            if root.file_name().is_none_or(|name| name != "SKILL.md") {
                anyhow::bail!("skill path must name SKILL.md: {}", root.display());
            }
            let mut skill = parse_skill(root)?;
            if allowed.is_empty() || allowed.contains(&skill.name) {
                if let Some(existing) = self.skills.get(&skill.name)
                    && (existing.source == source
                        || (existing.source.starts_with("plugin:")
                            && source.starts_with("plugin:")))
                {
                    anyhow::bail!("duplicate skill name {} in {source}", skill.name);
                }
                skill.source = source.into();
                self.skills.insert(skill.name.clone(), skill);
            }
            return Ok(());
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            let skill_file = if path.is_dir() {
                path.join("SKILL.md")
            } else {
                path
            };
            if skill_file.file_name().is_none_or(|name| name != "SKILL.md") {
                continue;
            }
            let mut skill = parse_skill(&skill_file)?;
            if !allowed.is_empty() && !allowed.contains(&skill.name) {
                continue;
            }
            if !names.insert(skill.name.clone()) {
                anyhow::bail!("duplicate skill name {} in {source}", skill.name);
            }
            if self.skills.get(&skill.name).is_some_and(|existing| {
                (existing.source.starts_with("plugin:") && source.starts_with("plugin:"))
                    || existing.source == source
            }) {
                anyhow::bail!("duplicate skill name {} in {source}", skill.name);
            }
            skill.source = source.into();
            self.skills.insert(skill.name.clone(), skill);
        }
        Ok(())
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.skills.keys()
    }
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
    pub fn activate(&self, name: &str) -> Option<&Skill> {
        self.get(name)
    }
    pub fn active_descriptions(&self) -> Vec<(String, String)> {
        self.skills
            .values()
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .collect()
    }
    pub fn loaded_names(&self) -> impl Iterator<Item = &String> {
        self.loaded.iter()
    }
    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.contains(name)
    }
    pub fn load_body(&mut self, name: &str) -> Result<String> {
        let skill = self
            .skills
            .get(name)
            .with_context(|| format!("skill {name} is unavailable"))?;
        let body = fs::read_to_string(&skill.path)
            .with_context(|| format!("read skill {name} from {}", skill.path.display()))?;
        self.loaded.insert(name.into());
        Ok(body)
    }
    pub fn unload(&mut self, name: &str) -> Result<()> {
        if !self.skills.contains_key(name) {
            anyhow::bail!("skill {name} is unavailable");
        }
        self.loaded.remove(name);
        Ok(())
    }
    pub fn loaded_bodies(&self) -> Result<Vec<(String, String)>> {
        self.loaded
            .iter()
            .map(|name| {
                let skill = self.skills.get(name).context("loaded skill disappeared")?;
                Ok((name.clone(), fs::read_to_string(&skill.path)?))
            })
            .collect()
    }
}

fn parse_skill(path: &Path) -> Result<Skill> {
    let text = fs::read_to_string(path)?;
    let mut name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .unwrap_or("skill")
        .to_string();
    let mut description = String::new();
    for line in text.lines().take(40) {
        if let Some(value) = line.strip_prefix("name:") {
            name = value.trim().trim_matches('"').into();
        }
        if let Some(value) = line.strip_prefix("description:") {
            description = value.trim().trim_matches('"').into();
        }
    }
    if description.is_empty() {
        description = "No description provided.".into();
    }
    if name.trim().is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        anyhow::bail!("invalid skill name {name:?}");
    }
    Ok(Skill {
        name,
        description,
        path: path.to_path_buf(),
        source: String::new(),
    })
}

fn safe_plugin_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.trim().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        anyhow::bail!("plugin path must be relative and stay inside the plugin: {relative}");
    }
    Ok(root.join(path))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct PromptCatalog {
    prompts: BTreeMap<String, PromptTemplate>,
}

impl PromptCatalog {
    pub fn load(
        paths: &VeraPaths,
        project: &Path,
        plugins: &[PluginManifest],
        extra_roots: &[PathBuf],
    ) -> Result<Self> {
        let mut catalog = Self::default();
        catalog.add_root(&paths.prompts, "global")?;
        for root in extra_roots {
            catalog.add_root(root, "configured")?;
        }
        for plugin in plugins {
            let plugin_root = paths.plugins.join(&plugin.name);
            for relative in &plugin.prompts {
                catalog.add_root(
                    &safe_plugin_path(&plugin_root, relative)?,
                    &format!("plugin:{}", plugin.name),
                )?;
            }
        }
        // Project templates have the highest precedence.
        catalog.add_root(&project.join(".vera/prompts"), "project")?;
        Ok(catalog)
    }

    fn add_root(&mut self, root: &Path, source: &str) -> Result<()> {
        if !root.exists() {
            return Ok(());
        }
        if root.is_file() {
            if root.extension().and_then(|value| value.to_str()) != Some("md") {
                anyhow::bail!("prompt path must name a Markdown file: {}", root.display());
            }
            let name = root
                .file_stem()
                .and_then(|value| value.to_str())
                .context("prompt file has no name")?;
            self.insert_prompt(root, name, source)?;
            return Ok(());
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if name.is_empty() || !names.insert(name.to_owned()) {
                anyhow::bail!("duplicate or empty prompt name in {}", root.display());
            }
            self.insert_prompt(&path, name, source)?;
        }
        Ok(())
    }

    fn insert_prompt(&mut self, path: &Path, name: &str, source: &str) -> Result<()> {
        if self.prompts.get(name).is_some_and(|existing| {
            (existing.source.starts_with("plugin:") && source.starts_with("plugin:"))
                || existing.source == source
        }) {
            anyhow::bail!("duplicate prompt name {name} in {source}");
        }
        let text = fs::read_to_string(path)?;
        let description = text
            .lines()
            .find(|line| line.starts_with("# "))
            .map(|line| line.trim_start_matches("# ").trim().to_owned())
            .unwrap_or_else(|| "Reusable prompt template".into());
        self.prompts.insert(
            name.into(),
            PromptTemplate {
                name: name.into(),
                description,
                path: path.to_path_buf(),
                source: source.into(),
            },
        );
        Ok(())
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.prompts.keys()
    }
    pub fn get(&self, name: &str) -> Option<&PromptTemplate> {
        self.prompts.get(name)
    }
    pub fn preview(&self, name: &str) -> Result<String> {
        let template = self
            .prompts
            .get(name)
            .context("prompt template not found")?;
        Ok(fs::read_to_string(&template.path)?)
    }
    pub fn expand(&self, name: &str, args: &str) -> Result<String> {
        Ok(self.preview(name)?.replace("{{args}}", args))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookEvent {
    pub version: u32,
    pub event: String,
    pub session_id: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HookSpec {
    pub name: String,
    pub command: String,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_hook_events")]
    pub events: Vec<String>,
}

fn default_hook_timeout_ms() -> u64 {
    10_000
}

fn default_hook_events() -> Vec<String> {
    vec![
        "session_start".into(),
        "session_end".into(),
        "before_turn".into(),
        "after_turn".into(),
        "before_tool".into(),
        "after_tool".into(),
    ]
}

pub struct HookRunner;

impl HookRunner {
    pub async fn run(
        &self,
        spec: &HookSpec,
        event: HookEvent,
        policy: &mut PermissionPolicy,
        approval: &mut dyn ApprovalHandler,
    ) -> Result<String> {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        self.run_in(spec, event, &root, policy, approval, None)
            .await
    }

    pub async fn run_in(
        &self,
        spec: &HookSpec,
        event: HookEvent,
        root: &Path,
        policy: &mut PermissionPolicy,
        approval: &mut dyn ApprovalHandler,
        mut session: Option<&mut Session>,
    ) -> Result<String> {
        policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Hook,
                    tool_name: Some(format!("hook:{}", spec.name)),
                    ..ActionSignature::default()
                },
                &format!("run hook {}", spec.name),
                approval,
                session.as_deref_mut(),
            )
            .await?;
        let payload = serde_json::to_vec(&event)?;
        // Hooks are intentionally read-only and receive a minimal environment;
        // in particular HOME/auth-store variables are never forwarded.
        let mut command = Sandbox::read_only_command(
            "/bin/zsh",
            &["-lc".into(), spec.command.clone()],
            root,
            false,
        );
        command.env("VERA_REPOSITORY_ROOT", root);
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&payload).await?;
        }
        let output = timeout(
            Duration::from_millis(spec.timeout_ms.max(1)),
            child.wait_with_output(),
        )
        .await
        .context("hook timed out")??;
        if !output.status.success() {
            if let Some(session) = session.as_deref_mut() {
                session.append(SessionRecord::Hook {
                    name: spec.name.clone(),
                    event: event.event.clone(),
                    output: redact(&String::from_utf8_lossy(&output.stderr)),
                    success: false,
                })?;
            }
            anyhow::bail!(
                "hook failed: {}",
                redact(&String::from_utf8_lossy(&output.stderr))
            );
        }
        let result = redact(&String::from_utf8_lossy(&output.stdout));
        if let Some(session) = session {
            session.append(SessionRecord::Hook {
                name: spec.name.clone(),
                event: event.event,
                output: result.clone(),
                success: true,
            })?;
        }
        Ok(result)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
    #[serde(default)]
    pub mcp: Vec<McpSpec>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default, alias = "prompt_paths", alias = "prompt_dirs")]
    pub prompts: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_mcp_timeout_ms")]
    pub timeout_ms: u64,
    /// Empty means all tools advertised by the server; otherwise only these
    /// exact names are allowed to reach the provider schema.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub network: bool,
}

fn default_mcp_timeout_ms() -> u64 {
    15_000
}

const MAX_MCP_MESSAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_MCP_STDERR_BYTES: usize = 16 * 1024;
const MAX_MCP_TOOL_NAME_CHARS: usize = 256;
const MAX_MCP_TOOL_DESCRIPTION_CHARS: usize = 8 * 1024;
const MAX_MCP_SCHEMA_BYTES: usize = 128 * 1024;
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

fn supported_mcp_protocol_version(version: &str) -> bool {
    matches!(version, MCP_PROTOCOL_VERSION | "2024-11-05")
}

/// Transport boundary for MCP. Phase 1 implements stdio; a future
/// Streamable-HTTP transport can satisfy this trait without changing tool
/// registration or permission evaluation.
#[async_trait]
pub trait McpTransport: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value>;
    async fn shutdown(&self) -> Result<()>;
}

impl PluginManifest {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty()
            || self.name.contains('/')
            || self.name.contains('\\')
            || self.name.contains("..")
        {
            anyhow::bail!("invalid plugin name {}", self.name);
        }
        if self.version.trim().is_empty() {
            anyhow::bail!("plugin {} has no version", self.name);
        }
        for (kind, paths) in [
            ("skill", &self.skills),
            ("prompt", &self.prompts),
            ("role", &self.roles),
        ] {
            for path in paths {
                safe_plugin_path(Path::new("."), path)
                    .with_context(|| format!("invalid {kind} path in plugin {}", self.name))?;
            }
        }
        let mut names = std::collections::BTreeSet::new();
        for mcp in &self.mcp {
            if mcp.name.trim().is_empty() || !names.insert(&mcp.name) {
                anyhow::bail!("plugin {} has duplicate MCP name", self.name);
            }
            if mcp.command.trim().is_empty() || mcp.timeout_ms == 0 {
                anyhow::bail!("plugin {} has an invalid MCP declaration", self.name);
            }
            if mcp.allowed_tools.iter().any(|tool| tool.trim().is_empty()) {
                anyhow::bail!(
                    "plugin {} has an empty MCP tool allow-list entry",
                    self.name
                );
            }
        }
        Ok(())
    }
}

pub struct PluginManager {
    paths: VeraPaths,
}

impl PluginManager {
    pub fn new(paths: VeraPaths) -> Self {
        Self { paths }
    }
    pub fn list(&self) -> Result<Vec<PluginManifest>> {
        if !self.paths.plugins.exists() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for entry in fs::read_dir(&self.paths.plugins)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            let manifest = entry.path().join("vera-plugin.toml");
            if manifest.exists() {
                let value: PluginManifest = toml::from_str(&fs::read_to_string(manifest)?)?;
                value.validate()?;
                if !names.insert(value.name.clone()) {
                    anyhow::bail!("duplicate plugin name {}", value.name);
                }
                result.push(value);
            }
        }
        Ok(result)
    }

    pub fn add_local(&self, source: &Path) -> Result<PluginManifest> {
        let manifest_path = source.join("vera-plugin.toml");
        if fs::symlink_metadata(&manifest_path)?
            .file_type()
            .is_symlink()
        {
            anyhow::bail!("plugin manifest must not be a symlink");
        }
        let manifest: PluginManifest = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
        // The caller owns the async approval boundary for CLI operations; the explicit
        // add path also rejects scripts because manifests are data-only in v1.
        manifest.validate()?;
        let destination = self.paths.plugins.join(&manifest.name);
        if destination.exists() {
            fs::remove_dir_all(&destination)?;
        }
        copy_tree(source, &destination)?;
        Ok(manifest)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        if name.contains('/') || name.contains("..") {
            anyhow::bail!("invalid plugin name");
        }
        let path = self.paths.plugins.join(name);
        if path.exists() {
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                anyhow::bail!("plugin path must not be a symlink");
            }
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!("plugin tree must not contain symlinks: {}", from.display());
        }
        if metadata.is_dir() {
            copy_tree(&from, &to)?;
        } else if metadata.is_file() {
            fs::copy(from, to)?;
        }
    }
    Ok(())
}

pub struct McpRegistry {
    paths: VeraPaths,
}

impl McpRegistry {
    pub fn new(paths: VeraPaths) -> Self {
        Self { paths }
    }
    pub fn list(&self) -> Result<Vec<McpSpec>> {
        let mut result = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for manifest in PluginManager::new(self.paths.clone()).list()? {
            for spec in manifest.mcp {
                if !names.insert(spec.name.clone()) {
                    anyhow::bail!("duplicate MCP server name {}", spec.name);
                }
                result.push(spec);
            }
        }
        Ok(result)
    }
    pub async fn test(
        &self,
        name: &str,
        policy: &mut PermissionPolicy,
        approval: &mut dyn ApprovalHandler,
    ) -> Result<String> {
        let root = std::env::current_dir()?;
        self.test_in(name, policy, approval, &root).await
    }

    pub async fn test_in(
        &self,
        name: &str,
        policy: &mut PermissionPolicy,
        approval: &mut dyn ApprovalHandler,
        root: &Path,
    ) -> Result<String> {
        let spec = self
            .list()?
            .into_iter()
            .find(|spec| spec.name == name)
            .context("MCP server not found")?;
        policy
            .authorize_action(
                ActionSignature {
                    permission_kind: PermissionKind::Mcp,
                    tool_name: Some("mcp_server_start".into()),
                    mcp_server: Some(spec.name.clone()),
                    ..ActionSignature::default()
                },
                &format!("start MCP server {}", spec.name),
                approval,
                None,
            )
            .await?;
        if spec.network {
            policy
                .authorize_action(
                    ActionSignature {
                        permission_kind: PermissionKind::Network,
                        tool_name: Some("mcp_server_network".into()),
                        mcp_server: Some(spec.name.clone()),
                        ..ActionSignature::default()
                    },
                    &format!("allow network for MCP server {}", spec.name),
                    approval,
                    None,
                )
                .await?;
        }
        let client = McpClient::new(spec.clone(), root.to_path_buf());
        let tools = match client.tools().await {
            Ok(tools) => tools,
            Err(error) => {
                let stderr = client.stderr().await;
                let _ = client.shutdown().await;
                anyhow::bail!(
                    "MCP server {} failed: {}; stderr={}",
                    spec.name,
                    redact(&error.to_string()),
                    stderr
                );
            }
        };
        let stderr = client.stderr().await;
        client.shutdown().await?;
        Ok(redact(
            &serde_json::json!({
                "server": spec.name,
                "server_info": client.server_info().await,
                "tools": tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
                "stderr": stderr,
            })
            .to_string(),
        ))
    }
}

/// A persistent line-oriented MCP JSON-RPC client. It starts lazily on the
/// first `tools` or `call` request and is shut down explicitly by the runtime.
pub struct McpClient {
    spec: McpSpec,
    root: PathBuf,
    sandboxed: bool,
    process: Mutex<Option<Arc<McpProcess>>>,
    server_info: Arc<Mutex<Value>>,
    last_stderr: Arc<Mutex<String>>,
    next_id: AtomicU64,
}

struct McpProcess {
    child: Mutex<tokio::process::Child>,
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    pending: Arc<Mutex<BTreeMap<u64, oneshot::Sender<Value>>>>,
    reader_task: tokio::task::JoinHandle<()>,
    stderr: Arc<Mutex<String>>,
    stderr_task: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpToolDescription {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, alias = "inputSchema")]
    pub input_schema: Value,
    #[serde(default)]
    pub annotations: Value,
}

impl McpClient {
    pub fn new(spec: McpSpec, root: PathBuf) -> Self {
        Self {
            spec,
            root,
            sandboxed: true,
            process: Mutex::new(None),
            server_info: Arc::new(Mutex::new(Value::Null)),
            last_stderr: Arc::new(Mutex::new(String::new())),
            next_id: AtomicU64::new(1),
        }
    }

    #[cfg(test)]
    fn for_fixture(spec: McpSpec, root: PathBuf) -> Self {
        Self {
            spec,
            root,
            sandboxed: false,
            process: Mutex::new(None),
            server_info: Arc::new(Mutex::new(Value::Null)),
            last_stderr: Arc::new(Mutex::new(String::new())),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub async fn server_info(&self) -> Value {
        self.server_info.lock().await.clone()
    }

    pub async fn is_started(&self) -> bool {
        self.process.lock().await.is_some()
    }

    async fn ensure_started(&self) -> Result<Arc<McpProcess>> {
        let mut process = self.process.lock().await;
        if process.is_none() {
            self.last_stderr.lock().await.clear();
            let mut command = if self.sandboxed {
                Sandbox::read_only_command(
                    &self.spec.command,
                    &self.spec.args,
                    &self.root,
                    self.spec.network,
                )
            } else {
                let mut command = Command::new(&self.spec.command);
                command
                    .args(&self.spec.args)
                    .env_clear()
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .current_dir(&self.root);
                command
            };
            command.env("VERA_REPOSITORY_ROOT", &self.root);
            let mut child = command.spawn().context("spawn MCP server")?;
            let stdin = child.stdin.take().context("MCP stdin unavailable")?;
            let stdout = child.stdout.take().context("MCP stdout unavailable")?;
            let pending = Arc::new(Mutex::new(BTreeMap::<u64, oneshot::Sender<Value>>::new()));
            let reader_pending = pending.clone();
            let reader_task = tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let Ok(Some(line)) = read_bounded_mcp_line(&mut reader).await else {
                        break;
                    };
                    if line.is_empty() {
                        break;
                    }
                    let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = value.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    if let Some(sender) = reader_pending.lock().await.remove(&id) {
                        let _ = sender.send(value);
                    }
                }
            });
            let stderr = Arc::new(Mutex::new(String::new()));
            let stderr_buffer = stderr.clone();
            let stderr_pipe = child.stderr.take().context("MCP stderr unavailable")?;
            let stderr_task = tokio::spawn(async move {
                let reader = BufReader::new(stderr_pipe);
                let mut bytes = Vec::new();
                let mut limited = reader.take((MAX_MCP_STDERR_BYTES + 1) as u64);
                let _ = limited.read_to_end(&mut bytes).await;
                let text = redact(&String::from_utf8_lossy(&bytes));
                let bounded: String = text.chars().take(MAX_MCP_STDERR_BYTES).collect();
                *stderr_buffer.lock().await = bounded;
            });
            let created = Arc::new(McpProcess {
                child: Mutex::new(child),
                stdin: Mutex::new(Some(stdin)),
                pending,
                reader_task,
                stderr,
                stderr_task,
            });
            let initialize = match self
                .request_locked(
                    &created,
                    "initialize",
                    serde_json::json!({
                        "protocolVersion":MCP_PROTOCOL_VERSION,
                        "capabilities":{},
                        "clientInfo":{"name":"vera","version":env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    shutdown_mcp_process(created).await;
                    return Err(error);
                }
            };
            if initialize.get("error").is_some() {
                shutdown_mcp_process(created).await;
                anyhow::bail!("MCP initialize failed: {}", redact(&initialize.to_string()));
            }
            let Some(negotiated_version) =
                initialize.get("protocolVersion").and_then(Value::as_str)
            else {
                shutdown_mcp_process(created).await;
                anyhow::bail!("MCP initialize did not negotiate a protocol version");
            };
            if !supported_mcp_protocol_version(negotiated_version) {
                shutdown_mcp_process(created).await;
                anyhow::bail!("unsupported MCP protocol version {negotiated_version}");
            }
            if initialize
                .get("capabilities")
                .and_then(|value| value.get("tools"))
                .and_then(Value::as_object)
                .is_none()
            {
                shutdown_mcp_process(created).await;
                anyhow::bail!("MCP server did not advertise the tools capability");
            }
            if let Some(server_info) = initialize.get("serverInfo")
                && serde_json::to_vec(server_info)?.len() <= 8 * 1024
            {
                *self.server_info.lock().await = server_info.clone();
            }
            // Notifications have no response and are safe to send before the
            // first request on a persistent session.
            if let Err(error) = self
                .notify(
                    &created,
                    serde_json::json!({
                        "jsonrpc":"2.0",
                        "method":"notifications/initialized"
                    }),
                )
                .await
            {
                shutdown_mcp_process(created).await;
                return Err(error);
            }
            *process = Some(created);
        }
        Ok(process
            .as_ref()
            .expect("MCP process was just initialized")
            .clone())
    }

    async fn request_locked(
        &self,
        process: &McpProcess,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let (sender, receiver) = oneshot::channel();
        process.pending.lock().await.insert(id, sender);
        if let Err(error) = self.write_message(process, &request).await {
            process.pending.lock().await.remove(&id);
            return Err(error);
        }
        let value =
            match timeout(Duration::from_millis(self.spec.timeout_ms.max(1)), receiver).await {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => anyhow::bail!("MCP server exited before replying to {method}"),
                Err(_) => {
                    process.pending.lock().await.remove(&id);
                    let cancellation = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": {"requestId": id, "reason": "timeout"}
                    });
                    let _ = self.write_message(process, &cancellation).await;
                    anyhow::bail!("MCP request {method} timed out")
                }
            };
        if let Some(error) = value.get("error") {
            anyhow::bail!("MCP {method} error: {}", redact(&error.to_string()));
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, process: &McpProcess, message: Value) -> Result<()> {
        self.write_message(process, &message).await
    }

    async fn write_message(&self, process: &McpProcess, message: &Value) -> Result<()> {
        let encoded = serde_json::to_vec(message)?;
        if encoded.len() > MAX_MCP_MESSAGE_BYTES {
            anyhow::bail!("MCP message exceeds {} byte limit", MAX_MCP_MESSAGE_BYTES);
        }
        let mut stdin = process.stdin.lock().await;
        let stdin = stdin.as_mut().context("MCP stdin is closed")?;
        stdin.write_all(&encoded).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn tools(&self) -> Result<Vec<McpToolDescription>> {
        let process = self.ensure_started().await?;
        let response = match self
            .request_locked(&process, "tools/list", serde_json::json!({}))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.shutdown().await?;
                return Err(error);
            }
        };
        let parsed = (|| -> Result<Vec<McpToolDescription>> {
            let mut tools = Vec::new();
            let mut seen_names = std::collections::BTreeSet::new();
            let advertised_tools = response
                .get("tools")
                .and_then(Value::as_array)
                .context("MCP tools/list response is missing a tools array")?;
            for value in advertised_tools.iter().cloned() {
                let Ok(mut tool) = serde_json::from_value::<McpToolDescription>(value) else {
                    continue;
                };
                if tool.name.trim().is_empty() {
                    continue;
                }
                if !seen_names.insert(tool.name.clone()) {
                    continue;
                }
                if tool.name.chars().count() > MAX_MCP_TOOL_NAME_CHARS
                    || tool.description.chars().count() > MAX_MCP_TOOL_DESCRIPTION_CHARS
                {
                    continue;
                }
                if tool.input_schema.is_null() {
                    tool.input_schema = serde_json::json!({"type":"object","properties":{}});
                }
                if !tool.input_schema.is_object()
                    || serde_json::to_vec(&tool.input_schema)?.len() > MAX_MCP_SCHEMA_BYTES
                {
                    continue;
                }
                if tool
                    .input_schema
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "object")
                {
                    continue;
                }
                tools.push(tool);
            }
            Ok(tools)
        })();
        if parsed.is_err() {
            let _ = self.shutdown().await;
        }
        parsed
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value> {
        let process = self.ensure_started().await?;
        match self
            .request_locked(
                &process,
                "tools/call",
                serde_json::json!({"name":name,"arguments":arguments}),
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(error) => {
                self.shutdown().await?;
                Err(error)
            }
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let mut guard = self.process.lock().await;
        if let Some(process) = guard.take() {
            *self.last_stderr.lock().await = process.stderr.lock().await.clone();
            shutdown_mcp_process(process).await;
        }
        Ok(())
    }

    pub async fn stderr(&self) -> String {
        let guard = self.process.lock().await;
        let buffer = guard.as_ref().map(|process| process.stderr.clone());
        drop(guard);
        match buffer {
            Some(buffer) => buffer.lock().await.clone(),
            None => self.last_stderr.lock().await.clone(),
        }
    }
}

async fn shutdown_mcp_process(process: Arc<McpProcess>) {
    process.reader_task.abort();
    process.stderr_task.abort();
    let _ = process.stdin.lock().await.take();
    let mut child = process.child.lock().await;
    if timeout(Duration::from_millis(500), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

async fn read_bounded_mcp_line<R>(reader: &mut BufReader<R>) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if line.is_empty() && !oversized {
                return Ok(None);
            }
            return Ok(Some(if oversized { vec![b' '] } else { line }));
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(chunk.len(), |index| index + 1);
        if !oversized {
            if line.len().saturating_add(take) <= MAX_MCP_MESSAGE_BYTES {
                line.extend_from_slice(&chunk[..take]);
            } else {
                oversized = true;
            }
        }
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(if oversized { vec![b' '] } else { line }));
        }
    }
}

pub struct StdioMcpTransport {
    client: Arc<McpClient>,
}

impl StdioMcpTransport {
    pub fn new(client: Arc<McpClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl McpTransport for StdioMcpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        match method {
            "tools/list" => Ok(serde_json::to_value(self.client.tools().await?)?),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .context("MCP tools/call is missing name")?;
                self.client
                    .call(
                        name,
                        params.get("arguments").cloned().unwrap_or(Value::Null),
                    )
                    .await
            }
            _ => anyhow::bail!("unsupported stdio MCP operation {method}"),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        self.client.shutdown().await
    }
}

pub fn mcp_tool_is_read_only(tool: &McpToolDescription) -> bool {
    if tool
        .annotations
        .get("readOnlyHint")
        .and_then(Value::as_bool)
        == Some(true)
        || tool
            .annotations
            .get("read_only_hint")
            .and_then(Value::as_bool)
            == Some(true)
    {
        return true;
    }
    let name = tool.name.to_ascii_lowercase();
    [
        "read", "list", "get", "find", "search", "query", "describe", "fetch",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

pub struct SubagentCoordinator {
    permits: Arc<Semaphore>,
    writes: Arc<Mutex<()>>,
}

impl Default for SubagentCoordinator {
    fn default() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(4)),
            writes: Arc::new(Mutex::new(())),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub task: String,
    pub depth: u8,
    pub writes: bool,
}

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn run(&self, request: SubagentRequest) -> Result<String>;
}

impl SubagentCoordinator {
    pub async fn run(
        &self,
        runner: &dyn SubagentRunner,
        request: SubagentRequest,
    ) -> Result<String> {
        if request.depth > 0 {
            anyhow::bail!("subagents cannot recurse");
        }
        let _permit = self.permits.acquire().await?;
        let _write_guard = if request.writes {
            Some(self.writes.lock().await)
        } else {
            None
        };
        runner.run(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_advertise_metadata_before_body_load() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let paths = VeraPaths::from_home(home).unwrap();
        fs::create_dir_all(paths.skills.join("review")).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            paths.skills.join("review/SKILL.md"),
            "name: review\ndescription: Review changes\n\nfull private body",
        )
        .unwrap();
        let catalog = SkillCatalog::load(&paths, &project).unwrap();
        assert_eq!(catalog.names().cloned().collect::<Vec<_>>(), vec!["review"]);
        assert!(
            !catalog.active_descriptions()[0]
                .1
                .contains("full private body")
        );
        assert!(catalog.loaded_bodies().unwrap().is_empty());
    }

    #[test]
    fn prompt_project_precedence_and_argument_expansion() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let paths = VeraPaths::from_home(home).unwrap();
        fs::create_dir_all(&paths.prompts).unwrap();
        fs::create_dir_all(project.join(".vera/prompts")).unwrap();
        fs::write(paths.prompts.join("review.md"), "# Global\nGlobal {{args}}").unwrap();
        fs::write(
            project.join(".vera/prompts/review.md"),
            "# Project\nProject {{args}}",
        )
        .unwrap();
        let catalog = PromptCatalog::load(&paths, &project, &[], &[]).unwrap();
        assert_eq!(
            catalog.expand("review", "the diff").unwrap(),
            "# Project\nProject the diff"
        );
    }

    #[test]
    fn plugin_paths_must_not_traverse() {
        let manifest = PluginManifest {
            name: "demo".into(),
            version: "1".into(),
            skills: vec!["../escape".into()],
            hooks: Vec::new(),
            mcp: Vec::new(),
            roles: Vec::new(),
            prompts: Vec::new(),
        };
        assert!(manifest.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn plugin_copy_rejects_symlinked_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside.toml");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("vera-plugin.toml"),
            "name = \"demo\"\nversion = \"1\"\n",
        )
        .unwrap();
        fs::write(&outside, "secret").unwrap();
        symlink(&outside, source.join("secret.txt")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let manager = PluginManager::new(paths);
        assert!(manager.add_local(&source).is_err());
    }

    #[test]
    fn mcp_mutation_tools_are_not_read_only() {
        let tool = McpToolDescription {
            name: "write_file".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object"}),
            annotations: serde_json::json!({"readOnlyHint":false}),
        };
        assert!(!mcp_tool_is_read_only(&tool));
    }

    #[tokio::test]
    async fn fixture_mcp_negotiates_lists_and_calls_tools() {
        let temp = tempfile::tempdir().unwrap();
        let command = r#"while IFS= read -r line; do
  case "$line" in
    *initialize*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" ;;
    *tools/list*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"read_fixture","description":"fixture","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}},{"name":"bad","inputSchema":{"type":"array"}}]}}\n' "$id" ;;
    *tools/call*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); (sleep 0.02; printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"fixture result %s"}]}}\n' "$id" "$id") & ;;
  esac
done
"#;
        let spec = McpSpec {
            name: "fixture".into(),
            command: "/bin/sh".into(),
            args: vec!["-c".into(), command.into()],
            timeout_ms: 2_000,
            allowed_tools: Vec::new(),
            network: false,
        };
        let client = McpClient::for_fixture(spec, temp.path().to_path_buf());
        let tools = match client.tools().await {
            Ok(tools) => tools,
            Err(error) => panic!(
                "MCP fixture failed: {error}; stderr={}",
                client.stderr().await
            ),
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_fixture");
        assert_eq!(client.server_info().await["name"], "fixture");
        let (first, second) = tokio::join!(
            client.call("read_fixture", serde_json::json!({})),
            client.call("read_fixture", serde_json::json!({}))
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first["content"][0]["text"], second["content"][0]["text"]);
        client.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_mcp_lines_are_discarded_without_poisoning_the_reader() {
        let mut bytes = vec![b'x'; MAX_MCP_MESSAGE_BYTES + 1];
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"jsonrpc":"2.0","id":7}"#);
        bytes.push(b'\n');
        let mut reader = BufReader::new(bytes.as_slice());

        let oversized = read_bounded_mcp_line(&mut reader).await.unwrap().unwrap();
        assert!(serde_json::from_slice::<Value>(&oversized).is_err());
        let valid = read_bounded_mcp_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&valid).unwrap()["id"], 7);
    }

    #[tokio::test]
    async fn failed_mcp_initialization_cleans_up_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("cleaned");
        let script = r#"
marker="$1"
trap 'printf cleaned > "$marker"' EXIT
while IFS= read -r line; do
  case "$line" in
    *initialize*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{}}}\n' "$id" ;;
  esac
done
"#;
        let spec = McpSpec {
            name: "bad-fixture".into(),
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                marker.display().to_string(),
            ],
            timeout_ms: 2_000,
            allowed_tools: Vec::new(),
            network: false,
        };
        let client = McpClient::for_fixture(spec, temp.path().to_path_buf());
        assert!(client.tools().await.is_err());
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn malformed_tools_list_cleans_up_the_child() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("cleaned");
        let script = r#"marker="$1"; trap 'printf cleaned > "$marker"' EXIT; while IFS= read -r line; do case "$line" in *initialize*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}}\n' "$id" ;; *tools/list*) id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'); printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":"not-an-array"}}\n' "$id" ;; esac; done"#;
        let spec = McpSpec {
            name: "malformed-list".into(),
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                script.into(),
                "sh".into(),
                marker.display().to_string(),
            ],
            timeout_ms: 2_000,
            allowed_tools: Vec::new(),
            network: false,
        };
        let client = McpClient::for_fixture(spec, temp.path().to_path_buf());
        assert!(client.tools().await.is_err());
        assert!(marker.exists());
    }
}
