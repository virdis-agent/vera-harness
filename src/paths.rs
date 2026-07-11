use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::error::VeraError;

#[derive(Clone, Debug)]
pub struct VeraPaths {
    pub home: PathBuf,
    pub root: PathBuf,
    pub auth_file: PathBuf,
    pub auth_lock: PathBuf,
    pub sessions: PathBuf,
    pub plugins: PathBuf,
    pub skills: PathBuf,
    pub prompts: PathBuf,
    pub roles: PathBuf,
    pub logs: PathBuf,
}

impl VeraPaths {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().context("could not determine home directory")?;
        Self::from_home(home)
    }

    pub fn from_home(home: PathBuf) -> Result<Self> {
        let root = home.join(".vera");
        Ok(Self {
            home,
            auth_file: root.join("auth.json"),
            auth_lock: root.join("auth.lock"),
            sessions: root.join("sessions"),
            plugins: root.join("plugins"),
            skills: root.join("skills"),
            prompts: root.join("prompts"),
            roles: root.join("roles"),
            logs: root.join("logs"),
            root,
        })
    }

    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.sessions)?;
        fs::create_dir_all(&self.plugins)?;
        fs::create_dir_all(&self.skills)?;
        fs::create_dir_all(&self.prompts)?;
        fs::create_dir_all(&self.roles)?;
        fs::create_dir_all(&self.logs)?;
        set_mode(&self.root, 0o700)?;
        set_mode(&self.sessions, 0o700)?;
        set_mode(&self.plugins, 0o700)?;
        set_mode(&self.skills, 0o700)?;
        Ok(())
    }

    pub fn session_file(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.jsonl"))
    }
}

pub fn repository_root(start: &Path) -> Result<PathBuf> {
    let start = fs::canonicalize(start)?;
    let mut current = if start.is_file() {
        start.parent().unwrap_or(&start).to_path_buf()
    } else {
        start.clone()
    };
    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Ok(start);
        }
    }
}

pub fn safe_join(root: &Path, requested: &Path) -> Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let existing_parent = candidate
        .parent()
        .map(fs::canonicalize)
        .transpose()?
        .unwrap_or_else(|| root.clone());
    let normalized = existing_parent.join(candidate.file_name().unwrap_or_default());
    if !normalized.starts_with(&root) {
        return Err(VeraError::UnsafePath(candidate).into());
    }
    if normalized.exists() && normalized.is_symlink() {
        return Err(VeraError::UnsafePath(normalized).into());
    }
    Ok(normalized)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

pub fn set_private_file(path: &Path) -> Result<()> {
    set_mode(path, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        assert!(safe_join(temp.path(), Path::new("../outside")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path().join("secret"), temp.path().join("link")).unwrap();
        assert!(safe_join(temp.path(), Path::new("link")).is_err());
    }
}
