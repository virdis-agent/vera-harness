use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{Duration, timeout};

use crate::auth::redact;
use crate::paths::VeraPaths;
use crate::safety::{ApprovalHandler, PermissionKind, PermissionPolicy};

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
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
}

impl SkillCatalog {
    pub fn load(paths: &VeraPaths, project: &Path) -> Result<Self> {
        let mut catalog = Self::default();
        let roots = [paths.skills.clone(), project.join(".vera/skills")];
        for root in roots {
            if !root.exists() {
                continue;
            }
            for entry in fs::read_dir(root)? {
                let path = entry?.path();
                let skill_file = if path.is_dir() {
                    path.join("SKILL.md")
                } else {
                    path
                };
                if skill_file
                    .file_name()
                    .is_some_and(|name| name == "SKILL.md")
                    && let Ok(skill) = parse_skill(&skill_file)
                {
                    catalog.skills.insert(skill.name.clone(), skill);
                }
            }
        }
        Ok(catalog)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.skills.keys()
    }
    pub fn activate(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
    pub fn active_descriptions(&self) -> Vec<(String, String)> {
        self.skills
            .values()
            .map(|skill| (skill.name.clone(), skill.description.clone()))
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
    Ok(Skill {
        name,
        description,
        path: path.to_path_buf(),
    })
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
    pub timeout_ms: u64,
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
        policy
            .authorize(
                PermissionKind::Hook,
                &format!("run hook {}", spec.name),
                approval,
                None,
            )
            .await?;
        let payload = serde_json::to_vec(&event)?;
        let mut command = Command::new("/bin/zsh");
        command.args(["-lc", &spec.command]).env_clear();
        for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
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
            anyhow::bail!(
                "hook failed: {}",
                redact(&String::from_utf8_lossy(&output.stderr))
            );
        }
        Ok(redact(&String::from_utf8_lossy(&output.stdout)))
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpSpec {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
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
        for entry in fs::read_dir(&self.paths.plugins)? {
            let manifest = entry?.path().join("vera-plugin.toml");
            if manifest.exists()
                && let Ok(value) = toml::from_str(&fs::read_to_string(manifest)?)
            {
                result.push(value);
            }
        }
        Ok(result)
    }

    pub fn add_local(&self, source: &Path) -> Result<PluginManifest> {
        let manifest_path = source.join("vera-plugin.toml");
        let manifest: PluginManifest = toml::from_str(&fs::read_to_string(&manifest_path)?)?;
        // The caller owns the async approval boundary for CLI operations; the explicit
        // add path also rejects scripts because manifests are data-only in v1.
        if manifest.name.contains('/') || manifest.name.contains("..") {
            anyhow::bail!("invalid plugin name");
        }
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
        if from.is_dir() {
            copy_tree(&from, &to)?;
        } else if from.is_file() {
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
        Ok(PluginManager::new(self.paths.clone())
            .list()?
            .into_iter()
            .flat_map(|manifest| manifest.mcp)
            .collect())
    }
    pub async fn test(
        &self,
        name: &str,
        policy: &mut PermissionPolicy,
        approval: &mut dyn ApprovalHandler,
    ) -> Result<String> {
        let spec = self
            .list()?
            .into_iter()
            .find(|spec| spec.name == name)
            .context("MCP server not found")?;
        policy
            .authorize(
                PermissionKind::Mcp,
                &format!("start MCP server {}", spec.name),
                approval,
                None,
            )
            .await?;
        let mut command = Command::new(&spec.command);
        command.args(&spec.args).env_clear();
        for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let request = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"vera","version":"0.1.0-alpha.3"}}});
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(format!("{}\n", request).as_bytes()).await?;
        }
        let stdout = child.stdout.take().context("MCP stdout unavailable")?;
        let mut lines = BufReader::new(stdout).lines();
        let line = timeout(Duration::from_secs(5), lines.next_line())
            .await
            .context("MCP timeout")??
            .context("MCP returned no response")?;
        let _ = child.kill().await;
        Ok(redact(&line))
    }
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
