use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::error::VeraError;
use crate::sessions::Session;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Read,
    Write,
    Shell,
    Network,
    ExternalPath,
    Hook,
    Plugin,
    Mcp,
    Subagent,
    Browser,
}

impl PermissionKind {
    pub fn mutates(self) -> bool {
        matches!(
            self,
            Self::Write
                | Self::Shell
                | Self::Hook
                | Self::Plugin
                | Self::Mcp
                | Self::Subagent
                | Self::Browser
        )
    }

    pub fn risky(self) -> bool {
        matches!(
            self,
            Self::ExternalPath
                | Self::Shell
                | Self::Hook
                | Self::Plugin
                | Self::Mcp
                | Self::Subagent
                | Self::Browser
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PermissionMode {
    Plan,
    #[default]
    Confirm,
    Auto,
    Yolo,
}

impl PermissionMode {
    pub fn next(self) -> Self {
        match self {
            Self::Plan => Self::Confirm,
            Self::Confirm => Self::Auto,
            Self::Auto => Self::Yolo,
            Self::Yolo => Self::Plan,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Confirm => "Confirm",
            Self::Auto => "Auto",
            Self::Yolo => "Yolo",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalChoice {
    Deny,
    Once,
    Turn,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionEffect {
    Deny,
    Ask,
    Allow,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct PermissionMatcher {
    #[serde(default)]
    pub permission_kind: Option<PermissionKind>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub command_prefix: Option<String>,
    #[serde(default)]
    pub canonical_path: Option<PathBuf>,
    #[serde(default)]
    pub network_host: Option<String>,
    #[serde(default)]
    pub mcp_server: Option<String>,
    #[serde(default)]
    pub mcp_tool: Option<String>,
    #[serde(default)]
    pub browser_action: Option<String>,
    #[serde(default)]
    pub subagent_operation: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub effect: PermissionEffect,
    #[serde(default)]
    pub matcher: PermissionMatcher,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ActionSignature {
    pub permission_kind: PermissionKind,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub command_prefix: Option<String>,
    #[serde(default)]
    pub canonical_path: Option<PathBuf>,
    #[serde(default)]
    pub network_host: Option<String>,
    #[serde(default)]
    pub mcp_server: Option<String>,
    #[serde(default)]
    pub mcp_tool: Option<String>,
    #[serde(default)]
    pub browser_action: Option<String>,
    #[serde(default)]
    pub subagent_operation: Option<String>,
    #[serde(default)]
    pub safety_denied: bool,
}

impl Default for ActionSignature {
    fn default() -> Self {
        Self {
            permission_kind: PermissionKind::Read,
            tool_name: None,
            command_prefix: None,
            canonical_path: None,
            network_host: None,
            mcp_server: None,
            mcp_tool: None,
            browser_action: None,
            subagent_operation: None,
            safety_denied: false,
        }
    }
}

impl ActionSignature {
    pub fn for_description(kind: PermissionKind, description: &str) -> Self {
        Self {
            permission_kind: kind,
            tool_name: Some(normalize_action_text(description)),
            ..Self::default()
        }
    }

    pub fn matches(&self, matcher: &PermissionMatcher) -> bool {
        matcher
            .permission_kind
            .is_none_or(|kind| kind == self.permission_kind)
            && matcher
                .tool_name
                .as_deref()
                .is_none_or(|value| self.tool_name.as_deref() == Some(value))
            && matcher.command_prefix.as_deref().is_none_or(|value| {
                self.command_prefix
                    .as_deref()
                    .is_some_and(|actual| command_prefix_matches(value, actual))
            })
            && matcher
                .canonical_path
                .as_ref()
                .is_none_or(|value| self.canonical_path.as_ref() == Some(value))
            && matcher
                .network_host
                .as_deref()
                .is_none_or(|value| self.network_host.as_deref() == Some(value))
            && matcher
                .mcp_server
                .as_deref()
                .is_none_or(|value| self.mcp_server.as_deref() == Some(value))
            && matcher
                .mcp_tool
                .as_deref()
                .is_none_or(|value| self.mcp_tool.as_deref() == Some(value))
            && matcher
                .browser_action
                .as_deref()
                .is_none_or(|value| self.browser_action.as_deref() == Some(value))
            && matcher
                .subagent_operation
                .as_deref()
                .is_none_or(|value| self.subagent_operation.as_deref() == Some(value))
    }
}

fn normalize_action_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub fn normalize_command_prefix(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

fn command_prefix_matches(prefix: &str, actual: &str) -> bool {
    let prefix = normalize_command_prefix(prefix);
    let actual = normalize_command_prefix(actual);
    actual == prefix
        || actual
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with(' '))
}

#[derive(Clone, Debug)]
pub struct PermissionPolicy {
    mode: PermissionMode,
    grants: HashMap<ActionSignature, ApprovalChoice>,
    auto_read: bool,
    always_ask: HashSet<PermissionKind>,
    user_rules: Vec<PermissionRule>,
    project_rules: Vec<PermissionRule>,
    global_rules: Vec<PermissionRule>,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Confirm,
            grants: HashMap::new(),
            auto_read: true,
            always_ask: HashSet::new(),
            user_rules: Vec::new(),
            project_rules: Vec::new(),
            global_rules: Vec::new(),
        }
    }
}

impl PermissionPolicy {
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
        self.grants.clear();
    }

    pub fn begin_turn(&mut self) {
        self.grants
            .retain(|_, choice| *choice != ApprovalChoice::Turn);
    }

    pub fn end_turn(&mut self) {
        self.grants
            .retain(|_, choice| *choice != ApprovalChoice::Turn);
    }

    pub fn cycle_mode(&mut self) {
        self.set_mode(self.mode.next());
    }

    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.set_mode(if enabled {
            PermissionMode::Plan
        } else {
            PermissionMode::Confirm
        });
    }

    pub fn plan_mode(&self) -> bool {
        self.mode == PermissionMode::Plan
    }

    pub fn check(&self, kind: PermissionKind) -> Result<bool> {
        Ok(self.check_action(&ActionSignature {
            permission_kind: kind,
            ..ActionSignature::default()
        }))
    }
    pub fn remember(&mut self, kind: PermissionKind, choice: ApprovalChoice) {
        self.grants.insert(
            ActionSignature {
                permission_kind: kind,
                ..ActionSignature::default()
            },
            choice,
        );
    }

    pub fn set_auto_read(&mut self, enabled: bool) {
        self.auto_read = enabled;
    }

    pub fn set_always_ask(&mut self, kind: PermissionKind, enabled: bool) {
        if enabled {
            self.always_ask.insert(kind);
        } else {
            self.always_ask.remove(&kind);
        }
    }

    pub fn add_global_rule(&mut self, rule: PermissionRule) {
        self.global_rules.push(normalize_permission_rule(rule));
    }

    pub fn add_user_rule(&mut self, rule: PermissionRule) {
        self.user_rules.push(normalize_permission_rule(rule));
    }

    pub fn add_project_rule(&mut self, rule: PermissionRule) {
        self.project_rules.push(normalize_permission_rule(rule));
    }

    pub fn rules(&self) -> impl Iterator<Item = &PermissionRule> {
        self.user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
    }

    pub fn check_action(&self, action: &ActionSignature) -> bool {
        let action = normalize_action_signature(action);
        if action.safety_denied {
            return !action.safety_denied;
        }
        if self
            .user_rules
            .iter()
            .any(|rule| rule.effect == PermissionEffect::Deny && action.matches(&rule.matcher))
            || self
                .project_rules
                .iter()
                .any(|rule| rule.effect == PermissionEffect::Deny && action.matches(&rule.matcher))
            || self
                .global_rules
                .iter()
                .any(|rule| rule.effect == PermissionEffect::Deny && action.matches(&rule.matcher))
        {
            return false;
        }
        if self
            .user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
            .any(|rule| rule.effect == PermissionEffect::Ask && action.matches(&rule.matcher))
        {
            return false;
        }
        if self
            .user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
            .any(|rule| rule.effect == PermissionEffect::Allow && action.matches(&rule.matcher))
        {
            return true;
        }
        if self.always_ask.contains(&action.permission_kind) {
            return false;
        }
        if matches!(
            self.grants.get(&action),
            Some(ApprovalChoice::Turn | ApprovalChoice::Session)
        ) {
            return true;
        }
        if action.permission_kind == PermissionKind::Read || is_non_mutating_browser_action(&action)
        {
            return self.auto_read;
        }
        if self.mode == PermissionMode::Plan
            && action.permission_kind.mutates()
            && !is_non_mutating_browser_action(&action)
        {
            return false;
        }
        self.mode == PermissionMode::Yolo
            || (self.mode == PermissionMode::Auto && !action.permission_kind.risky())
    }
    pub async fn authorize(
        &mut self,
        kind: PermissionKind,
        description: &str,
        handler: &mut dyn ApprovalHandler,
        session: Option<&mut Session>,
    ) -> Result<()> {
        self.authorize_action(
            ActionSignature::for_description(kind, description),
            description,
            handler,
            session,
        )
        .await
    }

    pub async fn authorize_action(
        &mut self,
        action: ActionSignature,
        description: &str,
        handler: &mut dyn ApprovalHandler,
        mut session: Option<&mut Session>,
    ) -> Result<()> {
        let action = normalize_action_signature(&action);
        if action.safety_denied {
            return Err(
                VeraError::Permission("immutable safety policy denied action".into()).into(),
            );
        }
        let denied_by_rule = self
            .user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
            .any(|rule| rule.effect == PermissionEffect::Deny && action.matches(&rule.matcher));
        if denied_by_rule {
            return Err(
                VeraError::Permission(format!("permission rule denied {description}")).into(),
            );
        }
        let ruled_allow = self
            .user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
            .any(|rule| rule.effect == PermissionEffect::Allow && action.matches(&rule.matcher));
        let ruled_ask = self
            .user_rules
            .iter()
            .chain(self.project_rules.iter())
            .chain(self.global_rules.iter())
            .any(|rule| rule.effect == PermissionEffect::Ask && action.matches(&rule.matcher));
        let forced_ask = self.always_ask.contains(&action.permission_kind);
        if !ruled_ask && !forced_ask && (ruled_allow || self.check_action(&action)) {
            if let Some(session) = session.as_deref_mut() {
                session.append(crate::sessions::SessionRecord::Approval {
                    action: description.into(),
                    scope: if ruled_allow {
                        "rule"
                    } else {
                        self.mode.label()
                    }
                    .into(),
                    granted: true,
                })?;
            }
            return Ok(());
        }
        if self.mode == PermissionMode::Plan
            && action.permission_kind.mutates()
            && !is_non_mutating_browser_action(&action)
        {
            return Err(VeraError::Permission("plan mode blocks mutating tools".into()).into());
        }
        let choice = handler.ask(action.permission_kind, description).await?;
        if choice == ApprovalChoice::Deny {
            return Err(VeraError::Permission(format!("approval denied for {description}")).into());
        }
        if matches!(choice, ApprovalChoice::Turn | ApprovalChoice::Session) {
            self.grants.insert(action, choice);
        }
        if let Some(session) = session {
            session.append(crate::sessions::SessionRecord::Approval {
                action: description.into(),
                scope: format!("{choice:?}"),
                granted: true,
            })?;
        }
        Ok(())
    }
}

fn is_non_mutating_browser_action(action: &ActionSignature) -> bool {
    action.permission_kind == PermissionKind::Browser
        && matches!(
            action.browser_action.as_deref(),
            Some("status" | "evaluate" | "snapshot")
        )
}

fn normalize_permission_rule(mut rule: PermissionRule) -> PermissionRule {
    if let Some(prefix) = rule.matcher.command_prefix.as_mut() {
        *prefix = normalize_command_prefix(prefix);
    }
    if let Some(host) = rule.matcher.network_host.as_mut() {
        *host = host.trim().to_ascii_lowercase();
    }
    if let Some(action) = rule.matcher.browser_action.as_mut() {
        *action = action.trim().to_ascii_lowercase();
    }
    if let Some(path) = rule.matcher.canonical_path.as_mut()
        && path.exists()
        && let Ok(canonical) = fs::canonicalize(&*path)
    {
        *path = canonical;
    }
    rule
}

fn normalize_action_signature(action: &ActionSignature) -> ActionSignature {
    let mut normalized = action.clone();
    if let Some(prefix) = normalized.command_prefix.as_mut() {
        *prefix = normalize_command_prefix(prefix);
    }
    if let Some(host) = normalized.network_host.as_mut() {
        *host = host.trim().to_ascii_lowercase();
    }
    if let Some(browser_action) = normalized.browser_action.as_mut() {
        *browser_action = browser_action.trim().to_ascii_lowercase();
    }
    if let Some(path) = normalized.canonical_path.as_mut()
        && path.exists()
        && let Ok(canonical) = fs::canonicalize(&*path)
    {
        *path = canonical;
    }
    normalized
}

#[async_trait]
pub trait ApprovalHandler: Send {
    async fn ask(&mut self, kind: PermissionKind, description: &str) -> Result<ApprovalChoice>;
}

pub struct TerminalApproval;

#[async_trait]
impl ApprovalHandler for TerminalApproval {
    async fn ask(&mut self, kind: PermissionKind, description: &str) -> Result<ApprovalChoice> {
        use std::io::{self, Write};
        print!(
            "\n[approval {:?}] {}\nAllow? [y] once, [t] turn, [s] session, [n] deny: ",
            kind, description
        );
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        Ok(match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalChoice::Once,
            "t" => ApprovalChoice::Turn,
            "s" => ApprovalChoice::Session,
            _ => ApprovalChoice::Deny,
        })
    }
}

pub struct PathGuard {
    root: PathBuf,
}

impl PathGuard {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
        })
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn resolve(&self, path: &Path) -> Result<PathBuf> {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let parent = candidate.parent().unwrap_or(&self.root);
        let canonical_parent =
            fs::canonicalize(parent).map_err(|_| VeraError::UnsafePath(candidate.clone()))?;
        let normalized = canonical_parent.join(candidate.file_name().unwrap_or_default());
        if !normalized.starts_with(&self.root) || normalized.exists() && normalized.is_symlink() {
            return Err(VeraError::UnsafePath(normalized).into());
        }
        Ok(normalized)
    }
}

pub struct Sandbox;

impl Sandbox {
    pub fn command(program: &str, args: &[String], cwd: &Path, network: bool) -> Command {
        Self::command_with_policy(program, args, cwd, network, true)
    }

    pub fn read_only_command(program: &str, args: &[String], cwd: &Path, network: bool) -> Command {
        Self::command_with_policy(program, args, cwd, network, false)
    }

    /// Build the same environment-cleared Seatbelt policy for a portable PTY
    /// child. The PTY crate owns the process launch, so this mirrors the
    /// foreground command boundary without handing credentials to it.
    pub fn pty_command(
        program: &str,
        args: &[String],
        cwd: &Path,
        network: bool,
    ) -> portable_pty::CommandBuilder {
        let mut command;
        #[cfg(target_os = "macos")]
        {
            let network_rule = if network {
                "(allow network*)"
            } else {
                "(deny network*)"
            };
            let safe_cwd = escape_sandbox_path(cwd);
            let profile = format!(
                "(version 1) (deny default) (allow process*) (allow file-read* (subpath \"{safe_cwd}\") (subpath \"/bin\") (subpath \"/usr/bin\") (subpath \"/usr/lib\") (subpath \"/System/Library\") (subpath \"/Library\")) (allow file-write* (subpath \"{safe_cwd}\")) {network_rule}"
            );
            command = portable_pty::CommandBuilder::new("/usr/bin/sandbox-exec");
            command.args(["-p", &profile, program]);
            command.args(args);
        }
        #[cfg(not(target_os = "macos"))]
        {
            command = portable_pty::CommandBuilder::new(program);
            command.args(args);
        }
        command.cwd(cwd);
        command.env_clear();
        for key in ["PATH", "TMPDIR", "LANG", "LC_ALL", "TERM"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
    }

    fn command_with_policy(
        program: &str,
        args: &[String],
        cwd: &Path,
        network: bool,
        writable: bool,
    ) -> Command {
        let mut command;
        #[cfg(target_os = "macos")]
        {
            command = Command::new("/usr/bin/sandbox-exec");
            let safe_cwd = escape_sandbox_path(cwd);
            let network_rule = if network {
                "(allow network*)"
            } else {
                "(deny network*)"
            };
            let write_rule = if writable {
                format!("(allow file-write* (subpath \"{safe_cwd}\"))")
            } else {
                "(deny file-write*)".into()
            };
            let profile = format!(
                "(version 1) (deny default) (allow process*) (allow file-read* (subpath \"{safe_cwd}\") (subpath \"/bin\") (subpath \"/usr/bin\") (subpath \"/usr/lib\") (subpath \"/System/Library\") (subpath \"/Library\")) {write_rule} {network_rule}",
            );
            command.arg("-p").arg(profile).arg(program);
        }
        #[cfg(not(target_os = "macos"))]
        {
            command = Command::new(program);
        }
        #[cfg(target_os = "macos")]
        {
            command.args(args);
        }
        #[cfg(not(target_os = "macos"))]
        {
            command.args(args);
        }
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.env_clear();
        // Do not forward HOME or SHELL: auth stores and user startup files are
        // intentionally outside the child process capability boundary.
        for key in ["PATH", "TMPDIR", "LANG", "LC_ALL", "TERM"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
        command
    }

    pub async fn run(
        program: &str,
        args: &[String],
        cwd: &Path,
        network: bool,
        limit: Duration,
    ) -> Result<CommandOutput> {
        let child = Self::command(program, args, cwd, network)
            .spawn()
            .context("spawn sandboxed command")?;
        let output = timeout(limit, child.wait_with_output())
            .await
            .context("command timed out")??;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(target_os = "macos")]
fn escape_sandbox_path(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[derive(Debug)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_mode_blocks_mutation() {
        let mut policy = PermissionPolicy::default();
        policy.set_plan_mode(true);
        assert!(!policy.check(PermissionKind::Write).unwrap());
        assert!(policy.check(PermissionKind::Read).unwrap());
    }

    #[test]
    fn turn_and_session_grants_are_remembered() {
        let mut policy = PermissionPolicy::default();
        policy.remember(PermissionKind::Shell, ApprovalChoice::Turn);
        assert!(policy.check(PermissionKind::Shell).unwrap());
        policy.end_turn();
        assert!(!policy.check(PermissionKind::Shell).unwrap());
        policy.remember(PermissionKind::Shell, ApprovalChoice::Session);
        assert!(policy.check(PermissionKind::Shell).unwrap());
        policy.remember(PermissionKind::Network, ApprovalChoice::Once);
        assert!(!policy.check(PermissionKind::Network).unwrap());
    }

    #[test]
    fn modes_cycle_in_requested_order() {
        let mut policy = PermissionPolicy::default();
        assert_eq!(policy.mode(), PermissionMode::Confirm);
        policy.cycle_mode();
        assert_eq!(policy.mode(), PermissionMode::Auto);
        policy.cycle_mode();
        assert_eq!(policy.mode(), PermissionMode::Yolo);
        policy.cycle_mode();
        assert_eq!(policy.mode(), PermissionMode::Plan);
        policy.cycle_mode();
        assert_eq!(policy.mode(), PermissionMode::Confirm);
    }

    #[test]
    fn auto_approves_non_risky_and_external_actions_but_not_risky_tools() {
        let mut policy = PermissionPolicy::default();
        policy.set_mode(PermissionMode::Auto);
        assert!(policy.check(PermissionKind::Write).unwrap());
        assert!(policy.check(PermissionKind::Network).unwrap());
        assert!(!policy.check(PermissionKind::ExternalPath).unwrap());
        assert!(!policy.check(PermissionKind::Shell).unwrap());
    }

    #[test]
    fn yolo_approves_without_changing_hard_path_invariants() {
        let mut policy = PermissionPolicy::default();
        policy.set_mode(PermissionMode::Yolo);
        assert!(policy.check(PermissionKind::Shell).unwrap());
        assert!(policy.check(PermissionKind::Mcp).unwrap());
    }

    #[test]
    fn action_signatures_normalize_and_rules_precede_mode_defaults() {
        let mut policy = PermissionPolicy::default();
        policy.add_project_rule(PermissionRule {
            effect: PermissionEffect::Deny,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Shell),
                command_prefix: Some("git status".into()),
                ..PermissionMatcher::default()
            },
        });
        let action = ActionSignature {
            permission_kind: PermissionKind::Shell,
            command_prefix: Some(normalize_command_prefix("git   status --short")),
            ..ActionSignature::default()
        };
        assert!(!policy.check_action(&action));
    }

    #[test]
    fn network_rules_can_match_the_specific_tool_and_command() {
        let mut policy = PermissionPolicy::default();
        policy.set_mode(PermissionMode::Yolo);
        policy.add_global_rule(PermissionRule {
            effect: PermissionEffect::Deny,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Network),
                tool_name: Some("shell".into()),
                command_prefix: Some("curl https://example.test".into()),
                ..PermissionMatcher::default()
            },
        });
        let denied = ActionSignature {
            permission_kind: PermissionKind::Network,
            tool_name: Some("shell".into()),
            command_prefix: Some("curl   https://example.test --fail".into()),
            ..ActionSignature::default()
        };
        let allowed = ActionSignature {
            command_prefix: Some("curl https://other.test".into()),
            ..denied.clone()
        };
        assert!(!policy.check_action(&denied));
        assert!(policy.check_action(&allowed));
    }

    #[test]
    fn action_grants_are_narrower_than_permission_categories() {
        let mut policy = PermissionPolicy::default();
        policy.remember(PermissionKind::Shell, ApprovalChoice::Turn);
        let exact = ActionSignature {
            permission_kind: PermissionKind::Shell,
            ..ActionSignature::default()
        };
        let different = ActionSignature {
            permission_kind: PermissionKind::Shell,
            command_prefix: Some("rm".into()),
            ..ActionSignature::default()
        };
        assert!(policy.check_action(&exact));
        assert!(!policy.check_action(&different));
    }

    #[test]
    fn ask_rules_override_mode_defaults_and_allows() {
        let mut policy = PermissionPolicy::default();
        policy.set_mode(PermissionMode::Yolo);
        policy.add_user_rule(PermissionRule {
            effect: PermissionEffect::Ask,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Shell),
                ..PermissionMatcher::default()
            },
        });
        policy.add_global_rule(PermissionRule {
            effect: PermissionEffect::Allow,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Shell),
                ..PermissionMatcher::default()
            },
        });
        policy.set_mode(PermissionMode::Yolo);
        assert!(!policy.check(PermissionKind::Shell).unwrap());
    }

    #[test]
    fn ordered_rules_can_deny_or_ask_even_for_automatic_reads() {
        let mut policy = PermissionPolicy::default();
        policy.add_user_rule(PermissionRule {
            effect: PermissionEffect::Deny,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Read),
                ..PermissionMatcher::default()
            },
        });
        assert!(!policy.check(PermissionKind::Read).unwrap());
    }

    #[test]
    fn configured_approval_defaults_reach_action_evaluation() {
        let mut policy = PermissionPolicy::default();
        policy.set_auto_read(false);
        policy.set_always_ask(PermissionKind::Shell, true);
        policy.set_mode(PermissionMode::Yolo);
        assert!(!policy.check(PermissionKind::Read).unwrap());
        assert!(!policy.check(PermissionKind::Shell).unwrap());
    }

    #[test]
    fn browser_snapshot_evaluation_is_read_only_but_still_rule_matchable() {
        let action = ActionSignature {
            permission_kind: PermissionKind::Browser,
            tool_name: Some("browser_snapshot".into()),
            browser_action: Some("evaluate".into()),
            ..ActionSignature::default()
        };
        let mut policy = PermissionPolicy::default();
        policy.set_plan_mode(true);
        assert!(policy.check_action(&action));
        policy.add_user_rule(PermissionRule {
            effect: PermissionEffect::Deny,
            matcher: PermissionMatcher {
                browser_action: Some("evaluate".into()),
                ..PermissionMatcher::default()
            },
        });
        assert!(!policy.check_action(&action));
    }

    #[test]
    fn mcp_server_and_tool_matchers_are_independent() {
        let mut policy = PermissionPolicy::default();
        policy.add_global_rule(PermissionRule {
            effect: PermissionEffect::Deny,
            matcher: PermissionMatcher {
                permission_kind: Some(PermissionKind::Mcp),
                mcp_server: Some("fixture".into()),
                mcp_tool: Some("write_file".into()),
                ..PermissionMatcher::default()
            },
        });
        policy.set_mode(PermissionMode::Yolo);
        let denied = ActionSignature {
            permission_kind: PermissionKind::Mcp,
            mcp_server: Some("fixture".into()),
            mcp_tool: Some("write_file".into()),
            ..ActionSignature::default()
        };
        let allowed = ActionSignature {
            mcp_tool: Some("read_file".into()),
            ..denied.clone()
        };
        assert!(!policy.check_action(&denied));
        assert!(policy.check_action(&allowed));
    }
}
