use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::paths::{VeraPaths, set_private_file};
use crate::safety::{PathGuard, PermissionMode, PermissionRule};
use crate::sessions::DisplayMode;

pub const CURRENT_GLOBAL_STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_read: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub writes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_plugins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_plugins: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_mcp: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_skills: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_roots: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_cdp_endpoints: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_rules: Option<Vec<PermissionRule>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalState {
    pub version: u32,
    pub config: ConfigOverrides,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_mode: Option<DisplayMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded_skills: Option<Vec<String>>,
}

impl Default for GlobalState {
    fn default() -> Self {
        Self {
            version: CURRENT_GLOBAL_STATE_VERSION,
            config: ConfigOverrides::default(),
            display_mode: None,
            permission_mode: None,
            loaded_skills: None,
        }
    }
}

impl GlobalState {
    pub fn load(paths: &VeraPaths) -> Result<Self> {
        match fs::symlink_metadata(&paths.global_state) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                anyhow::bail!(
                    "global state {} is not a regular file",
                    paths.global_state.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        }
        let text = fs::read_to_string(&paths.global_state)
            .with_context(|| format!("read global state {}", paths.global_state.display()))?;
        let state: Self = serde_json::from_str(&text).map_err(|error| {
            anyhow::anyhow!(
                "parse global state {}: {error}",
                paths.global_state.display()
            )
        })?;
        state.validate_version(paths)?;
        Ok(state)
    }

    fn validate_version(&self, paths: &VeraPaths) -> Result<()> {
        if self.version == 0 || self.version > CURRENT_GLOBAL_STATE_VERSION {
            anyhow::bail!(
                "global state {} has unsupported version {}",
                paths.global_state.display(),
                self.version
            );
        }
        if let Some(mode) = self.permission_mode.as_deref() {
            PermissionMode::parse(mode).with_context(|| {
                format!(
                    "global state {} has invalid permission_mode {mode:?}",
                    paths.global_state.display()
                )
            })?;
        }
        validate_string_list(
            "loaded_skills",
            self.loaded_skills.as_deref().unwrap_or(&[]),
        )
        .with_context(|| format!("validate global state {}", paths.global_state.display()))?;
        Ok(())
    }

    pub fn permission_mode(&self) -> Option<PermissionMode> {
        self.permission_mode
            .as_deref()
            .and_then(PermissionMode::parse)
    }

    pub fn save(&self, paths: &VeraPaths, project: &Path) -> Result<()> {
        self.validate_version(paths)?;
        // Validate the complete merged result before touching the state file.
        Config::load_with_state(paths, project, self)
            .with_context(|| format!("validate global state {}", paths.global_state.display()))?;
        paths.ensure_runtime_dirs()?;
        let guard = PathGuard::new(paths.root.clone())?;
        let target = guard.resolve(&paths.global_state)?;
        let mut contents = serde_json::to_vec_pretty(self)?;
        contents.push(b'\n');
        let temporary = guard.resolve(&paths.root.join(format!(
            ".global-state.vera-tmp-{}",
            uuid::Uuid::new_v4().simple()
        )))?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| {
                format!("create global state temporary file {}", temporary.display())
            })?;
        set_private_file(&temporary)?;
        if let Err(error) = file.write_all(&contents).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("write global state {}", target.display()));
        }
        if let Err(error) = fs::rename(&temporary, &target) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("replace global state {}", target.display()));
        }
        Ok(())
    }

    pub fn set_value(&mut self, key: &str, raw: &str) -> Result<()> {
        match key {
            "provider" => self.config.provider = Some(raw.to_owned()),
            "model" => self.config.model = Some(raw.to_owned()),
            "effort" => self.config.effort = Some(raw.to_owned()),
            "shell_timeout_seconds" => {
                self.config.shell_timeout_seconds = Some(parse_json(raw, key)?)
            }
            "context_window_tokens" => {
                self.config.context_window_tokens = Some(parse_json(raw, key)?)
            }
            "role" => self.config.role = Some(raw.to_owned()),
            "hooks" => self.config.hooks = Some(parse_json(raw, key)?),
            "trusted_plugins" => self.config.trusted_plugins = Some(parse_json(raw, key)?),
            "enabled_plugins" => self.config.enabled_plugins = Some(parse_json(raw, key)?),
            "enabled_mcp" => self.config.enabled_mcp = Some(parse_json(raw, key)?),
            "allowed_skills" => self.config.allowed_skills = Some(parse_json(raw, key)?),
            "prompt_roots" => self.config.prompt_roots = Some(parse_json(raw, key)?),
            "browser_cdp_endpoints" => {
                self.config.browser_cdp_endpoints = Some(parse_json(raw, key)?)
            }
            "permission_rules" => self.config.permission_rules = Some(parse_json(raw, key)?),
            "approval.auto_read" => self.approval_mut().auto_read = Some(parse_json(raw, key)?),
            "approval.writes" => self.approval_mut().writes = Some(raw.to_owned()),
            "approval.shell" => self.approval_mut().shell = Some(raw.to_owned()),
            "approval.network" => self.approval_mut().network = Some(raw.to_owned()),
            "display_mode" => {
                self.display_mode = Some(
                    DisplayMode::parse(raw)
                        .with_context(|| "display_mode must be grouped, minimal, or detailed")?,
                );
            }
            "permission_mode" => {
                PermissionMode::parse(raw)
                    .with_context(|| "permission_mode must be plan, confirm, auto, or yolo")?;
                self.permission_mode = Some(raw.to_owned());
            }
            "loaded_skills" => self.loaded_skills = Some(parse_json(raw, key)?),
            _ => anyhow::bail!("unknown setting key {key:?}"),
        }
        Ok(())
    }

    pub fn unset_value(&mut self, key: &str) -> Result<()> {
        match key {
            "provider" => self.config.provider = None,
            "model" => self.config.model = None,
            "effort" => self.config.effort = None,
            "shell_timeout_seconds" => self.config.shell_timeout_seconds = None,
            "context_window_tokens" => self.config.context_window_tokens = None,
            "role" => self.config.role = None,
            "hooks" => self.config.hooks = None,
            "trusted_plugins" => self.config.trusted_plugins = None,
            "enabled_plugins" => self.config.enabled_plugins = None,
            "enabled_mcp" => self.config.enabled_mcp = None,
            "allowed_skills" => self.config.allowed_skills = None,
            "prompt_roots" => self.config.prompt_roots = None,
            "browser_cdp_endpoints" => self.config.browser_cdp_endpoints = None,
            "permission_rules" => self.config.permission_rules = None,
            "approval.auto_read" | "approval.writes" | "approval.shell" | "approval.network" => {
                let approval = self.approval_mut();
                match key {
                    "approval.auto_read" => approval.auto_read = None,
                    "approval.writes" => approval.writes = None,
                    "approval.shell" => approval.shell = None,
                    "approval.network" => approval.network = None,
                    _ => unreachable!(),
                }
                if approval == &ApprovalOverrides::default() {
                    self.config.approval = None;
                }
            }
            "display_mode" => self.display_mode = None,
            "permission_mode" => self.permission_mode = None,
            "loaded_skills" => self.loaded_skills = None,
            _ => anyhow::bail!("unknown setting key {key:?}"),
        }
        Ok(())
    }

    fn approval_mut(&mut self) -> &mut ApprovalOverrides {
        self.config
            .approval
            .get_or_insert_with(ApprovalOverrides::default)
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(raw: &str, key: &str) -> Result<T> {
    serde_json::from_str(raw)
        .with_context(|| format!("setting {key:?} expects JSON syntax for this value"))
}

fn validate_string_list(field: &str, values: &[String]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if value.trim().is_empty() || !seen.insert(value) {
            anyhow::bail!("{field} contains an empty or duplicate entry: {value:?}");
        }
    }
    Ok(())
}

pub fn value_for_key(config: &Config, state: &GlobalState, key: &str) -> Result<Value> {
    let value = match key {
        "provider" => serde_json::to_value(&config.provider)?,
        "model" => serde_json::to_value(&config.model)?,
        "effort" => serde_json::to_value(&config.effort)?,
        "shell_timeout_seconds" => serde_json::to_value(config.shell_timeout_seconds)?,
        "context_window_tokens" => serde_json::to_value(config.context_window_tokens)?,
        "hooks" => serde_json::to_value(&config.hooks)?,
        "trusted_plugins" => serde_json::to_value(&config.trusted_plugins)?,
        "enabled_plugins" => serde_json::to_value(&config.enabled_plugins)?,
        "enabled_mcp" => serde_json::to_value(&config.enabled_mcp)?,
        "allowed_skills" => serde_json::to_value(&config.allowed_skills)?,
        "role" => serde_json::to_value(&config.role)?,
        "prompt_roots" => serde_json::to_value(&config.prompt_roots)?,
        "browser_cdp_endpoints" => serde_json::to_value(&config.browser_cdp_endpoints)?,
        "permission_rules" => serde_json::to_value(&config.permission_rules)?,
        "approval.auto_read" => serde_json::to_value(config.approval.auto_read)?,
        "approval.writes" => serde_json::to_value(&config.approval.writes)?,
        "approval.shell" => serde_json::to_value(&config.approval.shell)?,
        "approval.network" => serde_json::to_value(&config.approval.network)?,
        "display_mode" => serde_json::to_value(state.display_mode.unwrap_or_default())?,
        "permission_mode" => {
            serde_json::to_value(state.permission_mode.as_deref().unwrap_or("confirm"))?
        }
        "loaded_skills" => serde_json::to_value(state.loaded_skills.as_deref().unwrap_or(&[]))?,
        _ => anyhow::bail!("unknown setting key {key:?}"),
    };
    Ok(value)
}

pub fn setting_keys() -> &'static [&'static str] {
    &[
        "provider",
        "model",
        "effort",
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
        "approval.auto_read",
        "approval.writes",
        "approval.shell",
        "approval.network",
        "display_mode",
        "permission_mode",
        "loaded_skills",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn state_is_partial_and_unset_removes_only_the_override() {
        let mut state = GlobalState::default();
        state.set_value("provider", "xai-oauth").unwrap();
        state.set_value("enabled_plugins", r#"["one"]"#).unwrap();
        assert_eq!(state.config.provider.as_deref(), Some("xai-oauth"));
        assert_eq!(
            state
                .config
                .enabled_plugins
                .as_deref()
                .map(|values| values[0].as_str()),
            Some("one")
        );
        state.unset_value("provider").unwrap();
        assert!(state.config.provider.is_none());
    }

    #[test]
    fn effective_values_use_defaults_when_state_is_empty() {
        let config = Config::default();
        let value = value_for_key(&config, &GlobalState::default(), "provider").unwrap();
        assert_eq!(value, Value::String("openai-codex".into()));
    }

    #[test]
    fn failed_state_validation_does_not_replace_disk_state() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let before = fs::read(&paths.global_state).unwrap();
        let mut candidate = GlobalState::load(&paths).unwrap();
        candidate.config.context_window_tokens = Some(0);
        assert!(candidate.save(&paths, &project).is_err());
        assert_eq!(fs::read(&paths.global_state).unwrap(), before);
        assert_eq!(
            GlobalState::load(&paths)
                .unwrap()
                .config
                .context_window_tokens,
            None
        );
    }

    #[test]
    fn successful_state_write_is_atomic_and_changes_next_load() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let mut candidate = GlobalState::load(&paths).unwrap();
        candidate.set_value("provider", "xai-oauth").unwrap();
        candidate.save(&paths, &project).unwrap();
        assert_eq!(
            GlobalState::load(&paths)
                .unwrap()
                .config
                .provider
                .as_deref(),
            Some("xai-oauth")
        );
        assert_eq!(
            Config::load(&paths, &project).unwrap().provider,
            "xai-oauth"
        );
    }
}
