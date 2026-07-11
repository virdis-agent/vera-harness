use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use uuid::Uuid;

use crate::safety::{PathGuard, Sandbox};
use crate::sessions::{LifecycleState, Session};

const MAX_WORKTREE_DIFF_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub worktree_id: String,
    pub path: PathBuf,
    pub branch: String,
    pub base_revision: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorktreeReview {
    pub worktree_id: String,
    pub base_revision: String,
    pub changed_paths: Vec<String>,
    pub diff_stat: String,
    pub tests: Vec<String>,
    pub conflicts: Vec<String>,
}

#[derive(Clone)]
pub struct WorktreeManager {
    repository: PathBuf,
    worktree_root: PathBuf,
    sandboxed: bool,
}

impl WorktreeManager {
    pub fn new(repository: PathBuf) -> Result<Self> {
        let repository = std::fs::canonicalize(repository)?;
        let worktree_root = worktree_root_for(&repository);
        Ok(Self {
            repository,
            worktree_root,
            sandboxed: true,
        })
    }

    #[cfg(test)]
    fn for_fixture(repository: PathBuf) -> Result<Self> {
        let repository = std::fs::canonicalize(repository)?;
        let worktree_root = worktree_root_for(&repository);
        Ok(Self {
            repository,
            worktree_root,
            sandboxed: false,
        })
    }

    pub async fn create(&self, session: &mut Session) -> Result<WorktreeInfo> {
        if !self.repository.join(".git").exists() {
            anyhow::bail!("write-capable delegation requires a Git-backed repository");
        }
        if self.repository.join(".gitmodules").exists() {
            anyhow::bail!("write-capable delegation is disabled for repositories with submodules");
        }
        let index = self.git(&["ls-files", "--stage"], &self.repository).await?;
        if index
            .stdout
            .lines()
            .any(|line| line.split_whitespace().next() == Some("160000"))
        {
            anyhow::bail!("write-capable delegation is disabled for repositories with gitlinks");
        }
        let head = self.git(&["rev-parse", "HEAD"], &self.repository).await?;
        let base_revision = head.stdout.trim().to_owned();
        if base_revision.is_empty() {
            anyhow::bail!("repository has no captured HEAD");
        }
        let worktree_id = Uuid::new_v4().simple().to_string();
        let branch = format!("vera-agent-{worktree_id}");
        let guard = PathGuard::new(self.repository.clone())?;
        let relative_root = self
            .worktree_root
            .strip_prefix(&self.repository)
            .context("worktree root escaped repository")?;
        let parent = guard.resolve(relative_root.parent().unwrap_or_else(|| Path::new(".")))?;
        std::fs::create_dir_all(&parent)?;
        let worktree_root = guard.resolve(relative_root)?;
        std::fs::create_dir_all(&worktree_root)?;
        let path = guard.resolve(&relative_root.join(&worktree_id))?;
        let result = self
            .git_mutating(
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_str().context("invalid worktree path")?,
                    &base_revision,
                ],
                &self.repository,
            )
            .await?;
        if result.status != 0 {
            anyhow::bail!("git worktree add failed: {}", result.stderr);
        }
        let info = WorktreeInfo {
            worktree_id: worktree_id.clone(),
            path: path.clone(),
            branch,
            base_revision,
        };
        if let Err(error) = session.record_worktree_state(
            worktree_id,
            LifecycleState {
                state: "created".into(),
                detail: Some(serde_json::to_string(&info)?),
            },
        ) {
            let _ = self
                .git_mutating(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        path.to_str().unwrap_or_default(),
                    ],
                    &self.repository,
                )
                .await;
            return Err(error);
        }
        Ok(info)
    }

    pub async fn review(&self, info: &WorktreeInfo) -> Result<WorktreeReview> {
        self.validate_info(info)?;
        self.validate_existing_path(info)?;
        let source_guard = PathGuard::new(info.path.clone())?;
        let changed = self
            .git_paths(
                &["diff", "--name-only", &info.base_revision, "--"],
                &info.path,
            )
            .await?;
        let stat = self
            .git(&["diff", "--stat", &info.base_revision, "--"], &info.path)
            .await?;
        let untracked = self
            .git_paths(&["ls-files", "--others", "--exclude-standard"], &info.path)
            .await?;
        let mut changed_paths = Vec::new();
        for path in changed.iter().take(512) {
            source_guard.resolve(Path::new(path))?;
            changed_paths.push(path.chars().take(512).collect::<String>());
        }
        let mut diff_stat = stat.stdout;
        for path in untracked.iter().take(512) {
            let source = source_guard.resolve(Path::new(path))?;
            if !changed_paths.iter().any(|changed| changed == path) {
                changed_paths.push(path.chars().take(512).collect());
            }
            let size = std::fs::metadata(source)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            diff_stat.push_str(&format!("\n untracked {path} ({size} bytes)"));
        }
        Ok(WorktreeReview {
            worktree_id: info.worktree_id.clone(),
            base_revision: info.base_revision.clone(),
            changed_paths,
            diff_stat: diff_stat.chars().take(16_000).collect(),
            tests: Vec::new(),
            conflicts: Vec::new(),
        })
    }

    pub async fn apply_review(
        &self,
        info: &WorktreeInfo,
        session: &mut Session,
    ) -> Result<WorktreeReview> {
        self.validate_info(info)?;
        self.validate_existing_path(info)?;
        let current = self.git(&["rev-parse", "HEAD"], &self.repository).await?;
        if current.stdout.trim() != info.base_revision {
            anyhow::bail!("parent repository moved since worktree creation; diff is stale");
        }
        let review = self.review(info).await?;
        let mut parent_changed = BTreeSet::new();
        parent_changed.extend(
            self.git_paths(&["diff", "--name-only"], &self.repository)
                .await?,
        );
        parent_changed.extend(
            self.git_paths(&["diff", "--cached", "--name-only"], &self.repository)
                .await?,
        );
        parent_changed.extend(
            self.git_paths(
                &["ls-files", "--others", "--exclude-standard"],
                &self.repository,
            )
            .await?,
        );
        if let Some(conflict) = review
            .changed_paths
            .iter()
            .find(|path| parent_changed.contains(*path))
        {
            session.record_merge_decision(
                &info.worktree_id,
                "conflict",
                format!("parent already changed {conflict}"),
            )?;
            anyhow::bail!("selected worktree diff conflicts with parent path {conflict}");
        }
        let diff = self
            .git_bytes(&["diff", "--binary", &info.base_revision, "--"], &info.path)
            .await?;
        if diff.len() > MAX_WORKTREE_DIFF_BYTES {
            session.record_merge_decision(
                &info.worktree_id,
                "conflict",
                format!(
                    "worktree diff exceeds {} byte limit",
                    MAX_WORKTREE_DIFF_BYTES
                ),
            )?;
            anyhow::bail!(
                "worktree diff exceeds {} byte limit",
                MAX_WORKTREE_DIFF_BYTES
            );
        }
        let guard = PathGuard::new(self.repository.clone())?;
        let source_guard = PathGuard::new(info.path.clone())?;
        let mut untracked_files = Vec::new();
        let mut untracked_bytes = 0usize;
        let untracked = self
            .git_paths(&["ls-files", "--others", "--exclude-standard"], &info.path)
            .await?;
        for relative in untracked.iter().take(512) {
            let source = source_guard.resolve(Path::new(relative))?;
            if source.is_file() {
                let bytes = std::fs::read(source)?;
                untracked_bytes = untracked_bytes.saturating_add(bytes.len());
                if diff.len().saturating_add(untracked_bytes) > MAX_WORKTREE_DIFF_BYTES {
                    session.record_merge_decision(
                        &info.worktree_id,
                        "conflict",
                        format!(
                            "tracked and untracked worktree changes exceed {} byte limit",
                            MAX_WORKTREE_DIFF_BYTES
                        ),
                    )?;
                    anyhow::bail!(
                        "tracked and untracked worktree changes exceed {} byte limit",
                        MAX_WORKTREE_DIFF_BYTES
                    );
                }
                untracked_files.push((relative.to_owned(), bytes));
            }
        }
        for relative in &review.changed_paths {
            let path = guard.resolve(Path::new(relative))?;
            session.record_binary_preimage(path.clone(), std::fs::read(&path).ok())?;
        }
        let temporary = self
            .repository
            .join(format!(".vera-apply-{}.patch", Uuid::new_v4().simple()));
        std::fs::write(&temporary, &diff)?;
        let result = if diff.is_empty() {
            crate::safety::CommandOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            }
        } else {
            self.git_allow_failure(
                &[
                    "apply",
                    "--3way",
                    temporary.to_str().context("invalid patch path")?,
                ],
                &self.repository,
            )
            .await?
        };
        let _ = std::fs::remove_file(&temporary);
        if result.status != 0 {
            session.record_merge_decision(&info.worktree_id, "conflict", result.stderr.clone())?;
            anyhow::bail!("selected worktree diff conflicts: {}", result.stderr);
        }
        for (relative, bytes) in untracked_files {
            let path = guard.resolve(Path::new(&relative))?;
            atomic_write_bytes(&path, &bytes)?;
        }
        session.record_merge_decision(&info.worktree_id, "accepted", "guarded git apply")?;
        Ok(review)
    }

    pub async fn discard(&self, info: &WorktreeInfo, session: &mut Session) -> Result<()> {
        self.validate_info(info)?;
        self.validate_existing_path(info)?;
        let result = self
            .git_mutating(
                &[
                    "worktree",
                    "remove",
                    "--force",
                    info.path.to_str().context("invalid worktree path")?,
                ],
                &self.repository,
            )
            .await?;
        if result.status != 0 {
            anyhow::bail!("git worktree remove failed: {}", result.stderr);
        }
        session.record_worktree_state(
            &info.worktree_id,
            LifecycleState {
                state: "discarded".into(),
                detail: Some(serde_json::to_string(info)?),
            },
        )?;
        Ok(())
    }

    /// Reconstruct a worktree descriptor from the durable session journal so
    /// an interrupted parent can inspect or discard an abandoned worktree.
    pub fn recover(session: &Session, worktree_id: &str) -> Result<WorktreeInfo> {
        let state = session
            .worktree_lifecycle
            .get(worktree_id)
            .context("worktree is not recorded in this session")?;
        if matches!(state.state.as_str(), "discarded" | "merged") {
            anyhow::bail!("worktree is already {}", state.state);
        }
        let detail = state
            .detail
            .as_deref()
            .context("worktree lifecycle is missing recovery metadata")?;
        serde_json::from_str(detail).context("invalid worktree recovery metadata")
    }

    fn validate_info(&self, info: &WorktreeInfo) -> Result<()> {
        let expected_path = self.worktree_root.join(&info.worktree_id);
        if info.path != expected_path
            || info.worktree_id.contains('/')
            || info.worktree_id.contains("..")
            || info.branch != format!("vera-agent-{}", info.worktree_id)
        {
            anyhow::bail!("invalid Vera worktree metadata");
        }
        Ok(())
    }

    fn validate_existing_path(&self, info: &WorktreeInfo) -> Result<()> {
        let guard = PathGuard::new(self.repository.clone())?;
        if guard.resolve(&info.path)? != info.path {
            anyhow::bail!("worktree path is not safely inside the repository");
        }
        Ok(())
    }

    async fn git(&self, args: &[&str], cwd: &Path) -> Result<crate::safety::CommandOutput> {
        let args = git_args(args, cwd, &self.repository, self.sandboxed);
        let output = if self.sandboxed {
            Sandbox::run(
                "git",
                &args,
                &self.repository,
                false,
                Duration::from_secs(30),
            )
            .await?
        } else {
            let output = Command::new("git")
                .args(&args)
                .env_clear()
                .current_dir(cwd)
                .output()
                .await
                .context("run fixture git command")?;
            crate::safety::CommandOutput {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        };
        if output.status != 0 {
            anyhow::bail!("git {:?} failed: {}", args, output.stderr);
        }
        Ok(output)
    }

    async fn git_mutating(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> Result<crate::safety::CommandOutput> {
        let args = git_args(args, cwd, &self.repository, self.sandboxed);
        let output = if self.sandboxed {
            tokio::time::timeout(
                Duration::from_secs(30),
                Sandbox::command("git", &args, &self.repository, false).output(),
            )
            .await
            .context("git worktree mutation timed out")??
        } else {
            Command::new("git")
                .args(&args)
                .env_clear()
                .current_dir(cwd)
                .output()
                .await
                .context("run fixture git command")?
        };
        let output = crate::safety::CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        if output.status != 0 {
            anyhow::bail!("git {:?} failed: {}", args, output.stderr);
        }
        Ok(output)
    }

    async fn git_allow_failure(
        &self,
        args: &[&str],
        cwd: &Path,
    ) -> Result<crate::safety::CommandOutput> {
        let args = git_args(args, cwd, &self.repository, self.sandboxed);
        if self.sandboxed {
            let output = tokio::time::timeout(
                Duration::from_secs(30),
                Sandbox::command("git", &args, &self.repository, false).output(),
            )
            .await
            .context("git apply timed out")??;
            Ok(crate::safety::CommandOutput {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        } else {
            let output = Command::new("git")
                .args(&args)
                .env_clear()
                .current_dir(cwd)
                .output()
                .await
                .context("run fixture git command")?;
            Ok(crate::safety::CommandOutput {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    async fn git_bytes(&self, args: &[&str], cwd: &Path) -> Result<Vec<u8>> {
        let args = git_args(args, cwd, &self.repository, self.sandboxed);
        let output = if self.sandboxed {
            Sandbox::command("git", &args, &self.repository, false)
                .output()
                .await
                .context("run git diff")?
        } else {
            Command::new("git")
                .args(&args)
                .env_clear()
                .current_dir(cwd)
                .output()
                .await
                .context("run fixture git diff")?
        };
        if !output.status.success() {
            anyhow::bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output.stdout)
    }

    async fn git_paths(&self, args: &[&str], cwd: &Path) -> Result<Vec<String>> {
        let mut owned = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let insert_at = owned
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(owned.len());
        owned.insert(insert_at, "-z".into());
        let refs = owned.iter().map(String::as_str).collect::<Vec<_>>();
        let bytes = self.git_bytes(&refs, cwd).await?;
        bytes
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).context("git returned a non-UTF-8 path"))
            .collect()
    }
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("worktree merge target has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".vera-merge-{}", Uuid::new_v4().simple()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn worktree_root_for(repository: &Path) -> PathBuf {
    let dot_git = repository.join(".git");
    if dot_git.is_dir() {
        dot_git.join("vera-worktrees")
    } else {
        repository.join(".vera/worktrees")
    }
}

fn git_args(args: &[&str], cwd: &Path, repository: &Path, sandboxed: bool) -> Vec<String> {
    let mut result = Vec::new();
    if sandboxed && cwd != repository {
        result.push("-C".into());
        result.push(cwd.display().to_string());
    }
    result.extend(args.iter().map(|arg| (*arg).to_owned()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::VeraPaths;
    use crate::sessions::{CapabilitySelection, SessionStore};
    use std::process::Command;

    #[test]
    fn rejects_user_controlled_worktree_metadata() {
        let root = tempfile::tempdir().unwrap();
        let manager = WorktreeManager::new(root.path().to_path_buf()).unwrap();
        let invalid = WorktreeInfo {
            worktree_id: "../escape".into(),
            path: root.path().join(".vera/worktrees/escape"),
            branch: "vera-agent-../escape".into(),
            base_revision: "HEAD".into(),
        };
        assert!(manager.validate_info(&invalid).is_err());
    }

    #[test]
    fn accepts_only_generated_worktree_shape() {
        let root = tempfile::tempdir().unwrap();
        let manager = WorktreeManager::new(root.path().to_path_buf()).unwrap();
        let id = Uuid::new_v4().simple().to_string();
        let valid = WorktreeInfo {
            worktree_id: id.clone(),
            path: manager.worktree_root.join(&id),
            branch: format!("vera-agent-{id}"),
            base_revision: "0123456789abcdef".into(),
        };
        assert!(manager.validate_info(&valid).is_ok());
    }

    #[test]
    fn rejects_worktree_path_prefix_injection() {
        let root = tempfile::tempdir().unwrap();
        let manager = WorktreeManager::new(root.path().to_path_buf()).unwrap();
        let id = Uuid::new_v4().simple().to_string();
        let invalid = WorktreeInfo {
            worktree_id: id.clone(),
            path: manager.worktree_root.join(&id).join(".."),
            branch: format!("vera-agent-{id}"),
            base_revision: "0123456789abcdef".into(),
        };
        assert!(manager.validate_info(&invalid).is_err());
    }

    #[tokio::test]
    async fn reviews_and_applies_a_clean_worktree_diff_with_journaled_preimage() {
        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&repository)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(repository.join("fixture.txt"), "before\n").unwrap();
        git(&["add", "fixture.txt"]);
        git(&[
            "-c",
            "user.name=Vera Fixture",
            "-c",
            "user.email=vera-fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);

        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let mut session = SessionStore::new(paths)
            .create_with_selection(
                repository.clone(),
                CapabilitySelection {
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    model_context_window: 10_000,
                    ..CapabilitySelection::default()
                },
            )
            .unwrap();
        let manager = WorktreeManager::for_fixture(repository.clone()).unwrap();
        let info = manager.create(&mut session).await.unwrap();
        assert_eq!(
            WorktreeManager::recover(&session, &info.worktree_id)
                .unwrap()
                .path,
            info.path
        );
        let lifecycle_detail = session
            .worktree_lifecycle
            .get(&info.worktree_id)
            .and_then(|state| state.detail.as_deref())
            .and_then(|detail| serde_json::from_str::<WorktreeInfo>(detail).ok())
            .unwrap();
        assert_eq!(lifecycle_detail.base_revision, info.base_revision);
        assert_eq!(lifecycle_detail.branch, info.branch);
        std::fs::write(info.path.join("fixture.txt"), "after\n").unwrap();
        std::fs::write(info.path.join("new.txt"), "created by child\n").unwrap();
        std::fs::write(info.path.join("file..txt"), "合法 filename\n").unwrap();
        std::fs::write(repository.join("fixture.txt"), "parent edit\n").unwrap();
        assert!(manager.apply_review(&info, &mut session).await.is_err());
        std::fs::write(repository.join("fixture.txt"), "before\n").unwrap();
        let review = manager.review(&info).await.unwrap();
        assert!(review.changed_paths.contains(&"fixture.txt".to_owned()));
        assert!(review.changed_paths.contains(&"new.txt".to_owned()));
        assert!(review.changed_paths.contains(&"file..txt".to_owned()));
        manager.apply_review(&info, &mut session).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(repository.join("fixture.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("new.txt")).unwrap(),
            "created by child\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("file..txt")).unwrap(),
            "合法 filename\n"
        );
        manager.discard(&info, &mut session).await.unwrap();
        assert!(!info.path.exists());
        assert!(session.merge_decisions.contains_key(&info.worktree_id));

        let oversized = manager.create(&mut session).await.unwrap();
        std::fs::write(
            oversized.path.join("oversized.bin"),
            vec![b'x'; MAX_WORKTREE_DIFF_BYTES + 1],
        )
        .unwrap();
        assert!(
            manager
                .apply_review(&oversized, &mut session)
                .await
                .is_err()
        );
        assert_eq!(
            session.merge_decisions.get(&oversized.worktree_id),
            Some(&"conflict".to_owned())
        );
        manager.discard(&oversized, &mut session).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_worktree_diff_that_replaces_a_repository_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repo");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "keep me\n").unwrap();
        symlink(outside.join("secret"), repository.join("escape.txt")).unwrap();

        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&repository)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["add", "escape.txt"]);
        git(&[
            "-c",
            "user.name=Vera Fixture",
            "-c",
            "user.email=vera-fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ]);

        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let mut session = SessionStore::new(paths)
            .create_with_selection(
                repository.clone(),
                CapabilitySelection {
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    model_context_window: 10_000,
                    ..CapabilitySelection::default()
                },
            )
            .unwrap();
        let manager = WorktreeManager::for_fixture(repository).unwrap();
        let info = manager.create(&mut session).await.unwrap();
        std::fs::remove_file(info.path.join("escape.txt")).unwrap();
        std::fs::write(info.path.join("escape.txt"), "do not follow\n").unwrap();

        assert!(manager.apply_review(&info, &mut session).await.is_err());
        assert_eq!(
            std::fs::read_to_string(outside.join("secret")).unwrap(),
            "keep me\n"
        );
        manager.discard(&info, &mut session).await.unwrap();
    }
}
