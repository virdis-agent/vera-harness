use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::safety::Sandbox;

const OUTPUT_LIMIT: usize = 256 * 1024;
const MAX_LOG_BYTES: usize = 64 * 1024;
const MAX_ENV_VARS: usize = 64;
const MAX_ENV_VALUE_BYTES: usize = 8 * 1024;
const MAX_ENV_TOTAL_BYTES: usize = 64 * 1024;

/// The process manager uses the vetted PTY backend for interactive sessions.
/// Lifecycle and bounded-output behavior remains independent of the platform
/// PTY implementation.
pub fn pty_backend_name() -> &'static str {
    "portable-pty-0.9"
}

#[derive(Clone, Debug)]
pub struct ProcessStartRequest {
    pub command: String,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub network: bool,
    pub columns: u16,
    pub rows: u16,
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessSnapshot {
    pub process_id: String,
    pub session_id: String,
    pub command: String,
    pub status: String,
    pub cursor: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub cursor: u64,
    pub next_cursor: u64,
    pub output: String,
    pub truncated: bool,
}

struct OutputBuffer {
    chunks: VecDeque<(u64, String)>,
    bytes: usize,
    next_cursor: u64,
}

impl OutputBuffer {
    fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            next_cursor: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(bytes).into_owned();
        let cursor = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(text.len() as u64);
        self.bytes = self.bytes.saturating_add(text.len());
        self.chunks.push_back((cursor, text));
        while self.bytes > OUTPUT_LIMIT {
            if let Some((_, removed)) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(removed.len());
            } else {
                break;
            }
        }
    }

    fn since(&self, cursor: u64, limit: usize) -> ProcessOutput {
        let oldest = self
            .chunks
            .front()
            .map(|(at, _)| *at)
            .unwrap_or(self.next_cursor);
        let truncated = cursor < oldest;
        let start = cursor.max(oldest);
        let mut output = String::new();
        if truncated {
            output.push_str("[output truncated]\n");
        }
        for (at, text) in &self.chunks {
            if *at + text.len() as u64 <= start {
                continue;
            }
            let offset = start.saturating_sub(*at) as usize;
            let mut offset = offset.min(text.len());
            while offset < text.len() && !text.is_char_boundary(offset) {
                offset += 1;
            }
            output.push_str(&text[offset..]);
            if output.len() >= limit {
                truncate_utf8(&mut output, limit);
                break;
            }
        }
        ProcessOutput {
            cursor,
            next_cursor: self.next_cursor,
            output,
            truncated,
        }
    }

    fn page(&self, offset: usize, limit: usize) -> ProcessOutput {
        let all = self
            .chunks
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>();
        let mut start = offset.min(all.len());
        while start < all.len() && !all.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + limit.min(MAX_LOG_BYTES)).min(all.len());
        while end > start && !all.is_char_boundary(end) {
            end -= 1;
        }
        ProcessOutput {
            cursor: start as u64,
            next_cursor: end as u64,
            output: all[start..end].to_owned(),
            truncated: end < all.len(),
        }
    }
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

type PtyChild = Box<dyn portable_pty::Child + Send + Sync>;
type PtyWriter = Box<dyn Write + Send>;

struct ManagedProcess {
    snapshot: ProcessSnapshot,
    child: Arc<StdMutex<PtyChild>>,
    writer: Arc<StdMutex<Option<PtyWriter>>>,
    output: Arc<Mutex<OutputBuffer>>,
    #[cfg(unix)]
    process_group: Option<libc::pid_t>,
}

#[derive(Clone)]
pub struct ProcessManager {
    processes: Arc<Mutex<BTreeMap<String, Arc<ManagedProcess>>>>,
    sandboxed: bool,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            processes: Arc::new(Mutex::new(BTreeMap::new())),
            sandboxed: true,
        }
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn for_fixture() -> Self {
        Self {
            processes: Arc::new(Mutex::new(BTreeMap::new())),
            sandboxed: false,
        }
    }

    pub async fn start(&self, request: ProcessStartRequest) -> Result<ProcessSnapshot> {
        if request.command.trim().is_empty() {
            anyhow::bail!("process command must not be empty");
        }
        if !request.cwd.is_dir() {
            anyhow::bail!("process cwd is not a directory");
        }
        if request.columns == 0 || request.rows == 0 {
            anyhow::bail!("process PTY size must be non-zero");
        }
        if request.environment.len() > MAX_ENV_VARS {
            anyhow::bail!("process environment has too many variables");
        }
        let mut environment_bytes: usize = 0;
        let args = vec!["-lc".to_owned(), request.command.clone()];
        let mut command = if self.sandboxed {
            Sandbox::pty_command("/bin/zsh", &args, &request.cwd, request.network)
        } else {
            let mut command = portable_pty::CommandBuilder::new("/bin/zsh");
            command.args(&args);
            command.env_clear();
            command.env("PATH", "/usr/bin:/bin");
            command.cwd(&request.cwd);
            command
        };
        for (key, value) in &request.environment {
            validate_environment_key(key)?;
            if value.len() > MAX_ENV_VALUE_BYTES {
                anyhow::bail!("environment value for {key:?} is too large");
            }
            environment_bytes = environment_bytes.saturating_add(key.len() + value.len());
            if environment_bytes > MAX_ENV_TOTAL_BYTES {
                anyhow::bail!("process environment is too large");
            }
            command.env(key, value);
        }

        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: request.rows,
                cols: request.columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open process PTY")?;
        let child = pty
            .slave
            .spawn_command(command)
            .context("start background process")?;
        #[cfg(unix)]
        let process_group = {
            let child_pid = child.process_id().map(|pid| pid as libc::pid_t);
            pty.master
                .process_group_leader()
                .filter(|group| child_pid.is_none_or(|pid| *group == pid))
        };
        let mut reader = pty
            .master
            .try_clone_reader()
            .context("clone process PTY reader")?;
        let writer = pty
            .master
            .take_writer()
            .context("take process PTY writer")?;

        let process_id = Uuid::new_v4().simple().to_string();
        let managed = Arc::new(ManagedProcess {
            snapshot: ProcessSnapshot {
                process_id: process_id.clone(),
                session_id: request.session_id,
                command: request.command,
                status: "running".into(),
                cursor: 0,
            },
            child: Arc::new(StdMutex::new(child)),
            writer: Arc::new(StdMutex::new(Some(writer))),
            output: Arc::new(Mutex::new(OutputBuffer::new())),
            #[cfg(unix)]
            process_group,
        });

        let output = managed.output.clone();
        std::thread::Builder::new()
            .name(format!("vera-process-{process_id}"))
            .spawn(move || {
                let mut bytes = [0_u8; 8 * 1024];
                loop {
                    match reader.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(size) => output.blocking_lock().push(&bytes[..size]),
                    }
                }
            })
            .context("spawn PTY reader")?;

        self.processes
            .lock()
            .await
            .insert(process_id, managed.clone());
        Ok(managed.snapshot.clone())
    }

    pub async fn list(&self, session_id: Option<&str>) -> Vec<ProcessSnapshot> {
        let processes = self.processes.lock().await;
        let mut snapshots = Vec::new();
        for process in processes.values() {
            if session_id.is_none_or(|id| id == process.snapshot.session_id) {
                let status = {
                    let mut child = match process.child.lock() {
                        Ok(child) => child,
                        Err(_) => continue,
                    };
                    match child.try_wait() {
                        Ok(Some(status)) => format!("exited:{}", status.exit_code()),
                        Ok(None) => "running".into(),
                        Err(_) => "unknown".into(),
                    }
                };
                let mut snapshot = process.snapshot.clone();
                snapshot.status = status;
                snapshot.cursor = process.output.lock().await.next_cursor;
                snapshots.push(snapshot);
            }
        }
        snapshots
    }

    pub async fn poll(&self, process_id: &str, cursor: u64) -> Result<ProcessOutput> {
        let process = self.get(process_id).await?;
        Ok(process.output.lock().await.since(cursor, MAX_LOG_BYTES))
    }

    pub async fn poll_for_session(
        &self,
        process_id: &str,
        cursor: u64,
        session_id: &str,
    ) -> Result<ProcessOutput> {
        let process = self.get_owned(process_id, session_id).await?;
        Ok(process.output.lock().await.since(cursor, MAX_LOG_BYTES))
    }

    pub async fn log(
        &self,
        process_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ProcessOutput> {
        let process = self.get(process_id).await?;
        Ok(process.output.lock().await.page(offset, limit))
    }

    pub async fn log_for_session(
        &self,
        process_id: &str,
        offset: usize,
        limit: usize,
        session_id: &str,
    ) -> Result<ProcessOutput> {
        let process = self.get_owned(process_id, session_id).await?;
        Ok(process.output.lock().await.page(offset, limit))
    }

    pub async fn write(&self, process_id: &str, input: &[u8]) -> Result<()> {
        if input.len() > 16 * 1024 {
            anyhow::bail!("process input exceeds 16 KiB");
        }
        let process = self.get(process_id).await?;
        let mut writer = process
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("process writer lock poisoned"))?;
        let writer = writer.as_mut().context("process PTY writer is closed")?;
        writer.write_all(input)?;
        writer.flush()?;
        Ok(())
    }

    pub async fn write_for_session(
        &self,
        process_id: &str,
        input: &[u8],
        session_id: &str,
    ) -> Result<()> {
        if input.len() > 16 * 1024 {
            anyhow::bail!("process input exceeds 16 KiB");
        }
        let process = self.get_owned(process_id, session_id).await?;
        let mut writer = process
            .writer
            .lock()
            .map_err(|_| anyhow::anyhow!("process writer lock poisoned"))?;
        let writer = writer.as_mut().context("process PTY writer is closed")?;
        writer.write_all(input)?;
        writer.flush()?;
        Ok(())
    }

    pub async fn wait(&self, process_id: &str) -> Result<i32> {
        let process = self.get(process_id).await?;
        let child = process.child.clone();
        let status = tokio::task::spawn_blocking(move || {
            child
                .lock()
                .map_err(|_| std::io::Error::other("process child lock poisoned"))?
                .wait()
        })
        .await
        .context("join process wait")??;
        Ok(status.exit_code() as i32)
    }

    pub async fn wait_for_session(&self, process_id: &str, session_id: &str) -> Result<i32> {
        let process = self.get_owned(process_id, session_id).await?;
        let child = process.child.clone();
        let status = tokio::task::spawn_blocking(move || {
            child
                .lock()
                .map_err(|_| std::io::Error::other("process child lock poisoned"))?
                .wait()
        })
        .await
        .context("join process wait")??;
        Ok(status.exit_code() as i32)
    }

    pub async fn kill(&self, process_id: &str) -> Result<()> {
        let process = self.get(process_id).await?;
        kill_managed_process(&process)?;
        Ok(())
    }

    pub async fn kill_for_session(&self, process_id: &str, session_id: &str) -> Result<()> {
        let process = self.get_owned(process_id, session_id).await?;
        kill_managed_process(&process)?;
        Ok(())
    }

    pub async fn shutdown_session(&self, session_id: &str) {
        let processes = self.processes.lock().await;
        for process in processes.values() {
            if process.snapshot.session_id == session_id
                && let Ok(mut child) = process.child.lock()
            {
                #[cfg(unix)]
                if let Some(group) = process.process_group {
                    let _ = unsafe { libc::kill(-group, libc::SIGHUP) };
                } else {
                    let _ = child.kill();
                }
                #[cfg(not(unix))]
                let _ = child.kill();
            }
        }
    }

    async fn get(&self, process_id: &str) -> Result<Arc<ManagedProcess>> {
        self.processes
            .lock()
            .await
            .get(process_id)
            .cloned()
            .context("process not found")
    }

    async fn get_owned(&self, process_id: &str, session_id: &str) -> Result<Arc<ManagedProcess>> {
        let process = self.get(process_id).await?;
        if process.snapshot.session_id != session_id {
            anyhow::bail!("process is not attached to this session");
        }
        Ok(process)
    }
}

fn kill_managed_process(process: &ManagedProcess) -> Result<()> {
    #[cfg(unix)]
    if let Some(group) = process.process_group
        && unsafe { libc::kill(-group, libc::SIGHUP) } == 0
    {
        return Ok(());
    }
    process
        .child
        .lock()
        .map_err(|_| anyhow::anyhow!("process child lock poisoned"))?
        .kill()?;
    Ok(())
}

fn validate_environment_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || [
            "HOME",
            "SHELL",
            "USER",
            "PATH",
            "TMPDIR",
            "TERM",
            "LANG",
            "LC_ALL",
            "VERA_AUTH",
            "OPENAI_API_KEY",
            "XAI_API_KEY",
        ]
        .contains(&key)
        || key.contains("TOKEN")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
        || key.contains("CREDENTIAL")
    {
        anyhow::bail!("environment key {key:?} is not allowed");
    }
    Ok(())
}

pub fn canonical_process_cwd(root: &Path, cwd: Option<&Path>) -> Result<PathBuf> {
    let candidate = cwd.unwrap_or(root);
    let path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.starts_with(root) {
        anyhow::bail!("process cwd escapes repository root");
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_process_output_is_pollable_and_killable() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::for_fixture();
        let snapshot = manager
            .start(ProcessStartRequest {
                command: "echo ready; sleep 10".into(),
                cwd: temp.path().to_path_buf(),
                environment: BTreeMap::new(),
                network: false,
                columns: 120,
                rows: 40,
                session_id: "test".into(),
            })
            .await
            .unwrap();
        let mut output = manager.poll(&snapshot.process_id, 0).await.unwrap();
        for _ in 0..100 {
            if output.output.contains("ready") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            output = manager.poll(&snapshot.process_id, 0).await.unwrap();
        }
        assert!(output.output.contains("ready"));
        assert!(
            manager
                .poll_for_session(&snapshot.process_id, 0, "other-session")
                .await
                .is_err()
        );
        manager.kill(&snapshot.process_id).await.unwrap();
    }

    #[test]
    fn rejects_credential_environment_keys() {
        assert!(validate_environment_key("OPENAI_API_KEY").is_err());
        assert!(validate_environment_key("SESSION_TOKEN").is_err());
        assert!(validate_environment_key("SAFE_VALUE").is_ok());
    }

    #[test]
    fn rejects_oversized_environment_values() {
        let temp = tempfile::tempdir().unwrap();
        let manager = ProcessManager::for_fixture();
        let mut environment = BTreeMap::new();
        environment.insert("SAFE_VALUE".into(), "x".repeat(MAX_ENV_VALUE_BYTES + 1));
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(manager.start(ProcessStartRequest {
                command: "true".into(),
                cwd: temp.path().to_path_buf(),
                environment,
                network: false,
                columns: 80,
                rows: 24,
                session_id: "test".into(),
            }));
        assert!(result.is_err());
    }

    #[test]
    fn output_ring_marks_flooding_and_preserves_utf8_boundaries() {
        let mut buffer = OutputBuffer::new();
        buffer.push(&vec![b'x'; OUTPUT_LIMIT + 10]);
        buffer.push("é".as_bytes());
        let output = buffer.since(0, MAX_LOG_BYTES);
        assert!(output.truncated);
        assert!(output.output.starts_with("[output truncated]"));
        assert!(std::str::from_utf8(output.output.as_bytes()).is_ok());
    }
}
