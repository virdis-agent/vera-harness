use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::VeraPaths;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub approval: ApprovalConfig,
    pub shell_timeout_seconds: u64,
    pub context_window_tokens: usize,
    pub hooks: Vec<String>,
    pub trusted_plugins: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    pub auto_read: bool,
    pub writes: String,
    pub shell: String,
    pub network: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "openai-codex".into(),
            model: "gpt-5.6".into(),
            approval: ApprovalConfig::default(),
            shell_timeout_seconds: 120,
            context_window_tokens: 128_000,
            hooks: Vec::new(),
            trusted_plugins: Vec::new(),
        }
    }
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            auto_read: true,
            writes: "once".into(),
            shell: "once".into(),
            network: "once".into(),
        }
    }
}

impl Config {
    pub fn load(paths: &VeraPaths, project: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(global) = read_toml(&paths.root.join("config.toml"))? {
            config.merge_global(global)?;
        }
        let project_config = project.join(".vera/config.toml");
        if let Some(local) = read_toml(&project_config)? {
            config.merge_project(local)?;
        }
        Ok(config)
    }

    pub fn save_global(&self, paths: &VeraPaths) -> Result<()> {
        paths.ensure_runtime_dirs()?;
        let contents = toml::to_string_pretty(self)?;
        fs::write(paths.root.join("config.toml"), contents)?;
        Ok(())
    }

    fn merge_global(&mut self, incoming: Self) -> Result<()> {
        *self = incoming;
        Ok(())
    }

    fn merge_project(&mut self, incoming: Self) -> Result<()> {
        // Project files can select behavior, but cannot change credential endpoints or
        // silently weaken the global approval defaults.
        if incoming.provider != Self::default().provider {
            self.provider = incoming.provider;
        }
        if incoming.model != Self::default().model {
            self.model = incoming.model;
        }
        if incoming.shell_timeout_seconds != Self::default().shell_timeout_seconds {
            self.shell_timeout_seconds = incoming.shell_timeout_seconds;
        }
        if incoming.context_window_tokens != Self::default().context_window_tokens {
            self.context_window_tokens = incoming.context_window_tokens;
        }
        if incoming.approval.auto_read {
            self.approval.auto_read = true;
        }
        if incoming.approval.writes == "always" {
            self.approval.writes = "always".into();
        }
        if incoming.approval.shell == "always" {
            self.approval.shell = "always".into();
        }
        if incoming.approval.network == "always" {
            self.approval.network = "always".into();
        }
        self.hooks.extend(incoming.hooks);
        self.trusted_plugins.extend(incoming.trusted_plugins);
        Ok(())
    }
}

fn read_toml(path: &PathBuf) -> Result<Option<Config>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
    ))
}
