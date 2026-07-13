use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::paths::VeraPaths;
use crate::safety::{PathGuard, PermissionEffect, PermissionPolicy, PermissionRule};
use crate::settings::{ApprovalOverrides, ConfigOverrides, GlobalState};

pub const CURRENT_CONFIG_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SettingSource {
    Defaults,
    Toml,
    GlobalState,
    Project,
}

impl SettingSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Defaults => "defaults",
            Self::Toml => "config.toml",
            Self::GlobalState => "global state",
            Self::Project => "project configuration",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigLayers {
    pub config: Config,
    pub global_state: GlobalState,
    pub sources: BTreeMap<String, SettingSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub version: u32,
    pub provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub approval: ApprovalConfig,
    pub shell_timeout_seconds: u64,
    pub context_window_tokens: usize,
    pub hooks: Vec<String>,
    pub trusted_plugins: Vec<String>,
    /// Plugins enabled by default for new sessions. Interactive changes are
    /// stored as partial global-state overrides.
    pub enabled_plugins: Vec<String>,
    /// MCP server names enabled by default for new sessions.
    pub enabled_mcp: Vec<String>,
    /// Optional allow-list for skills. An empty list means all discovered
    /// skills are available.
    pub allowed_skills: Vec<String>,
    pub role: Option<String>,
    /// Additional prompt roots. Relative roots are resolved from the project
    /// for project config and from ~/.vera for global config.
    pub prompt_roots: Vec<String>,
    /// Explicit CDP HTTP endpoints that may be used without adding a new
    /// endpoint to project configuration.
    pub browser_cdp_endpoints: Vec<String>,
    #[serde(default)]
    pub permission_rules: Vec<PermissionRule>,
}

fn default_config_version() -> u32 {
    CURRENT_CONFIG_VERSION
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
            version: CURRENT_CONFIG_VERSION,
            provider: "openai-codex".into(),
            model: String::new(),
            effort: None,
            approval: ApprovalConfig::default(),
            shell_timeout_seconds: 120,
            context_window_tokens: 128_000,
            hooks: Vec::new(),
            trusted_plugins: Vec::new(),
            enabled_plugins: Vec::new(),
            enabled_mcp: Vec::new(),
            allowed_skills: Vec::new(),
            role: None,
            prompt_roots: Vec::new(),
            browser_cdp_endpoints: Vec::new(),
            permission_rules: Vec::new(),
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
    pub fn load_global(paths: &VeraPaths) -> Result<Self> {
        let state = GlobalState::load(paths)?;
        Self::load_with_state(paths, &paths.root, &state)
    }

    pub fn load(paths: &VeraPaths, project: &Path) -> Result<Self> {
        Ok(Self::load_layers(paths, project)?.config)
    }

    pub fn load_with_state(paths: &VeraPaths, project: &Path, state: &GlobalState) -> Result<Self> {
        let mut config = Self::default();
        if let Some(global) = read_toml(&paths.config_file)? {
            config
                .apply_toml(&global, &paths.root, false)
                .map_err(|error| {
                    annotate_validation_error(&paths.config_file, "configuration file", error)
                })?;
        }
        config
            .apply_overrides(&state.config, &paths.root)
            .map_err(|error| {
                annotate_validation_error(&paths.global_state, "global state", error)
            })?;
        let project_config = project.join(".vera/config.toml");
        if let Some(local) = read_toml(&project_config)? {
            config.apply_toml(&local, project, true).map_err(|error| {
                annotate_validation_error(&project_config, "project configuration file", error)
            })?;
        }
        config.validate().map_err(|error| {
            anyhow::anyhow!(
                "validate merged configuration ({} and {}): {error:#}",
                paths.config_file.display(),
                paths.global_state.display()
            )
        })?;
        config.version = CURRENT_CONFIG_VERSION;
        Ok(config)
    }

    pub fn load_layers(paths: &VeraPaths, project: &Path) -> Result<ConfigLayers> {
        let state = GlobalState::load(paths)?;
        let global = read_toml(&paths.config_file)?;
        let project_path = project.join(".vera/config.toml");
        let local = read_toml(&project_path)?;
        let mut config = Self::default();
        let mut sources = BTreeMap::new();
        if let Some(value) = &global {
            config
                .apply_toml(value, &paths.root, false)
                .map_err(|error| {
                    annotate_validation_error(&paths.config_file, "configuration file", error)
                })?;
            mark_toml_sources(value, SettingSource::Toml, &mut sources);
        }
        config
            .apply_overrides(&state.config, &paths.root)
            .map_err(|error| {
                annotate_validation_error(&paths.global_state, "global state", error)
            })?;
        mark_state_sources(&state, &mut sources);
        if let Some(value) = &local {
            let before_project = config.clone();
            let before_sources = sources.clone();
            config.apply_toml(value, project, true).map_err(|error| {
                annotate_validation_error(&project_path, "project configuration file", error)
            })?;
            mark_toml_sources(value, SettingSource::Project, &mut sources);
            if let Some(approval) = value.get("approval").and_then(toml::Value::as_table) {
                if approval.get("auto_read").and_then(toml::Value::as_bool) == Some(true)
                    && !before_project.approval.auto_read
                {
                    restore_source(&mut sources, &before_sources, "approval.auto_read");
                }
                for key in ["writes", "shell", "network"] {
                    if approval.get(key).and_then(toml::Value::as_str) == Some("once")
                        && match key {
                            "writes" => before_project.approval.writes == "always",
                            "shell" => before_project.approval.shell == "always",
                            "network" => before_project.approval.network == "always",
                            _ => false,
                        }
                    {
                        restore_source(&mut sources, &before_sources, &format!("approval.{key}"));
                    }
                }
            }
            if config.permission_rules == before_project.permission_rules {
                restore_source(&mut sources, &before_sources, "permission_rules");
            }
        }
        config.validate().map_err(|error| {
            anyhow::anyhow!(
                "validate merged configuration ({} and {}): {error:#}",
                paths.config_file.display(),
                paths.global_state.display()
            )
        })?;
        config.version = CURRENT_CONFIG_VERSION;
        Ok(ConfigLayers {
            config,
            global_state: state,
            sources,
        })
    }

    pub fn save_global(&self, paths: &VeraPaths) -> Result<()> {
        paths.ensure_runtime_dirs()?;
        let mut current = self.clone();
        current.version = CURRENT_CONFIG_VERSION;
        let contents = toml::to_string_pretty(&current)?;
        let guard = PathGuard::new(paths.root.clone())?;
        let target = guard.resolve(&paths.config_file)?;
        let temporary = target.with_file_name(format!(
            ".config.vera-tmp-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(temporary, &target)?;
        crate::paths::set_private_file(&target)?;
        Ok(())
    }

    fn apply_toml(&mut self, value: &toml::Value, path_base: &Path, project: bool) -> Result<()> {
        let table = value
            .as_table()
            .context("configuration root must be a TOML table")?;
        const KEYS: &[&str] = &[
            "version",
            "provider",
            "model",
            "effort",
            "approval",
            "shell_timeout_seconds",
            "context_window_tokens",
            "hooks",
            "trusted_plugins",
            "enabled_plugins",
            "enabled_mcp",
            "allowed_skills",
            "role",
            "prompt_roots",
            "browser_cdp_endpoints",
            "permission_rules",
        ];
        for key in table.keys() {
            if !KEYS.contains(&key.as_str()) {
                anyhow::bail!("unknown configuration key {key:?}");
            }
        }
        if let Some(version_value) = table.get("version") {
            let version = version_value
                .as_integer()
                .context("configuration version must be an integer")?;
            if version < 1 || version as u32 > CURRENT_CONFIG_VERSION {
                anyhow::bail!("unsupported configuration version {version}");
            }
        }
        if let Some(value) = table.get("provider") {
            self.provider = value.as_str().context("provider must be a string")?.into();
        }
        if let Some(value) = table.get("model") {
            let value = value.as_str().context("model must be a string")?;
            // `auto` was the legacy spelling for selecting the provider's
            // catalog default. Keep old configuration readable without
            // exposing it as a model option going forward.
            self.model = if value == "auto" {
                String::new()
            } else {
                value.into()
            };
        }
        if let Some(value) = table.get("effort") {
            let value = value.as_str().context("effort must be a string")?;
            self.effort = Some(value.into());
        }
        if let Some(value) = table.get("shell_timeout_seconds") {
            let value = value
                .as_integer()
                .context("shell_timeout_seconds must be an integer")?;
            if value <= 0 {
                anyhow::bail!("shell_timeout_seconds must be greater than zero");
            }
            self.shell_timeout_seconds = value as u64;
        }
        if let Some(value) = table.get("context_window_tokens") {
            let value = value
                .as_integer()
                .context("context_window_tokens must be an integer")?;
            if value <= 0 {
                anyhow::bail!("context_window_tokens must be greater than zero");
            }
            self.context_window_tokens = value as usize;
        }
        apply_strings(table, "hooks", &mut self.hooks, false)?;
        apply_strings(table, "trusted_plugins", &mut self.trusted_plugins, false)?;
        // These are selections, so a project value replaces the global value.
        apply_strings(table, "enabled_plugins", &mut self.enabled_plugins, true)?;
        apply_strings(table, "enabled_mcp", &mut self.enabled_mcp, true)?;
        apply_strings(table, "allowed_skills", &mut self.allowed_skills, true)?;
        apply_strings(table, "prompt_roots", &mut self.prompt_roots, true)?;
        apply_strings(
            table,
            "browser_cdp_endpoints",
            &mut self.browser_cdp_endpoints,
            true,
        )?;
        if let Some(value) = table.get("permission_rules") {
            let mut parsed: Vec<PermissionRule> = value
                .clone()
                .try_into()
                .context("permission_rules must be an array of rules")?;
            for rule in &mut parsed {
                normalize_permission_rule(rule, path_base)?;
            }
            if project {
                // Project configuration can strengthen the effective policy,
                // but an allow rule here must not weaken a global denial.
                self.permission_rules.extend(
                    parsed
                        .into_iter()
                        .filter(|rule| rule.effect != PermissionEffect::Allow),
                );
            } else {
                self.permission_rules.extend(parsed);
            }
        }
        if let Some(role) = table.get("role") {
            let role = role.as_str().context("role must be a string")?;
            self.role = Some(role.into());
        }

        if let Some(approval_value) = table.get("approval") {
            let approval = approval_value
                .as_table()
                .context("approval must be a TOML table")?;
            for key in approval.keys() {
                if !["auto_read", "writes", "shell", "network"].contains(&key.as_str()) {
                    anyhow::bail!("unknown approval configuration key {key:?}");
                }
            }
            if !project {
                if let Some(value) = approval.get("auto_read") {
                    self.approval.auto_read = value
                        .as_bool()
                        .context("approval.auto_read must be a boolean")?;
                }
                for key in ["writes", "shell", "network"] {
                    if let Some(value) = approval.get(key) {
                        let value = value
                            .as_str()
                            .with_context(|| format!("approval.{key} must be once or always"))?;
                        match key {
                            "writes" => self.approval.writes = value.into(),
                            "shell" => self.approval.shell = value.into(),
                            "network" => self.approval.network = value.into(),
                            _ => unreachable!(),
                        }
                    }
                }
            } else {
                // Project files may strengthen approval defaults, never weaken
                // a stricter global default.
                if approval
                    .get("auto_read")
                    .map(|value| {
                        value
                            .as_bool()
                            .context("approval.auto_read must be a boolean")
                    })
                    .transpose()?
                    == Some(false)
                {
                    self.approval.auto_read = false;
                }
                for key in ["writes", "shell", "network"] {
                    if approval
                        .get(key)
                        .map(|value| {
                            value
                                .as_str()
                                .context(format!("approval.{key} must be once or always"))
                        })
                        .transpose()?
                        == Some("always")
                    {
                        match key {
                            "writes" => self.approval.writes = "always".into(),
                            "shell" => self.approval.shell = "always".into(),
                            "network" => self.approval.network = "always".into(),
                            _ => unreachable!(),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn apply_overrides(&mut self, overrides: &ConfigOverrides, path_base: &Path) -> Result<()> {
        if let Some(provider) = &overrides.provider {
            self.provider = provider.clone();
        }
        if let Some(model) = &overrides.model {
            self.model = if model == "auto" {
                String::new()
            } else {
                model.clone()
            };
        }
        if let Some(effort) = &overrides.effort {
            self.effort = Some(effort.clone());
        }
        if let Some(approval) = &overrides.approval {
            apply_approval_overrides(&mut self.approval, approval)?;
        }
        if let Some(value) = overrides.shell_timeout_seconds {
            self.shell_timeout_seconds = value;
        }
        if let Some(value) = overrides.context_window_tokens {
            self.context_window_tokens = value;
        }
        if let Some(values) = &overrides.hooks {
            self.hooks = values.clone();
        }
        if let Some(values) = &overrides.trusted_plugins {
            self.trusted_plugins = values.clone();
        }
        if let Some(values) = &overrides.enabled_plugins {
            self.enabled_plugins = values.clone();
        }
        if let Some(values) = &overrides.enabled_mcp {
            self.enabled_mcp = values.clone();
        }
        if let Some(values) = &overrides.allowed_skills {
            self.allowed_skills = values.clone();
        }
        if let Some(role) = &overrides.role {
            self.role = Some(role.clone());
        }
        if let Some(values) = &overrides.prompt_roots {
            self.prompt_roots = values.clone();
        }
        if let Some(values) = &overrides.browser_cdp_endpoints {
            self.browser_cdp_endpoints = values.clone();
        }
        if let Some(rules) = &overrides.permission_rules {
            self.permission_rules = rules.clone();
            for rule in &mut self.permission_rules {
                normalize_permission_rule(rule, path_base)?;
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version == 0 || self.version > CURRENT_CONFIG_VERSION {
            anyhow::bail!("unsupported configuration version {}", self.version);
        }
        if self.provider.trim().is_empty() {
            anyhow::bail!("provider must not be empty");
        }
        for (field, values) in [
            ("hooks", &self.hooks),
            ("trusted_plugins", &self.trusted_plugins),
            ("enabled_plugins", &self.enabled_plugins),
            ("enabled_mcp", &self.enabled_mcp),
            ("allowed_skills", &self.allowed_skills),
            ("prompt_roots", &self.prompt_roots),
            ("browser_cdp_endpoints", &self.browser_cdp_endpoints),
        ] {
            let mut seen = std::collections::BTreeSet::new();
            for value in values {
                if value.trim().is_empty() || !seen.insert(value) {
                    anyhow::bail!("{field} contains an empty or duplicate entry: {value:?}");
                }
            }
        }
        for endpoint in &self.browser_cdp_endpoints {
            let parsed = Url::parse(endpoint)
                .with_context(|| format!("invalid browser_cdp_endpoints entry {endpoint:?}"))?;
            if parsed.scheme() != "http" || parsed.host_str().is_none() {
                anyhow::bail!(
                    "browser_cdp_endpoints entries must be explicit http:// endpoints: {endpoint:?}"
                );
            }
        }
        if self.context_window_tokens == 0 {
            anyhow::bail!("context_window_tokens must be greater than zero");
        }
        if self.shell_timeout_seconds == 0 {
            anyhow::bail!("shell_timeout_seconds must be greater than zero");
        }
        for (field, value) in [
            ("approval.writes", self.approval.writes.as_str()),
            ("approval.shell", self.approval.shell.as_str()),
            ("approval.network", self.approval.network.as_str()),
        ] {
            if !matches!(value, "once" | "always") {
                anyhow::bail!("{field} must be once or always");
            }
        }
        Ok(())
    }

    pub fn permission_policy(&self) -> PermissionPolicy {
        let mut policy = PermissionPolicy::default();
        policy.set_auto_read(self.approval.auto_read);
        policy.set_always_ask(
            crate::safety::PermissionKind::Write,
            self.approval.writes == "always",
        );
        policy.set_always_ask(
            crate::safety::PermissionKind::Shell,
            self.approval.shell == "always",
        );
        policy.set_always_ask(
            crate::safety::PermissionKind::Network,
            self.approval.network == "always",
        );
        for rule in &self.permission_rules {
            policy.add_global_rule(rule.clone());
        }
        // An endpoint listed in configuration is an explicit browser
        // approval. The BrowserManager still requires an exact endpoint
        // match, and any configured deny rule remains higher precedence.
        for endpoint in &self.browser_cdp_endpoints {
            if let Ok(url) = Url::parse(endpoint)
                && let Some(host) = url.host_str()
            {
                policy.add_global_rule(PermissionRule {
                    effect: PermissionEffect::Allow,
                    matcher: crate::safety::PermissionMatcher {
                        permission_kind: Some(crate::safety::PermissionKind::Browser),
                        browser_action: Some("connect".into()),
                        network_host: Some(host.to_owned()),
                        ..crate::safety::PermissionMatcher::default()
                    },
                });
            }
        }
        policy
    }
}

fn restore_source(
    sources: &mut BTreeMap<String, SettingSource>,
    before: &BTreeMap<String, SettingSource>,
    key: &str,
) {
    if let Some(source) = before.get(key) {
        sources.insert(key.to_owned(), *source);
    } else {
        sources.remove(key);
    }
}

fn annotate_validation_error(path: &Path, kind: &str, error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("validate {kind} {}: {error:#}", path.display())
}

/// Permission actions carry canonical paths, while configuration is commonly
/// authored with relative paths. Resolve those paths against the config's
/// scope and canonicalize the existing portion so symlinked directories still
/// match the action signature produced by PathGuard.
fn normalize_permission_rule(rule: &mut PermissionRule, base: &Path) -> Result<()> {
    let Some(path) = rule.matcher.canonical_path.as_ref() else {
        return Ok(());
    };
    let candidate = if path.is_absolute() {
        path.clone()
    } else {
        base.join(path)
    };
    rule.matcher.canonical_path = Some(canonicalize_existing_prefix(&candidate)?);
    Ok(())
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path.to_path_buf();
    while !current.exists() {
        let name = current
            .file_name()
            .context("permission canonical_path has no filename")?
            .to_owned();
        missing.push(name);
        current = current
            .parent()
            .context("permission canonical_path has no existing parent")?
            .to_path_buf();
    }
    let mut normalized = fs::canonicalize(current)?;
    for component in missing.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

fn apply_strings(
    table: &toml::map::Map<String, toml::Value>,
    key: &str,
    target: &mut Vec<String>,
    replace: bool,
) -> Result<()> {
    let Some(values) = table.get(key) else {
        return Ok(());
    };
    let values = values
        .as_array()
        .context(format!("{key} must be an array of strings"))?;
    let parsed = values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context(format!("{key} must be an array of strings"))
        })
        .collect::<Result<Vec<_>>>()?;
    if replace {
        *target = parsed;
    } else {
        for value in parsed {
            if !target.contains(&value) {
                target.push(value);
            }
        }
    }
    Ok(())
}

fn apply_approval_overrides(
    target: &mut ApprovalConfig,
    overrides: &ApprovalOverrides,
) -> Result<()> {
    if let Some(value) = overrides.auto_read {
        target.auto_read = value;
    }
    if let Some(value) = &overrides.writes {
        target.writes = value.clone();
    }
    if let Some(value) = &overrides.shell {
        target.shell = value.clone();
    }
    if let Some(value) = &overrides.network {
        target.network = value.clone();
    }
    Ok(())
}

fn mark_toml_sources(
    value: &toml::Value,
    source: SettingSource,
    sources: &mut BTreeMap<String, SettingSource>,
) {
    let Some(table) = value.as_table() else {
        return;
    };
    for key in table.keys() {
        if key == "approval" {
            if let Some(approval) = table.get(key).and_then(toml::Value::as_table) {
                for nested in approval.keys() {
                    sources.insert(format!("approval.{nested}"), source);
                }
            }
        } else {
            sources.insert(key.clone(), source);
        }
    }
}

fn mark_state_sources(state: &GlobalState, sources: &mut BTreeMap<String, SettingSource>) {
    let overrides = &state.config;
    if overrides.provider.is_some() {
        sources.insert("provider".into(), SettingSource::GlobalState);
    }
    if overrides.model.is_some() {
        sources.insert("model".into(), SettingSource::GlobalState);
    }
    if overrides.effort.is_some() {
        sources.insert("effort".into(), SettingSource::GlobalState);
    }
    if let Some(approval) = &overrides.approval {
        if approval.auto_read.is_some() {
            sources.insert("approval.auto_read".into(), SettingSource::GlobalState);
        }
        if approval.writes.is_some() {
            sources.insert("approval.writes".into(), SettingSource::GlobalState);
        }
        if approval.shell.is_some() {
            sources.insert("approval.shell".into(), SettingSource::GlobalState);
        }
        if approval.network.is_some() {
            sources.insert("approval.network".into(), SettingSource::GlobalState);
        }
    }
    for key in [
        (
            "shell_timeout_seconds",
            overrides.shell_timeout_seconds.is_some(),
        ),
        (
            "context_window_tokens",
            overrides.context_window_tokens.is_some(),
        ),
        ("hooks", overrides.hooks.is_some()),
        ("trusted_plugins", overrides.trusted_plugins.is_some()),
        ("enabled_plugins", overrides.enabled_plugins.is_some()),
        ("enabled_mcp", overrides.enabled_mcp.is_some()),
        ("allowed_skills", overrides.allowed_skills.is_some()),
        ("role", overrides.role.is_some()),
        ("prompt_roots", overrides.prompt_roots.is_some()),
        (
            "browser_cdp_endpoints",
            overrides.browser_cdp_endpoints.is_some(),
        ),
        ("permission_rules", overrides.permission_rules.is_some()),
        ("display_mode", state.display_mode.is_some()),
        ("permission_mode", state.permission_mode.is_some()),
        ("loaded_skills", state.loaded_skills.is_some()),
    ] {
        if key.1 {
            sources.insert(key.0.into(), SettingSource::GlobalState);
        }
    }
}

fn read_toml(path: &Path) -> Result<Option<toml::Value>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!(
            "configuration file {} is not a regular file",
            path.display()
        );
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(toml::from_str(&text).map_err(|error| {
        anyhow::anyhow!("parse {}: {error}", path.display())
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_capability_defaults_replace_global_values() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".vera")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "enabled_plugins = [\"global\"]\nprompt_roots = [\"global-prompts\"]\n",
        )
        .unwrap();
        fs::write(
            project.join(".vera/config.toml"),
            "enabled_plugins = [\"project\"]\nprompt_roots = [\"project-prompts\"]\n",
        )
        .unwrap();
        let config = Config::load(&paths, &project).unwrap();
        assert_eq!(config.enabled_plugins, vec!["project"]);
        assert_eq!(config.prompt_roots, vec!["project-prompts"]);
    }

    #[test]
    fn global_state_overrides_global_toml_but_project_config_wins() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".vera")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            &paths.config_file,
            "provider = \"global-toml\"\nmodel = \"global-model\"\n",
        )
        .unwrap();
        fs::write(
            &paths.global_state,
            r#"{
  "version": 1,
  "config": {"provider":"global-state", "model":"state-model"}
}
"#,
        )
        .unwrap();
        fs::write(
            project.join(".vera/config.toml"),
            "provider = \"project\"\n",
        )
        .unwrap();
        let layers = Config::load_layers(&paths, &project).unwrap();
        assert_eq!(layers.config.provider, "project");
        assert_eq!(layers.config.model, "state-model");
        assert_eq!(layers.sources["provider"], SettingSource::Project);
        assert_eq!(layers.sources["model"], SettingSource::GlobalState);
    }

    #[test]
    fn malformed_global_state_reports_file_and_json_location() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(&paths.global_state, "{\n  \"version\": 1,\n").unwrap();
        let error = Config::load(&paths, temp.path()).unwrap_err().to_string();
        assert!(error.contains(paths.global_state.to_string_lossy().as_ref()));
        assert!(error.contains("line"));
    }

    #[test]
    fn invalid_toml_type_reports_the_configuration_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(&paths.config_file, "shell_timeout_seconds = \"slow\"\n").unwrap();
        let error = Config::load(&paths, temp.path()).unwrap_err().to_string();
        assert!(error.contains(paths.config_file.to_string_lossy().as_ref()));
        assert!(error.contains("shell_timeout_seconds"));
    }

    #[test]
    fn legacy_configuration_is_accepted_and_normalized_on_save() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "version = 1\nprovider = \"xai\"\nmodel = \"auto\"\n",
        )
        .unwrap();
        let config = Config::load(&paths, &project).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert!(config.model.is_empty());
        config.save_global(&paths).unwrap();
        let saved = fs::read_to_string(paths.root.join("config.toml")).unwrap();
        assert!(saved.contains("version = 2"));
        assert!(!saved.contains("model ="));
    }

    #[test]
    fn project_approval_cannot_weaken_global_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".vera")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "[approval]\nauto_read = false\nwrites = \"always\"\n",
        )
        .unwrap();
        fs::write(
            project.join(".vera/config.toml"),
            "[approval]\nauto_read = true\nwrites = \"once\"\n",
        )
        .unwrap();

        let config = Config::load(&paths, &project).unwrap();
        let policy = config.permission_policy();
        assert!(!policy.check(crate::safety::PermissionKind::Read).unwrap());
        assert!(!policy.check(crate::safety::PermissionKind::Write).unwrap());
    }

    #[test]
    fn project_allow_rules_cannot_weaken_global_denials() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".vera")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "permission_rules = [{ effect = \"deny\", matcher = { permission_kind = \"write\" } }]\n",
        )
        .unwrap();
        fs::write(
            project.join(".vera/config.toml"),
            "permission_rules = [{ effect = \"allow\", matcher = { permission_kind = \"write\" } }]\n",
        )
        .unwrap();
        let policy = Config::load(&paths, &project).unwrap().permission_policy();
        assert!(!policy.check(crate::safety::PermissionKind::Write).unwrap());
    }

    #[test]
    fn permission_paths_are_resolved_from_project_and_existing_symlinks() {
        #[cfg(unix)]
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let real = project.join("real");
        fs::create_dir_all(project.join(".vera")).unwrap();
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("file.txt"), "fixture").unwrap();
        #[cfg(unix)]
        symlink("real", project.join("alias")).unwrap();

        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            project.join(".vera/config.toml"),
            "permission_rules = [{ effect = \"deny\", matcher = { permission_kind = \"write\", canonical_path = \"alias/file.txt\" } }]\n",
        )
        .unwrap();

        let config = Config::load(&paths, &project).unwrap();
        let policy = config.permission_policy();
        let guarded_path = crate::safety::PathGuard::new(project.clone())
            .unwrap()
            .resolve(Path::new("alias/file.txt"))
            .unwrap();
        assert!(!policy.check_action(&crate::safety::ActionSignature {
            permission_kind: crate::safety::PermissionKind::Write,
            canonical_path: Some(guarded_path),
            ..crate::safety::ActionSignature::default()
        }));
    }

    #[test]
    fn configured_browser_endpoint_is_explicitly_allowed_but_global_deny_wins() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(project.join(".vera")).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "browser_cdp_endpoints = [\"http://localhost:9222\"]\n",
        )
        .unwrap();
        let action = crate::safety::ActionSignature {
            permission_kind: crate::safety::PermissionKind::Browser,
            browser_action: Some("connect".into()),
            network_host: Some("LOCALHOST".into()),
            ..crate::safety::ActionSignature::default()
        };
        let config = Config::load(&paths, &project).unwrap();
        assert!(config.permission_policy().check_action(&action));

        fs::write(
            paths.root.join("config.toml"),
            "browser_cdp_endpoints = [\"http://localhost:9222\"]\npermission_rules = [{ effect = \"deny\", matcher = { permission_kind = \"browser\", browser_action = \"connect\" } }]\n",
        )
        .unwrap();
        let config = Config::load(&paths, &project).unwrap();
        assert!(!config.permission_policy().check_action(&action));
    }
}
