use std::collections::HashMap;
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
}

impl PermissionKind {
    pub fn mutates(self) -> bool {
        matches!(
            self,
            Self::Write | Self::Shell | Self::Hook | Self::Plugin | Self::Mcp | Self::Subagent
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalChoice {
    Deny,
    Once,
    Turn,
    Session,
}

#[derive(Debug, Default)]
pub struct PermissionPolicy {
    pub plan_mode: bool,
    grants: HashMap<PermissionKind, ApprovalChoice>,
}

impl PermissionPolicy {
    pub fn set_plan_mode(&mut self, enabled: bool) {
        self.plan_mode = enabled;
    }
    pub fn check(&self, kind: PermissionKind) -> Result<bool> {
        if kind == PermissionKind::Read {
            return Ok(true);
        }
        if self.plan_mode && kind.mutates() {
            return Ok(false);
        }
        Ok(matches!(
            self.grants.get(&kind),
            Some(ApprovalChoice::Turn | ApprovalChoice::Session)
        ))
    }
    pub fn remember(&mut self, kind: PermissionKind, choice: ApprovalChoice) {
        self.grants.insert(kind, choice);
    }
    pub async fn authorize(
        &mut self,
        kind: PermissionKind,
        description: &str,
        handler: &mut dyn ApprovalHandler,
        session: Option<&mut Session>,
    ) -> Result<()> {
        if self.plan_mode && kind.mutates() {
            return Err(VeraError::Permission("plan mode blocks mutating tools".into()).into());
        }
        if matches!(
            self.grants.get(&kind),
            Some(ApprovalChoice::Turn | ApprovalChoice::Session)
        ) {
            return Ok(());
        }
        let choice = handler.ask(kind, description).await?;
        if choice == ApprovalChoice::Deny {
            return Err(VeraError::Permission(format!("approval denied for {description}")).into());
        }
        if matches!(choice, ApprovalChoice::Turn | ApprovalChoice::Session) {
            self.remember(kind, choice);
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
        let mut command;
        #[cfg(target_os = "macos")]
        {
            command = Command::new("/usr/bin/sandbox-exec");
            let network_rule = if network {
                "(allow network*)"
            } else {
                "(deny network*)"
            };
            let profile = format!(
                "(version 1) (deny default) (allow process*) (allow file-read* (subpath \"{}\")) (allow file-write* (subpath \"{}\")) {}",
                cwd.display(),
                cwd.display(),
                network_rule
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
        for key in ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM", "SHELL"] {
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
        policy.remember(PermissionKind::Network, ApprovalChoice::Once);
        assert!(!policy.check(PermissionKind::Network).unwrap());
    }
}
