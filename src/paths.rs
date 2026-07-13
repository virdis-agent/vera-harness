use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
use uuid::Uuid;

use crate::error::VeraError;
use crate::safety::PathGuard;

const DEFAULT_CONFIG_TOML: &str = r#"# Vera's global base configuration. Interactive overrides are kept in
# .vera-global-state.json so upgrades never rewrite this file.
version = 2
provider = "openai-codex"
# model = "<model-id>"
# effort = "high"
shell_timeout_seconds = 120
context_window_tokens = 128000
hooks = []
trusted_plugins = []
enabled_plugins = []
enabled_mcp = []
allowed_skills = []
prompt_roots = []
browser_cdp_endpoints = []
# role = "reviewer"
# hooks = ["./scripts/vera-hook.zsh"]

# Permission rules are ordered by effect precedence and use structured TOML.
# [[permission_rules]]
# effect = "deny"
# [permission_rules.matcher]
# permission_kind = "shell"
# command_prefix = "git reset"

[approval]
auto_read = true
writes = "once"
shell = "once"
network = "once"
"#;

#[derive(Clone, Debug)]
pub struct VeraPaths {
    pub home: PathBuf,
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub auth_file: PathBuf,
    pub auth_lock: PathBuf,
    pub version_file: PathBuf,
    pub installation_id: PathBuf,
    pub global_state: PathBuf,
    pub keybindings: PathBuf,
    pub history: PathBuf,
    pub sessions: PathBuf,
    pub archived_sessions: PathBuf,
    pub attachments: PathBuf,
    pub plugins: PathBuf,
    pub plugin_cache: PathBuf,
    pub skills: PathBuf,
    pub system_skills: PathBuf,
    pub cache: PathBuf,
    pub shell_snapshots: PathBuf,
    pub rules: PathBuf,
    pub vendor_imports: PathBuf,
    pub dot_tmp: PathBuf,
    pub tmp: PathBuf,
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
            config_file: root.join("config.toml"),
            auth_file: root.join("auth.json"),
            auth_lock: root.join("auth.lock"),
            version_file: root.join("version.json"),
            installation_id: root.join("installation_id"),
            global_state: root.join(".vera-global-state.json"),
            keybindings: root.join("keybindings.json"),
            history: root.join("history.jsonl"),
            sessions: root.join("sessions"),
            archived_sessions: root.join("archived_sessions"),
            attachments: root.join("attachments"),
            plugins: root.join("plugins"),
            plugin_cache: root.join("plugins/cache"),
            skills: root.join("skills"),
            system_skills: root.join("skills/.system"),
            cache: root.join("cache"),
            shell_snapshots: root.join("shell_snapshots"),
            rules: root.join("rules"),
            vendor_imports: root.join("vendor_imports"),
            dot_tmp: root.join(".tmp"),
            tmp: root.join("tmp"),
            prompts: root.join("prompts"),
            roles: root.join("roles"),
            logs: root.join("logs"),
            root,
        })
    }

    pub fn ensure_runtime_dirs(&self) -> Result<()> {
        ensure_private_dir(&self.root)?;
        for directory in [
            &self.sessions,
            &self.archived_sessions,
            &self.attachments,
            &self.plugins,
            &self.plugin_cache,
            &self.skills,
            &self.system_skills,
            &self.cache,
            &self.shell_snapshots,
            &self.rules,
            &self.vendor_imports,
            &self.dot_tmp,
            &self.tmp,
            &self.prompts,
            &self.roles,
            &self.logs,
        ] {
            ensure_private_dir(directory)?;
        }
        let guard = PathGuard::new(self.root.clone())?;
        ensure_installation_id(&guard, &self.installation_id)?;
        self.record_version(env!("CARGO_PKG_VERSION"))?;
        self.ensure_user_files()?;
        Ok(())
    }

    pub fn ensure_user_files(&self) -> Result<()> {
        ensure_user_file(&self.config_file, DEFAULT_CONFIG_TOML.as_bytes())?;
        ensure_user_file(
            &self.global_state,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&crate::settings::GlobalState::default())?
            )
            .as_bytes(),
        )?;
        ensure_user_file(
            &self.keybindings,
            crate::keybindings::KeyMap::default_json().as_bytes(),
        )?;
        Ok(())
    }

    pub fn record_version(&self, version: &str) -> Result<()> {
        let guard = PathGuard::new(self.root.clone())?;
        let target = guard.resolve(&self.version_file)?;
        validate_managed_file_or_missing(&target)?;
        let mut contents = serde_json::to_vec_pretty(&json!({ "version": version }))?;
        contents.push(b'\n');
        if fs::read(&target).is_ok_and(|existing| existing == contents) {
            set_private_file(&target)?;
            return Ok(());
        }
        atomic_replace(&guard, &target, &contents)
    }

    pub fn session_file(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.jsonl"))
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(VeraError::UnsafePath(path.to_path_buf()).into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error.into()),
    }
    set_mode(path, 0o700)
}

fn ensure_installation_id(guard: &PathGuard, target: &Path) -> Result<()> {
    let target = guard.resolve(target)?;
    if validate_managed_file_or_missing(&target)? {
        set_private_file(&target)?;
        return Ok(());
    }
    let contents = format!("{}\n", Uuid::new_v4());
    atomic_create(guard, &target, contents.as_bytes())
}

fn atomic_create(guard: &PathGuard, target: &Path, contents: &[u8]) -> Result<()> {
    let temporary = guarded_temporary(guard, target)?;
    write_new_private_file(&temporary, contents)?;
    let linked = match fs::hard_link(&temporary, target) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    };
    fs::remove_file(&temporary)?;
    if !linked {
        validate_managed_file_or_missing(target)?;
    }
    set_private_file(target)
}

/// Returns true when the path names a regular file and false when it is absent.
fn validate_managed_file_or_missing(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(VeraError::UnsafePath(path.to_path_buf()).into())
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_user_file(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(VeraError::UnsafePath(path.to_path_buf()).into());
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    set_private_file(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_replace(guard: &PathGuard, target: &Path, contents: &[u8]) -> Result<()> {
    let temporary = guarded_temporary(guard, target)?;
    write_new_private_file(&temporary, contents)?;
    if let Err(error) = fs::rename(&temporary, target) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    set_private_file(target)
}

fn guarded_temporary(guard: &PathGuard, target: &Path) -> Result<PathBuf> {
    let name = target
        .file_name()
        .context("managed metadata path has no file name")?
        .to_string_lossy();
    guard.resolve(&target.with_file_name(format!(".{name}.vera-tmp-{}", Uuid::new_v4().simple())))
}

fn write_new_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    set_private_file(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn runtime_layout_is_complete_idempotent_and_bootstraps_settings_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();

        paths.ensure_runtime_dirs().unwrap();
        crate::keybindings::KeyMap::load(&paths.keybindings).unwrap();
        crate::config::Config::load(&paths, temp.path()).unwrap();
        let installation_id = fs::read_to_string(&paths.installation_id).unwrap();
        assert!(Uuid::parse_str(installation_id.trim()).is_ok());
        let version: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.version_file).unwrap()).unwrap();
        assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));

        for directory in [
            &paths.sessions,
            &paths.archived_sessions,
            &paths.attachments,
            &paths.plugins,
            &paths.plugin_cache,
            &paths.skills,
            &paths.system_skills,
            &paths.cache,
            &paths.shell_snapshots,
            &paths.rules,
            &paths.vendor_imports,
            &paths.dot_tmp,
            &paths.tmp,
            &paths.prompts,
            &paths.roles,
            &paths.logs,
        ] {
            assert!(
                directory.is_dir(),
                "{} was not created",
                directory.display()
            );
        }
        for user_file in [&paths.config_file, &paths.global_state, &paths.keybindings] {
            assert!(
                user_file.exists(),
                "{} should be created on first run",
                user_file.display()
            );
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(user_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!paths.auth_file.exists());
        assert!(!paths.history.exists());

        paths.ensure_runtime_dirs().unwrap();
        assert_eq!(
            fs::read_to_string(&paths.installation_id).unwrap(),
            installation_id
        );
    }

    #[test]
    fn records_new_versions_without_replacing_installation_identity() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        let installation_id = fs::read(&paths.installation_id).unwrap();

        paths.record_version("9.8.7").unwrap();

        let version: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.version_file).unwrap()).unwrap();
        assert_eq!(version["version"], "9.8.7");
        assert_eq!(fs::read(&paths.installation_id).unwrap(), installation_id);
    }

    #[test]
    fn bootstrap_preserves_existing_user_files_byte_for_byte() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        let config = b"# user config\nprovider = \"fixture\"\n";
        let state = b"{\n  \"version\": 1,\n  \"config\": {\"provider\": \"fixture\"}\n}\n";
        let keybindings = b"[{\"command\":\"submit\",\"key\":\"Enter\"},{\"command\":\"cancel\",\"key\":\"Esc\"}]\n";
        fs::write(&paths.config_file, config).unwrap();
        fs::write(&paths.global_state, state).unwrap();
        fs::write(&paths.keybindings, keybindings).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        assert_eq!(fs::read(&paths.config_file).unwrap(), config);
        assert_eq!(fs::read(&paths.global_state).unwrap(), state);
        assert_eq!(fs::read(&paths.keybindings).unwrap(), keybindings);
    }

    #[test]
    fn version_metadata_updates_leave_settings_and_identity_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        let config = fs::read(&paths.config_file).unwrap();
        let state = fs::read(&paths.global_state).unwrap();
        let keybindings = fs::read(&paths.keybindings).unwrap();
        let identity = fs::read(&paths.installation_id).unwrap();
        paths.record_version("99.1.0").unwrap();
        assert_eq!(fs::read(&paths.config_file).unwrap(), config);
        assert_eq!(fs::read(&paths.global_state).unwrap(), state);
        assert_eq!(fs::read(&paths.keybindings).unwrap(), keybindings);
        assert_eq!(fs::read(&paths.installation_id).unwrap(), identity);
        let version: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths.version_file).unwrap()).unwrap();
        assert_eq!(version["version"], "99.1.0");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_layout_rejects_managed_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        fs::create_dir_all(&paths.root).unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &paths.sessions).unwrap();

        assert!(paths.ensure_runtime_dirs().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn version_metadata_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();
        fs::remove_file(&paths.version_file).unwrap();
        let outside = temp.path().join("outside-version.json");
        fs::write(&outside, "do not replace").unwrap();
        symlink(&outside, &paths.version_file).unwrap();

        assert!(paths.record_version("9.8.7").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "do not replace");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_layout_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        paths.ensure_runtime_dirs().unwrap();

        assert_eq!(
            fs::metadata(&paths.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&paths.installation_id)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&paths.version_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

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
