use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::paths::VeraPaths;
use crate::safety::{PathGuard, PermissionEffect, PermissionPolicy, PermissionRule};

pub const CURRENT_CONFIG_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub version: u32,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub approval: ApprovalConfig,
    pub shell_timeout_seconds: u64,
    pub context_window_tokens: usize,
    pub hooks: Vec<String>,
    pub trusted_plugins: Vec<String>,
    /// Plugins enabled by default for new sessions. Interactive changes do not
    /// write this list back to disk.
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
            model: "auto".into(),
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
        let mut config = Self::default();
        if let Some(global) = read_toml(&paths.root.join("config.toml"))? {
            config.apply_toml(&global, &paths.root, false)?;
        }
        config.validate()?;
        config.version = CURRENT_CONFIG_VERSION;
        Ok(config)
    }

    pub fn load(paths: &VeraPaths, project: &Path) -> Result<Self> {
        let mut config = Self::default();
        if let Some(global) = read_toml(&paths.root.join("config.toml"))? {
            config.apply_toml(&global, &paths.root, false)?;
        }
        let project_config = project.join(".vera/config.toml");
        if let Some(local) = read_toml(&project_config)? {
            config.apply_toml(&local, project, true)?;
        }
        config.validate()?;
        config.version = CURRENT_CONFIG_VERSION;
        Ok(config)
    }

    pub fn save_global(&self, paths: &VeraPaths) -> Result<()> {
        paths.ensure_runtime_dirs()?;
        let mut current = self.clone();
        current.version = CURRENT_CONFIG_VERSION;
        let contents = toml::to_string_pretty(&current)?;
        let guard = PathGuard::new(paths.root.clone())?;
        let target = guard.resolve(Path::new("config.toml"))?;
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
        fs::rename(temporary, target)?;
        crate::paths::set_private_file(&target)?;
        Ok(())
    }

    fn apply_toml(&mut self, value: &toml::Value, path_base: &Path, project: bool) -> Result<()> {
        let table = value
            .as_table()
            .context("configuration root must be a TOML table")?;
        if let Some(version) = table.get("version").and_then(toml::Value::as_integer)
            && (version < 1 || version as u32 > CURRENT_CONFIG_VERSION)
        {
            anyhow::bail!("unsupported configuration version {version}");
        }
        if let Some(value) = table.get("provider").and_then(toml::Value::as_str) {
            self.provider = value.into();
        }
        if let Some(value) = table.get("model").and_then(toml::Value::as_str) {
            self.model = value.into();
        }
        if let Some(value) = table.get("effort").and_then(toml::Value::as_str) {
            self.effort = Some(value.into());
        }
        if let Some(value) = table
            .get("shell_timeout_seconds")
            .and_then(toml::Value::as_integer)
        {
            self.shell_timeout_seconds = value.max(1) as u64;
        }
        if let Some(value) = table
            .get("context_window_tokens")
            .and_then(toml::Value::as_integer)
        {
            self.context_window_tokens = value.max(1) as usize;
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
        if let Some(role) = table.get("role").and_then(toml::Value::as_str) {
            self.role = Some(role.into());
        }

        if let Some(approval) = table.get("approval").and_then(toml::Value::as_table) {
            if !project {
                if let Some(value) = approval.get("auto_read").and_then(toml::Value::as_bool) {
                    self.approval.auto_read = value;
                }
                for key in ["writes", "shell", "network"] {
                    if let Some(value) = approval.get(key).and_then(toml::Value::as_str) {
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
                if approval.get("auto_read").and_then(toml::Value::as_bool) == Some(false) {
                    self.approval.auto_read = false;
                }
                for key in ["writes", "shell", "network"] {
                    if approval.get(key).and_then(toml::Value::as_str) == Some("always") {
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

    pub fn validate(&self) -> Result<()> {
        if self.version == 0 || self.version > CURRENT_CONFIG_VERSION {
            anyhow::bail!("unsupported configuration version {}", self.version);
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
        if self.context_window_tokens == 0 {
            anyhow::bail!("context_window_tokens must be greater than zero");
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

fn read_toml(path: &PathBuf) -> Result<Option<toml::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(Some(
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
    ))
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
    fn legacy_configuration_is_accepted_and_normalized_on_save() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        fs::write(
            paths.root.join("config.toml"),
            "version = 1\nprovider = \"xai\"\n",
        )
        .unwrap();
        let config = Config::load(&paths, &project).unwrap();
        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        config.save_global(&paths).unwrap();
        let saved = fs::read_to_string(paths.root.join("config.toml")).unwrap();
        assert!(saved.contains("version = 2"));
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
