use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::header::LOCATION;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::auth::{AuthProvider, OAuthClient, TokenRecord, TokenStore, now_seconds, pkce_pair};
use crate::cli::{
    AuthCommand, Command, CommandLine, McpCommand, OutputFormat, PermissionsCommand, PluginCommand,
    SessionCommand,
};
use crate::config::Config;
use crate::events::{Event, EventSink, TerminalEventSink};
use crate::extensions::{
    HookEvent, HookRunner, HookSpec, McpRegistry, PluginManager, PluginManifest, PromptCatalog,
    SkillCatalog, discover_agents,
};
use crate::paths::{VeraPaths, repository_root};
use crate::prompt::{approximate_tokens, build_context};
use crate::providers::{
    ModelCatalog, ModelInfo, Provider, ProviderInput, ProviderKind, ProviderRequest,
    ResponsesProvider, cache_catalog, load_cached_models,
};
use crate::safety::{
    ApprovalHandler, PathGuard, PermissionEffect, PermissionKind, PermissionMatcher,
    PermissionMode, PermissionPolicy, PermissionRule, TerminalApproval,
};
use crate::sessions::{CapabilitySelection, SessionStore};
use crate::subagents::InProcessSubagentRunner;
use crate::tools::{ToolCall, ToolContext, ToolRegistry, execute};
use crate::ui::{Dashboard, InputAction, read_input, render_dashboard};

pub async fn run(cli: CommandLine) -> Result<()> {
    let paths = VeraPaths::discover()?;
    let root = repository_root(&cli.path)?;
    let config = Config::load(&paths, &root)?;
    match cli.command {
        Command::Help => print_help(),
        Command::Version => println!("vera {}", env!("CARGO_PKG_VERSION")),
        Command::Auth(command) => run_auth(&paths, command).await?,
        Command::Models { refresh } => run_models(&paths, &config, refresh).await?,
        Command::Session(SessionCommand::List) => run_session_list(&paths)?,
        Command::Session(SessionCommand::Resume { id }) => {
            run_session_resume(&paths, config, id).await?
        }
        Command::Inspect => inspect(&paths, &root, &config)?,
        Command::Mcp(command) => run_mcp(&paths, &root, &config, command).await?,
        Command::Permissions(command) => run_permissions(&config, command)?,
        Command::Plugin(command) => run_plugin(&paths, &config, command).await?,
        Command::Update => run_upgrade(&paths).await?,
        Command::Prompt => {
            let prompt = if let Some(prompt) = cli.prompt {
                prompt
            } else if let Some(name) = cli.prompt_template {
                let plugins = enabled_plugins(&paths, &config, &[])?;
                let catalog = PromptCatalog::load(
                    &paths,
                    &root,
                    &plugins,
                    &configured_prompt_roots(&paths, &root, &config),
                )?;
                catalog.expand(&name, cli.prompt_args.as_deref().unwrap_or(""))?
            } else {
                anyhow::bail!("prompt is required")
            };
            run_headless(
                &paths,
                &root,
                &config,
                cli.provider.as_deref(),
                cli.model.as_deref(),
                cli.effort.as_deref(),
                &prompt,
                cli.output,
            )
            .await?;
        }
        Command::Interactive => {
            run_interactive(
                &paths,
                &root,
                config,
                cli.provider,
                cli.model,
                cli.effort,
                None,
            )
            .await?
        }
    }
    Ok(())
}

const GITHUB_API_ROOT: &str = "https://api.github.com";
const VERA_REPOSITORY: &str = "virdis-agent/vera-harness";

async fn run_upgrade(paths: &VeraPaths) -> Result<()> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    anyhow::bail!("automatic upgrades currently support Apple Silicon macOS only");

    let marker = paths.root.join("installer-version");
    if !marker.exists() {
        anyhow::bail!(
            "this Vera copy is not installer-managed; use `cargo install --path . --locked` or `brew upgrade vera-harness`"
        );
    }
    let current = fs::read_to_string(&marker)?.trim().to_owned();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("vera/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let releases_url = format!("{GITHUB_API_ROOT}/repos/{VERA_REPOSITORY}/releases?per_page=20");
    let response = client.get(&releases_url).send().await?;
    validate_github_response(&response, "api.github.com")?;
    let releases: Vec<serde_json::Value> = response.json().await?;
    let (version, archive_id, checksum_id) = find_release_assets(&releases)?;
    if version == current {
        println!("Vera {current} is already up to date.");
        return Ok(());
    }

    let archive_name = format!("vera-{version}-aarch64-apple-darwin.tar.gz");
    let archive = fetch_release_asset(&client, archive_id).await?;
    let checksums = fetch_release_asset(&client, checksum_id).await?;
    let expected = checksum_for(&checksums, &archive_name)
        .context("release checksum does not contain the arm64 archive")?;
    let actual = sha256_hex(&archive);
    if actual != expected {
        anyhow::bail!("release checksum verification failed");
    }

    let temp_dir = std::env::temp_dir().join(format!("vera-upgrade-{}", Uuid::new_v4()));
    fs::create_dir_all(&temp_dir)?;
    let archive_path = temp_dir.join(&archive_name);
    fs::write(&archive_path, &archive)?;
    let status = tokio::process::Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&temp_dir)
        .env_clear()
        .env("HOME", &paths.home)
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("could not extract the verified Vera release");
    }

    let extracted = temp_dir.join("vera");
    if !extracted.is_file() {
        anyhow::bail!("verified Vera release did not contain a binary");
    }
    let target = paths.home.join(".local/bin/vera");
    if !target.is_file() {
        anyhow::bail!(
            "installer-managed Vera binary was not found at {}",
            target.display()
        );
    }
    let temporary_target = target
        .parent()
        .context("installer binary has no parent directory")?
        .join(format!(".vera-upgrade-{}", Uuid::new_v4()));
    fs::copy(&extracted, &temporary_target)?;
    set_executable(&temporary_target)?;
    fs::rename(&temporary_target, &target)?;
    fs::write(&marker, &version)?;
    println!("Updated Vera {current} → {version}.");
    Ok(())
}

fn find_release_assets(releases: &[serde_json::Value]) -> Result<(String, u64, u64)> {
    for release in releases {
        let Some(tag) = release.get("tag_name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let version = tag.strip_prefix('v').unwrap_or(tag);
        let archive_name = format!("vera-{version}-aarch64-apple-darwin.tar.gz");
        let Some(assets) = release.get("assets").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let archive_id = assets.iter().find_map(|asset| {
            (asset.get("name").and_then(serde_json::Value::as_str) == Some(archive_name.as_str()))
                .then(|| asset.get("id").and_then(serde_json::Value::as_u64))
                .flatten()
        });
        let checksum_id = assets.iter().find_map(|asset| {
            (asset.get("name").and_then(serde_json::Value::as_str) == Some("SHA256SUMS"))
                .then(|| asset.get("id").and_then(serde_json::Value::as_u64))
                .flatten()
        });
        if let (Some(archive_id), Some(checksum_id)) = (archive_id, checksum_id) {
            return Ok((version.to_owned(), archive_id, checksum_id));
        }
    }
    anyhow::bail!("no compatible Vera release assets were found")
}

async fn fetch_release_asset(client: &reqwest::Client, asset_id: u64) -> Result<Vec<u8>> {
    let url = format!("{GITHUB_API_ROOT}/repos/{VERA_REPOSITORY}/releases/assets/{asset_id}");
    let response = client
        .get(&url)
        .header("Accept", "application/octet-stream")
        .send()
        .await?;
    if response.status().is_redirection() {
        let location = response
            .headers()
            .get(LOCATION)
            .context("GitHub asset redirect did not include a location")?
            .to_str()?
            .to_owned();
        let location = reqwest::Url::parse(&location)?;
        if location.scheme() != "https"
            || location.host_str() != Some("release-assets.githubusercontent.com")
        {
            anyhow::bail!("GitHub release asset origin validation failed");
        }
        let redirected = client.get(location).send().await?;
        if !redirected.status().is_success() {
            anyhow::bail!("GitHub release asset returned {}", redirected.status());
        }
        return Ok(redirected.bytes().await?.to_vec());
    }
    validate_github_response(&response, "api.github.com")?;
    Ok(response.bytes().await?.to_vec())
}

fn validate_github_response(response: &reqwest::Response, host: &str) -> Result<()> {
    if response.url().scheme() != "https"
        || response.url().host_str() != Some(host)
        || !response.status().is_success()
    {
        anyhow::bail!("GitHub API origin or response validation failed");
    }
    Ok(())
}

fn checksum_for(checksums: &[u8], filename: &str) -> Option<String> {
    String::from_utf8_lossy(checksums).lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let checksum = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == filename).then(|| checksum.to_owned())
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn set_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

async fn run_auth(paths: &VeraPaths, command: AuthCommand) -> Result<()> {
    let store = TokenStore::new(paths.clone());
    match command {
        AuthCommand::Status => {
            for (provider, valid, expiry) in store.status()? {
                println!(
                    "{}\t{}\t{}",
                    provider.as_str(),
                    if valid { "valid" } else { "expired" },
                    expiry.map_or_else(|| "no expiry".into(), |value| value.to_string())
                );
            }
        }
        AuthCommand::Logout { provider } => {
            store.remove(provider.as_deref().map(AuthProvider::parse).transpose()?)?;
            println!("logged out");
        }
        AuthCommand::Login {
            provider,
            no_browser,
        } => {
            let provider = AuthProvider::parse(&provider)?;
            let oauth = OAuthClient::new()?;
            let (verifier, challenge) = if provider == AuthProvider::XaiOauth {
                let pair = pkce_pair();
                (Some(pair.0), Some(pair.1))
            } else {
                (None, None)
            };
            let device = oauth
                .device_authorize(provider, challenge.as_deref())
                .await?;
            println!(
                "Open {} and enter code {}.",
                device.verification_uri, device.user_code
            );
            if !no_browser {
                let _ = std::process::Command::new("open")
                    .arg(&device.verification_uri)
                    .status();
            }
            let deadline = now_seconds() + device.expires_in as i64;
            let mut last_error = None;
            let mut poll_interval = device.interval.max(2);
            while now_seconds() < deadline {
                match oauth.poll(provider, &device, verifier.as_deref()).await {
                    Ok(token) => {
                        store.put(token)?;
                        println!("login complete");
                        return Ok(());
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if message.contains("slow_down") || message.contains("rate limited") {
                            poll_interval = poll_interval.saturating_add(5).min(60);
                        }
                        last_error = Some(message);
                    }
                }
                sleep(Duration::from_secs(poll_interval)).await;
            }
            anyhow::bail!(
                "device authorization expired: {}",
                last_error.unwrap_or_else(|| "try again".into())
            );
        }
    }
    Ok(())
}

async fn run_models(paths: &VeraPaths, config: &Config, refresh: bool) -> Result<()> {
    let kind = ProviderKind::parse(&config.provider)?;
    let catalog = if refresh {
        provider_catalog_for(paths, kind).await?
    } else {
        cached_catalog(paths, kind)?
    };
    if catalog.for_provider(kind).is_empty() {
        anyhow::bail!(
            "no {} model catalog is available; authenticate and run `vera models --refresh`",
            kind.as_str()
        );
    }
    for model in catalog.for_provider(kind) {
        let effort = if model.supported_efforts.is_empty() {
            "provider-controlled".into()
        } else {
            format!(
                "{} (default {})",
                model.supported_efforts.join(", "),
                model.default_effort.as_deref().unwrap_or("auto")
            )
        };
        println!(
            "{}\t{}\t{} tokens\t{}\t{}",
            model.id, model.display_name, model.context_window, effort, model.source
        );
    }
    Ok(())
}

fn model_catalog(paths: &VeraPaths) -> Result<ModelCatalog> {
    let mut catalog = ModelCatalog::default();
    for kind in [ProviderKind::OpenaiCodex, ProviderKind::XaiOauth] {
        catalog.merge(load_cached_models(paths, kind)?);
    }
    Ok(catalog)
}

fn resolve_model(paths: &VeraPaths, provider: ProviderKind, id: &str) -> Result<ModelInfo> {
    let catalog = model_catalog(paths)?;
    resolve_model_from_catalog(&catalog, provider, id)
}

fn resolve_model_from_catalog(
    catalog: &ModelCatalog,
    provider: ProviderKind,
    id: &str,
) -> Result<ModelInfo> {
    let found = if id == "auto" {
        catalog.default_for(provider)
    } else {
        catalog.find(provider, id)
    };
    found
        .cloned()
        .with_context(|| {
            format!(
                "unknown model {id:?} for {}; authenticate and run `vera models --refresh`, then use /model <id>",
                provider.as_str()
            )
        })
}

fn cached_catalog(paths: &VeraPaths, provider: ProviderKind) -> Result<ModelCatalog> {
    let mut catalog = ModelCatalog::default();
    let models = load_cached_models(paths, provider)?;
    if !models.is_empty() {
        catalog.merge(models);
    }
    Ok(catalog)
}

async fn provider_catalog_for(paths: &VeraPaths, provider: ProviderKind) -> Result<ModelCatalog> {
    let store = TokenStore::new(paths.clone());
    let live = if let Some(token) = usable_token(&store, provider).await? {
        let client = ResponsesProvider::new(provider, token)?;
        match client.models().await {
            Ok(mut catalog) if !catalog.for_provider(provider).is_empty() => {
                for model in catalog.models.values_mut().flatten() {
                    model.source = "live".into();
                }
                cache_catalog(paths, provider, &catalog)?;
                Some(catalog)
            }
            Ok(_) => None,
            Err(_) => None,
        }
    } else {
        None
    };
    if let Some(catalog) = live {
        return Ok(catalog);
    }
    let cached = cached_catalog(paths, provider)?;
    if cached.for_provider(provider).is_empty() {
        anyhow::bail!(
            "live {} model discovery failed and no cached catalog exists; run `vera auth login {}`",
            provider.as_str(),
            provider.as_str()
        );
    }
    Ok(cached)
}

async fn resolve_live_model(
    paths: &VeraPaths,
    provider: ProviderKind,
    id: &str,
) -> Result<ModelInfo> {
    let catalog = provider_catalog_for(paths, provider).await?;
    resolve_model_from_catalog(&catalog, provider, id)
}

fn resolve_effort(model: &ModelInfo, requested: Option<&str>) -> Result<Option<String>> {
    if model.supported_efforts.is_empty() {
        if requested.is_some_and(|value| value != "auto") {
            anyhow::bail!(
                "model {} uses provider-controlled effort; /effort choices are only available for configurable models",
                model.id
            );
        }
        return Ok(None);
    }
    match requested.filter(|value| *value != "auto") {
        Some(value) if model.supported_efforts.iter().any(|effort| effort == value) => {
            Ok(Some(value.to_owned()))
        }
        Some(value) => anyhow::bail!(
            "invalid effort {value:?} for {}; choose: {}",
            model.id,
            model.supported_efforts.join(", ")
        ),
        None => Ok(model
            .default_effort
            .clone()
            .or_else(|| model.supported_efforts.first().cloned())),
    }
}

fn pick_model(models: &[ModelInfo]) -> Result<Option<String>> {
    if models.is_empty() {
        println!("no models are available");
        return Ok(None);
    }
    println!("available models:");
    for (index, model) in models.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, model.id, model.context_window);
    }
    println!("use /model <id> to select a model");
    Ok(None)
}

async fn select_provider(
    paths: &VeraPaths,
    config: &mut Config,
    provider_name: &str,
) -> Result<ModelInfo> {
    let provider = ProviderKind::parse(provider_name)?;
    usable_token(&TokenStore::new(paths.clone()), provider)
        .await?
        .with_context(|| {
            format!(
                "not authenticated to {}; run `vera auth login {}`",
                provider.as_str(),
                provider.as_str()
            )
        })?;
    let catalog = provider_catalog_for(paths, provider).await?;
    let model = catalog
        .default_for(provider)
        .cloned()
        .with_context(|| format!("no model catalog for {}", provider.as_str()))?;
    // The provider and its compatible default are committed together.
    config.provider = provider.as_str().into();
    config.model = model.id.clone();
    config.effort = None;
    Ok(model)
}

fn installed_plugins(paths: &VeraPaths) -> Result<Vec<PluginManifest>> {
    PluginManager::new(paths.clone()).list()
}

fn enabled_plugins(
    paths: &VeraPaths,
    config: &Config,
    session_plugins: &[String],
) -> Result<Vec<PluginManifest>> {
    let installed = installed_plugins(paths)?;
    let names = config
        .enabled_plugins
        .iter()
        .chain(session_plugins.iter())
        .collect::<std::collections::BTreeSet<_>>();
    for name in &names {
        if installed.iter().all(|plugin| &plugin.name != *name) {
            anyhow::bail!("enabled plugin {name} is not installed");
        }
    }
    Ok(installed
        .into_iter()
        .filter(|plugin| names.contains(&plugin.name))
        .collect())
}

fn configured_prompt_roots(paths: &VeraPaths, project: &Path, config: &Config) -> Vec<PathBuf> {
    config
        .prompt_roots
        .iter()
        .map(|value| {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else if project.join(value).exists() {
                project.join(value)
            } else {
                paths.root.join(value)
            }
        })
        .collect()
}

fn active_hooks(config: &Config, plugins: &[PluginManifest]) -> Vec<HookSpec> {
    let mut hooks = config
        .hooks
        .iter()
        .enumerate()
        .map(|(index, command)| HookSpec {
            name: format!("config-hook-{index}"),
            command: command.clone(),
            timeout_ms: 10_000,
            events: vec![
                "session_start".into(),
                "session_end".into(),
                "before_turn".into(),
                "after_turn".into(),
                "before_tool".into(),
                "after_tool".into(),
            ],
        })
        .collect::<Vec<_>>();
    hooks.extend(plugins.iter().flat_map(|plugin| plugin.hooks.clone()));
    hooks
}

async fn run_hooks(
    hooks: &[HookSpec],
    event: &str,
    root: &Path,
    session: &mut crate::sessions::Session,
    policy: &mut PermissionPolicy,
    approval: &mut dyn ApprovalHandler,
) -> Result<()> {
    let runner = HookRunner;
    for hook in hooks {
        if !hook.events.iter().any(|value| value == event) {
            continue;
        }
        runner
            .run_in(
                hook,
                HookEvent {
                    version: 1,
                    event: event.into(),
                    session_id: Some(session.header.id.clone()),
                    payload: serde_json::json!({
                        "root": root.display().to_string(),
                        "provider": session.selection.provider,
                        "model": session.selection.model,
                    }),
                },
                root,
                policy,
                approval,
                Some(session),
            )
            .await?;
    }
    Ok(())
}

fn run_session_list(paths: &VeraPaths) -> Result<()> {
    let store = SessionStore::new(paths.clone());
    for header in store.list()? {
        println!(
            "{}\t{}\t{}\t{}",
            header.id,
            header.created_at,
            header.provider,
            header.root.display()
        );
    }
    Ok(())
}

async fn run_session_resume(paths: &VeraPaths, config: Config, id: String) -> Result<()> {
    let session = SessionStore::new(paths.clone()).open(&id)?;
    let root = session.header.root.clone();
    drop(session);
    run_interactive(paths, &root, config, None, None, None, Some(id)).await
}

fn inspect(paths: &VeraPaths, root: &Path, config: &Config) -> Result<()> {
    let plugins = enabled_plugins(paths, config, &[])?;
    let skills = SkillCatalog::load_with_plugins(paths, root, &plugins, &config.allowed_skills)?;
    let prompts = PromptCatalog::load(
        paths,
        root,
        &plugins,
        &configured_prompt_roots(paths, root, config),
    )?;
    let agents = discover_agents(root, None)?;
    println!(
        "root: {}\nprovider: {}\nmodel: {}\n",
        root.display(),
        config.provider,
        config.model
    );
    println!("instructions:");
    for path in agents {
        println!("  {}", path.display());
    }
    println!("skills:");
    for name in skills.names() {
        println!(
            "  {name}\t{}",
            skills
                .get(name)
                .map(|skill| skill.description.as_str())
                .unwrap_or("")
        );
    }
    println!("prompts:");
    for name in prompts.names() {
        println!("  {name}");
    }
    println!("extensions:");
    for plugin in PluginManager::new(paths.clone()).list()? {
        println!(
            "  {}\t{}",
            plugin.name,
            if config.enabled_plugins.contains(&plugin.name) {
                "active"
            } else {
                "available"
            }
        );
    }
    println!("browser CDP endpoints:");
    if config.browser_cdp_endpoints.is_empty() {
        println!("  none (explicit browser approval required)");
    } else {
        for endpoint in &config.browser_cdp_endpoints {
            println!("  {endpoint}");
        }
    }
    println!("models:");
    for model in model_catalog(paths)?.for_provider(ProviderKind::parse(&config.provider)?) {
        println!("  {}\t{} tokens", model.id, model.context_window);
    }
    println!(
        "base prompt tokens: {}",
        approximate_tokens(crate::prompt::STATIC_SYSTEM_PROMPT)
    );
    let registry = ToolRegistry::standard_with_skills(Some(Arc::new(Mutex::new(skills))));
    let schemas = registry.schemas();
    println!(
        "tools: {}",
        schemas
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let schema_tokens = schemas
        .iter()
        .map(|tool| {
            approximate_tokens(&format!(
                "{} {} {}",
                tool.name, tool.description, tool.parameters
            ))
        })
        .sum::<usize>();
    println!("tool schema tokens: {schema_tokens}");
    println!(
        "tool schema bytes: {}",
        schemas
            .iter()
            .map(|tool| format!("{} {} {}", tool.name, tool.description, tool.parameters).len())
            .sum::<usize>()
    );
    Ok(())
}

async fn run_mcp(
    paths: &VeraPaths,
    root: &Path,
    effective_config: &Config,
    command: McpCommand,
) -> Result<()> {
    let registry = McpRegistry::new(paths.clone());
    match command {
        McpCommand::List => {
            for server in registry.list()? {
                println!(
                    "{}\t{} {}",
                    server.name,
                    server.command,
                    server.args.join(" ")
                );
            }
        }
        McpCommand::Tools { server } => {
            let spec = registry
                .list()?
                .into_iter()
                .find(|spec| spec.name == server)
                .context("MCP server not found")?;
            let mut policy = effective_config.permission_policy();
            let mut approval = TerminalApproval;
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::Mcp,
                        tool_name: Some("mcp_server_start".into()),
                        mcp_server: Some(spec.name.clone()),
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!("start MCP server {}", spec.name),
                    &mut approval,
                    None,
                )
                .await?;
            if spec.network {
                policy
                    .authorize_action(
                        crate::safety::ActionSignature {
                            permission_kind: PermissionKind::Network,
                            tool_name: Some("mcp_server_network".into()),
                            mcp_server: Some(spec.name.clone()),
                            ..crate::safety::ActionSignature::default()
                        },
                        &format!("allow network for MCP server {}", spec.name),
                        &mut approval,
                        None,
                    )
                    .await?;
            }
            let client = crate::extensions::McpClient::new(spec, root.to_path_buf());
            for tool in client.tools().await? {
                println!("{}\t{}", tool.name, tool.description);
            }
            client.shutdown().await?;
        }
        McpCommand::Enable { ref server } | McpCommand::Disable { ref server } => {
            if registry.list()?.iter().all(|spec| spec.name != *server) {
                anyhow::bail!("MCP server {server} is unavailable");
            }
            let enabled = matches!(&command, McpCommand::Enable { .. });
            let mut policy = effective_config.permission_policy();
            let mut approval = TerminalApproval;
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::Mcp,
                        tool_name: Some(
                            if enabled {
                                "mcp_server_enable"
                            } else {
                                "mcp_server_disable"
                            }
                            .into(),
                        ),
                        mcp_server: Some(server.clone()),
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!(
                        "{} MCP server {server}",
                        if enabled { "enable" } else { "disable" }
                    ),
                    &mut approval,
                    None,
                )
                .await?;
            let mut config = Config::load_global(paths)?;
            if enabled && !config.enabled_mcp.contains(server) {
                config.enabled_mcp.push(server.clone());
            }
            if !enabled {
                config.enabled_mcp.retain(|value| value != server);
            }
            config.save_global(paths)?;
            println!(
                "{} MCP {server}",
                if enabled { "enabled" } else { "disabled" }
            );
        }
        McpCommand::Test { name } => {
            let mut policy = effective_config.permission_policy();
            let mut approval = TerminalApproval;
            println!(
                "{}",
                registry
                    .test_in(&name, &mut policy, &mut approval, root)
                    .await?
            );
        }
    }
    Ok(())
}

fn run_permissions(config: &Config, command: PermissionsCommand) -> Result<()> {
    let policy = config.permission_policy();
    match command {
        PermissionsCommand::List => {
            println!("mode={}", policy.mode().label());
            for rule in policy.rules() {
                println!("{:?}\t{:?}", rule.effect, rule.matcher);
            }
        }
        PermissionsCommand::Check { kind } => {
            let kind = parse_permission_kind(&kind)?;
            println!("{}", policy.check(kind)?);
        }
    }
    Ok(())
}

fn parse_permission_kind(kind: &str) -> Result<PermissionKind> {
    match kind {
        "read" => Ok(PermissionKind::Read),
        "write" => Ok(PermissionKind::Write),
        "shell" => Ok(PermissionKind::Shell),
        "network" => Ok(PermissionKind::Network),
        "external-path" => Ok(PermissionKind::ExternalPath),
        "hook" => Ok(PermissionKind::Hook),
        "plugin" => Ok(PermissionKind::Plugin),
        "mcp" => Ok(PermissionKind::Mcp),
        "browser" => Ok(PermissionKind::Browser),
        "subagent" => Ok(PermissionKind::Subagent),
        other => anyhow::bail!("unknown permission kind {other}"),
    }
}

fn parse_permission_effect(effect: &str) -> Result<PermissionEffect> {
    match effect {
        "deny" => Ok(PermissionEffect::Deny),
        "ask" => Ok(PermissionEffect::Ask),
        "allow" => Ok(PermissionEffect::Allow),
        other => anyhow::bail!("unknown permission effect {other}"),
    }
}

async fn run_plugin(paths: &VeraPaths, config: &Config, command: PluginCommand) -> Result<()> {
    let manager = PluginManager::new(paths.clone());
    match command {
        PluginCommand::List => {
            for plugin in manager.list()? {
                println!("{}\t{}", plugin.name, plugin.version);
            }
        }
        PluginCommand::Remove { name } => {
            let mut policy = config.permission_policy();
            let mut approval = TerminalApproval;
            let canonical_plugin_path = if paths.plugins.exists() {
                Some(PathGuard::new(paths.plugins.clone())?.resolve(Path::new(&name))?)
            } else {
                None
            };
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::Plugin,
                        tool_name: Some(format!("plugin_remove:{name}")),
                        canonical_path: canonical_plugin_path,
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!("remove plugin {name}"),
                    &mut approval,
                    None,
                )
                .await?;
            manager.remove(&name)?;
            println!("removed {name}");
        }
        PluginCommand::Add { source } => {
            let source = fs::canonicalize(&source)
                .with_context(|| format!("resolve plugin source {}", source.display()))?;
            let mut policy = config.permission_policy();
            let mut approval = TerminalApproval;
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::Plugin,
                        tool_name: Some("plugin_add".into()),
                        canonical_path: Some(source.clone()),
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!("install plugin from {}", source.display()),
                    &mut approval,
                    None,
                )
                .await?;
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::ExternalPath,
                        tool_name: Some("plugin_add".into()),
                        canonical_path: Some(source.clone()),
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!("read plugin source {}", source.display()),
                    &mut approval,
                    None,
                )
                .await?;
            let plugin = manager.add_local(&source)?;
            println!("installed {} {}", plugin.name, plugin.version);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_headless(
    paths: &VeraPaths,
    root: &Path,
    config: &Config,
    provider_override: Option<&str>,
    model_override: Option<&str>,
    effort_override: Option<&str>,
    prompt: &str,
    output: OutputFormat,
) -> Result<()> {
    let provider_name = provider_override.unwrap_or(&config.provider);
    let kind = ProviderKind::parse(provider_name)?;
    let model = model_override.unwrap_or(&config.model);
    let store = TokenStore::new(paths.clone());
    let token = usable_token(&store, kind).await?.context(format!(
        "not logged in to {}; run vera auth login {}",
        kind.as_str(),
        kind.as_str()
    ))?;
    let catalog = provider_catalog_for(paths, kind).await?;
    let model_info = resolve_model_from_catalog(&catalog, kind, model)?;
    let effort = resolve_effort(&model_info, effort_override.or(config.effort.as_deref()))?;
    let provider = ResponsesProvider::new(kind, token)?;
    let plugins = enabled_plugins(paths, config, &[])?;
    let skills = Arc::new(Mutex::new(SkillCatalog::load_with_plugins(
        paths,
        root,
        &plugins,
        &config.allowed_skills,
    )?));
    let skill_snapshot = skills.lock().await.clone();
    let context = build_context(root, None, &skill_snapshot)?;
    let mut session = SessionStore::new(paths.clone()).create_with_selection(
        root.to_path_buf(),
        CapabilitySelection {
            provider: kind.as_str().into(),
            model: model_info.id.clone(),
            model_context_window: model_info.context_window,
            effort: config.effort.clone(),
            role: config.role.clone(),
            enabled_plugins: config.enabled_plugins.clone(),
            enabled_mcp: config.enabled_mcp.clone(),
            loaded_skills: Vec::new(),
        },
    )?;
    let hooks = active_hooks(config, &plugins);
    session.add_message("user", prompt)?;
    let guard = PathGuard::new(root.to_path_buf())?;
    let mut registry = ToolRegistry::standard_with_skills(Some(skills.clone()));
    registry
        .set_browser_endpoints(config.browser_cdp_endpoints.clone())
        .await;
    let available_mcp = McpRegistry::new(paths.clone()).list()?;
    let active_specs = active_mcp_specs(&available_mcp, &config.enabled_mcp);
    let mut mcp_clients = Vec::new();
    let mut policy = config.permission_policy();
    policy.restore_session_grants(session.approval_grants());
    let mut approval = TerminalApproval;
    if let Err(error) = refresh_mcp_tools(
        &mut registry,
        root,
        &active_specs,
        &mut mcp_clients,
        &mut policy,
        &mut approval,
        &mut session,
    )
    .await
    {
        let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
        return Err(error);
    }
    registry.set_subagent_runner(Arc::new(InProcessSubagentRunner::new(
        Arc::new(provider.clone()),
        paths.clone(),
        model_info.id.clone(),
        model_info.context_window,
        registry.clone(),
        policy.clone(),
        config.shell_timeout_seconds,
    )));
    if let Err(error) = run_hooks(
        &hooks,
        "session_start",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await
    {
        let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
        return Err(error);
    }
    if let Err(error) = run_hooks(
        &hooks,
        "before_turn",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await
    {
        let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
        return Err(error);
    }
    let input = session
        .messages
        .iter()
        .map(|message| ProviderInput::message(message.role.clone(), message.content.clone()))
        .collect();
    let response = match run_agent_turn(
        &provider,
        ProviderRequest {
            model: model_info.id,
            input,
            tools: registry.schemas(),
            instructions: format!(
                "{}\n\n# ACTIVE PLAN\n{}",
                context.system,
                session.plan_context()
            ),
            effort,
        },
        AgentRunContext {
            registry: &registry,
            guard: &guard,
            policy: &mut policy,
            approval: &mut approval,
            session: &mut session,
            output,
            shell_timeout: config.shell_timeout_seconds,
            hooks: &hooks,
            root,
            skills: Some(skills.clone()),
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => {
            let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
            return Err(error);
        }
    };
    if !response.text.is_empty() {
        session.add_message("assistant", response.text)?;
    }
    let pending_question_id = match response.control {
        Some(crate::tools::ToolResult::NeedsInput { question_id, .. }) => Some(question_id),
        _ => None,
    };
    if let Err(error) = run_hooks(
        &hooks,
        "after_turn",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await
    {
        let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
        return Err(error);
    }
    if let Err(error) = run_hooks(
        &hooks,
        "session_end",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await
    {
        let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
        return Err(error);
    }
    cleanup_runtime(&mcp_clients, &registry, &mut session).await?;
    if let Some(question_id) = pending_question_id {
        anyhow::bail!("pending question {question_id}; resume the session with an answer")
    }
    Ok(())
}

async fn run_interactive(
    paths: &VeraPaths,
    root: &Path,
    mut config: Config,
    provider_override: Option<String>,
    model_override: Option<String>,
    effort_override: Option<String>,
    resume_id: Option<String>,
) -> Result<()> {
    let store = SessionStore::new(paths.clone());
    let mut resumed = resume_id.as_deref().map(|id| store.open(id)).transpose()?;
    if let Some(session) = resumed.as_ref() {
        config.provider = session.settings.provider.clone();
        config.model = session.settings.model.clone();
        config.effort = session.settings.effort.clone();
    }
    if let Some(effort) = effort_override {
        config.effort = Some(effort);
    }
    if let Some(provider) = provider_override {
        select_provider(paths, &mut config, &provider).await?;
    }
    let provider_kind = ProviderKind::parse(&config.provider)?;
    let catalog = provider_catalog_for(paths, provider_kind).await?;
    if let Some(model) = model_override {
        config.model = resolve_model_from_catalog(&catalog, provider_kind, &model)?.id;
    }
    config.validate()?;
    let mut model_info = resolve_model_from_catalog(&catalog, provider_kind, &config.model)?;
    let mut session = if let Some(session) = resumed.take() {
        session
    } else {
        store.create_with_selection(
            root.to_path_buf(),
            CapabilitySelection {
                provider: provider_kind.as_str().into(),
                model: model_info.id.clone(),
                effort: config.effort.clone(),
                model_context_window: model_info.context_window,
                role: config.role.clone(),
                enabled_plugins: config.enabled_plugins.clone(),
                enabled_mcp: config.enabled_mcp.clone(),
                loaded_skills: Vec::new(),
            },
        )?
    };
    let mut session_plugins = if resume_id.is_some() {
        session.selection.enabled_plugins.clone()
    } else {
        config.enabled_plugins.clone()
    };
    let mut active_mcp = if resume_id.is_some() {
        session.selection.enabled_mcp.clone()
    } else {
        config.enabled_mcp.clone()
    };
    let mut plugins = enabled_plugins(paths, &config, &session_plugins)?;
    let skills = Arc::new(Mutex::new(SkillCatalog::load_with_plugins(
        paths,
        root,
        &plugins,
        &config.allowed_skills,
    )?));
    let guard = PathGuard::new(root.to_path_buf())?;
    let mut registry = ToolRegistry::standard_with_skills(Some(skills.clone()));
    registry
        .set_browser_endpoints(config.browser_cdp_endpoints.clone())
        .await;
    let mut mcp_clients: Vec<Arc<crate::extensions::McpClient>> = Vec::new();
    let mut policy = config.permission_policy();
    policy.restore_session_grants(session.approval_grants());
    let mut approval = TerminalApproval;
    let root_path = root.to_path_buf();
    run_hooks(
        &active_hooks(&config, &plugins),
        "session_start",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await?;
    let mut ctrl_c_pending = false;
    loop {
        let skill_snapshot = skills.lock().await.clone();
        let context = build_context(root, None, &skill_snapshot)?;
        let prompt_catalog = PromptCatalog::load(
            paths,
            root,
            &plugins,
            &configured_prompt_roots(paths, root, &config),
        )?;
        let instructions = context
            .instructions
            .iter()
            .map(|path| {
                path.strip_prefix(root).map_or_else(
                    |_| path.display().to_string(),
                    |relative| relative.display().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let skill_names = context
            .skills
            .iter()
            .map(|name| {
                if skill_snapshot.is_loaded(name) {
                    format!("{name} [loaded]")
                } else {
                    format!("{name} [available]")
                }
            })
            .collect::<Vec<_>>();
        let installed = installed_plugins(paths)?;
        let mut extensions = installed
            .iter()
            .map(|plugin| {
                if session_plugins.contains(&plugin.name) {
                    format!("{} [active]", plugin.name)
                } else {
                    format!("{} [available]", plugin.name)
                }
            })
            .collect::<Vec<_>>();
        extensions.extend(["hooks [active]".into(), "bounded subagents [active]".into()]);
        let available_mcp = McpRegistry::new(paths.clone()).list()?;
        let mcp_servers = active_mcp.len();
        let dashboard_effort = resolve_effort(
            &model_info,
            session
                .selection
                .effort
                .as_deref()
                .or(config.effort.as_deref()),
        )?
        .unwrap_or_else(|| "provider-controlled".into());
        let frame = render_dashboard(&Dashboard {
            version: env!("CARGO_PKG_VERSION"),
            root: &root_path,
            instructions: &instructions,
            skills: &skill_names,
            prompts: &prompt_catalog.names().cloned().collect::<Vec<_>>(),
            extensions: &extensions,
            mcp_servers,
            mcp_available: available_mcp.len(),
            provider: &config.provider,
            model: &config.model,
            effort: &dashboard_effort,
            context_tokens: session.context_display().0,
            context_limit: model_info.context_window,
            context_estimated: !session.context_display().1,
            mode: policy.mode(),
        })?;
        let line = match read_input(&mut ctrl_c_pending)? {
            InputAction::CycleMode => {
                policy.cycle_mode();
                continue;
            }
            InputAction::Cancel => continue,
            InputAction::Exit => break,
            InputAction::Submit(line) => {
                frame.finish_input()?;
                line
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let user_turn: Option<String> = match line {
            "/quit" | "/exit" => break,
            "/help" | "/commands" => {
                print_interactive_help();
                None
            }
            "/plan" => {
                policy.set_mode(PermissionMode::Plan);
                println!("mode: {}", policy.mode().label());
                println!(
                    "active plan (revision {}):\n{}",
                    session.plan.version,
                    session.plan_context()
                );
                None
            }
            "/confirm" => {
                policy.set_mode(PermissionMode::Confirm);
                println!("mode: {}", policy.mode().label());
                None
            }
            "/auto" => {
                policy.set_mode(PermissionMode::Auto);
                println!("mode: {}", policy.mode().label());
                None
            }
            "/yolo" => {
                policy.set_mode(PermissionMode::Yolo);
                println!("mode: {}", policy.mode().label());
                None
            }
            "/permissions" => {
                println!(
                    "mode={}; Plan=read-only; Confirm=ask; Auto=non-risky/external auto; Yolo=never ask",
                    policy.mode().label()
                );
                for rule in policy.rules() {
                    println!("{:?}\t{:?}", rule.effect, rule.matcher);
                }
                None
            }
            command if command.starts_with("/permissions ") => {
                let mut parts = command.split_whitespace();
                let _ = parts.next();
                match (parts.next(), parts.next()) {
                    (Some(effect), Some(kind)) => {
                        let rule = PermissionRule {
                            effect: parse_permission_effect(effect)?,
                            matcher: PermissionMatcher {
                                permission_kind: Some(parse_permission_kind(kind)?),
                                ..PermissionMatcher::default()
                            },
                        };
                        policy.add_user_rule(rule);
                        println!("added user permission rule; use /permissions to review");
                    }
                    _ => println!("use /permissions [deny|ask|allow] <kind>"),
                }
                None
            }
            "/context" => {
                println!(
                    "{} tokens; {} instruction file(s); {} skill(s)",
                    session.context_display().0,
                    context.instructions.len(),
                    context.skills.len()
                );
                None
            }
            "/compact" => {
                println!(
                    "compacted: {}",
                    session.compact(config.context_window_tokens)?
                );
                None
            }
            "/diff" => {
                println!(
                    "Use the git_status_diff tool in the next turn; the repository remains unchanged by this command."
                );
                None
            }
            "/undo" => {
                policy
                    .authorize_action(
                        crate::safety::ActionSignature {
                            permission_kind: PermissionKind::Write,
                            tool_name: Some("session_undo".into()),
                            ..crate::safety::ActionSignature::default()
                        },
                        "restore Vera-managed file preimages",
                        &mut approval,
                        Some(&mut session),
                    )
                    .await?;
                println!("restored {} Vera-managed file(s)", session.undo(root)?);
                None
            }
            "/skills" => {
                for name in skill_snapshot.names() {
                    println!(
                        "{name}\t{}",
                        skill_snapshot
                            .get(name)
                            .map(|skill| skill.description.as_str())
                            .unwrap_or("")
                    );
                }
                None
            }
            command if command.starts_with("/skill ") => {
                let mut parts = command.splitn(3, ' ');
                let _ = parts.next();
                match (parts.next(), parts.next()) {
                    (Some("load"), Some(name)) => {
                        skills.lock().await.load_body(name.trim())?;
                        session.record_skill_state(name.trim(), true)?;
                        println!("loaded skill {name}");
                    }
                    (Some("unload"), Some(name)) => {
                        skills.lock().await.unload(name.trim())?;
                        session.record_skill_state(name.trim(), false)?;
                        println!("unloaded skill {name}");
                    }
                    _ => println!("use /skill load|unload <name>"),
                }
                None
            }
            "/prompts" => {
                for name in prompt_catalog.names() {
                    if let Some(prompt) = prompt_catalog.get(name) {
                        println!("{name}\t{}", prompt.description);
                    }
                }
                None
            }
            "/prompt" | "/prompt list" => {
                for name in prompt_catalog.names() {
                    if let Some(prompt) = prompt_catalog.get(name) {
                        println!("{name}\t{}", prompt.description);
                    }
                }
                None
            }
            command if command.starts_with("/prompt ") => {
                let mut parts = command.splitn(3, ' ');
                let _ = parts.next();
                let name = parts.next().unwrap_or_default();
                if name == "preview" {
                    let template = parts.next().context("/prompt preview requires a name")?;
                    println!("{}", prompt_catalog.preview(template)?);
                    None
                } else {
                    Some(prompt_catalog.expand(name, parts.next().unwrap_or(""))?)
                }
            }
            "/agents" => {
                for path in discover_agents(root, None)? {
                    println!("{}", path.display());
                }
                for agent in registry.subagents().list(&session.header.id).await {
                    println!("{}\t{}\t{}", agent.agent_id, agent.status, agent.task);
                }
                for (worktree_id, state) in &session.worktree_lifecycle {
                    if !matches!(state.state.as_str(), "discarded" | "merged") {
                        println!("worktree:{worktree_id}\t{}\trecoverable", state.state);
                    }
                }
                None
            }
            command if command.starts_with("/agent ") => {
                let id = command[7..].trim();
                println!(
                    "{}",
                    serde_json::to_string(&registry.subagents().status(id).await?)?
                );
                None
            }
            "/processes" => {
                for process in registry.processes().list(Some(&session.header.id)).await {
                    println!(
                        "{}\t{}\t{}",
                        process.process_id, process.status, process.command
                    );
                }
                None
            }
            "/mcp" | "/mcp list" => {
                for server in McpRegistry::new(paths.clone()).list()? {
                    println!(
                        "{}\t{}",
                        server.name,
                        if active_mcp.contains(&server.name) {
                            "active"
                        } else {
                            "available"
                        }
                    );
                }
                None
            }
            command if command.starts_with("/mcp ") => {
                let mut parts = command.split_whitespace();
                let _ = parts.next();
                match (parts.next(), parts.next()) {
                    (Some("enable"), Some(name)) => {
                        if available_mcp.iter().all(|server| server.name != name) {
                            anyhow::bail!("MCP server {name} is unavailable");
                        }
                        policy
                            .authorize_action(
                                crate::safety::ActionSignature {
                                    permission_kind: PermissionKind::Mcp,
                                    tool_name: Some("mcp_server_enable".into()),
                                    mcp_server: Some(name.into()),
                                    ..crate::safety::ActionSignature::default()
                                },
                                &format!("enable MCP server {name} for this session"),
                                &mut approval,
                                Some(&mut session),
                            )
                            .await?;
                        if !active_mcp.contains(&name.to_owned()) {
                            active_mcp.push(name.to_owned());
                        }
                        session.record_mcp_state(name, true)?;
                        println!("enabled MCP {name}");
                    }
                    (Some("disable"), Some(name)) => {
                        policy
                            .authorize_action(
                                crate::safety::ActionSignature {
                                    permission_kind: PermissionKind::Mcp,
                                    tool_name: Some("mcp_server_disable".into()),
                                    mcp_server: Some(name.into()),
                                    ..crate::safety::ActionSignature::default()
                                },
                                &format!("disable MCP server {name} for this session"),
                                &mut approval,
                                Some(&mut session),
                            )
                            .await?;
                        active_mcp.retain(|value| value != name);
                        if let Some(client) =
                            mcp_clients.iter().find(|client| client.name() == name)
                        {
                            client.shutdown().await?;
                            session.record_mcp_lifecycle(
                                name,
                                crate::sessions::LifecycleState {
                                    state: "stopped".into(),
                                    detail: None,
                                },
                            )?;
                        }
                        registry.remove_tools_starting_with(&format!("mcp__{name}__"));
                        session.record_mcp_state(name, false)?;
                        println!("disabled MCP {name}");
                    }
                    _ => println!("use /mcp [list] or /mcp enable|disable <name>"),
                }
                None
            }
            "/extensions" => {
                for plugin in &installed {
                    println!(
                        "{}\t{}",
                        plugin.name,
                        if session_plugins.contains(&plugin.name) {
                            "active"
                        } else {
                            "available"
                        }
                    );
                }
                None
            }
            command if command.starts_with("/extension ") => {
                let mut parts = command.split_whitespace();
                let _ = parts.next();
                match (parts.next(), parts.next()) {
                    (Some("enable"), Some(name)) => {
                        if installed.iter().all(|plugin| plugin.name != name) {
                            anyhow::bail!("extension {name} is not installed");
                        }
                        policy
                            .authorize_action(
                                crate::safety::ActionSignature {
                                    permission_kind: PermissionKind::Plugin,
                                    tool_name: Some(format!("plugin_enable:{name}")),
                                    ..crate::safety::ActionSignature::default()
                                },
                                &format!("enable extension {name} for this session"),
                                &mut approval,
                                Some(&mut session),
                            )
                            .await?;
                        if !session_plugins.contains(&name.to_owned()) {
                            session_plugins.push(name.to_owned());
                        }
                        plugins = enabled_plugins(paths, &config, &session_plugins)?;
                        *skills.lock().await = SkillCatalog::load_with_plugins(
                            paths,
                            root,
                            &plugins,
                            &config.allowed_skills,
                        )?;
                        session.set_selection(CapabilitySelection {
                            enabled_plugins: session_plugins.clone(),
                            enabled_mcp: active_mcp.clone(),
                            loaded_skills: skills.lock().await.loaded_names().cloned().collect(),
                            ..session.selection.clone()
                        })?;
                        println!("enabled extension {name}");
                    }
                    (Some("disable"), Some(name)) => {
                        policy
                            .authorize_action(
                                crate::safety::ActionSignature {
                                    permission_kind: PermissionKind::Plugin,
                                    tool_name: Some(format!("plugin_disable:{name}")),
                                    ..crate::safety::ActionSignature::default()
                                },
                                &format!("disable extension {name} for this session"),
                                &mut approval,
                                Some(&mut session),
                            )
                            .await?;
                        session_plugins.retain(|value| value != name);
                        let disabled_mcp = installed
                            .iter()
                            .find(|plugin| plugin.name == name)
                            .map(|plugin| {
                                plugin
                                    .mcp
                                    .iter()
                                    .map(|server| server.name.clone())
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        active_mcp.retain(|value| !disabled_mcp.contains(value));
                        for client in &mcp_clients {
                            if disabled_mcp.iter().any(|value| value == client.name()) {
                                client.shutdown().await?;
                            }
                        }
                        for server in &disabled_mcp {
                            registry.remove_tools_starting_with(&format!("mcp__{server}__"));
                            if active_mcp.iter().all(|active| active != server) {
                                session.record_mcp_state(server, false)?;
                            }
                            session.record_mcp_lifecycle(
                                server,
                                crate::sessions::LifecycleState {
                                    state: "stopped".into(),
                                    detail: None,
                                },
                            )?;
                        }
                        plugins = enabled_plugins(paths, &config, &session_plugins)?;
                        *skills.lock().await = SkillCatalog::load_with_plugins(
                            paths,
                            root,
                            &plugins,
                            &config.allowed_skills,
                        )?;
                        session.set_selection(CapabilitySelection {
                            enabled_plugins: session_plugins.clone(),
                            enabled_mcp: active_mcp.clone(),
                            loaded_skills: skills.lock().await.loaded_names().cloned().collect(),
                            ..session.selection.clone()
                        })?;
                        println!("disabled extension {name}");
                    }
                    _ => println!("use /extension enable|disable <name>"),
                }
                None
            }
            command if command.starts_with("/provider ") => {
                let model = select_provider(paths, &mut config, command[10..].trim()).await?;
                model_info = model.clone();
                config.effort = Some("auto".into());
                session.set_selection(CapabilitySelection {
                    provider: config.provider.clone(),
                    model: model.id.clone(),
                    effort: config.effort.clone(),
                    model_context_window: model.context_window,
                    ..session.selection.clone()
                })?;
                println!("provider: {} (model {})", config.provider, config.model);
                None
            }
            "/model" => {
                let kind = ProviderKind::parse(&config.provider)?;
                let catalog = provider_catalog_for(paths, kind).await?;
                match pick_model(catalog.for_provider(kind))? {
                    Some(id) => {
                        let model = resolve_model_from_catalog(&catalog, kind, &id)?;
                        model_info = model.clone();
                        config.model = model.id.clone();
                        config.effort = Some("auto".into());
                        session.set_selection(CapabilitySelection {
                            model: model.id.clone(),
                            effort: config.effort.clone(),
                            model_context_window: model.context_window,
                            ..session.selection.clone()
                        })?;
                        println!("model: {}", config.model);
                    }
                    None => println!("model selection cancelled"),
                }
                None
            }
            "/models" => {
                let catalog =
                    provider_catalog_for(paths, ProviderKind::parse(&config.provider)?).await?;
                for model in catalog.for_provider(ProviderKind::parse(&config.provider)?) {
                    println!(
                        "{}\t{}\t{} tokens\t{}",
                        model.id, model.display_name, model.context_window, model.source
                    );
                }
                None
            }
            "/effort" => {
                if model_info.supported_efforts.is_empty() {
                    println!("{} uses provider-controlled effort", model_info.id);
                } else {
                    println!("effort levels for {}:", model_info.id);
                    for level in &model_info.supported_efforts {
                        let marker = if Some(level.as_str()) == model_info.default_effort.as_deref()
                        {
                            " (default)"
                        } else {
                            ""
                        };
                        println!("  {level}{marker}");
                    }
                }
                None
            }
            command if command.starts_with("/effort ") => {
                let value = command[8..].trim();
                let effort = resolve_effort(&model_info, Some(value))?;
                config.effort = Some(value.to_owned());
                session.set_selection(CapabilitySelection {
                    effort: config.effort.clone(),
                    ..session.selection.clone()
                })?;
                println!(
                    "effort: {}",
                    effort.as_deref().unwrap_or("provider-controlled")
                );
                None
            }
            command if command.starts_with("/model ") => {
                let kind = ProviderKind::parse(&config.provider)?;
                let model = resolve_live_model(paths, kind, command[7..].trim()).await?;
                model_info = model.clone();
                config.model = model.id.clone();
                config.effort = Some("auto".into());
                session.set_selection(CapabilitySelection {
                    model: config.model.clone(),
                    effort: config.effort.clone(),
                    model_context_window: model.context_window,
                    ..session.selection.clone()
                })?;
                println!("model: {}", config.model);
                None
            }
            command if command.starts_with("/resume ") => {
                let resumed = store.open(line[8..].trim())?;
                if resumed.header.root != root {
                    anyhow::bail!(
                        "session belongs to {}; use `vera session resume {}` from that repository",
                        resumed.header.root.display(),
                        resumed.header.id
                    );
                }
                session = resumed;
                config.provider = session.settings.provider.clone();
                config.model = session.settings.model.clone();
                config.effort = session.settings.effort.clone();
                let resumed_kind = ProviderKind::parse(&config.provider)?;
                let resumed_catalog = provider_catalog_for(paths, resumed_kind).await?;
                model_info =
                    resolve_model_from_catalog(&resumed_catalog, resumed_kind, &config.model)?;
                config.model = model_info.id.clone();
                session_plugins = session.selection.enabled_plugins.clone();
                active_mcp = session.selection.enabled_mcp.clone();
                plugins = enabled_plugins(paths, &config, &session_plugins)?;
                let mut resumed_skills =
                    SkillCatalog::load_with_plugins(paths, root, &plugins, &config.allowed_skills)?;
                for name in session.selection.loaded_skills.clone() {
                    let _ = resumed_skills.load_body(&name)?;
                }
                *skills.lock().await = resumed_skills;
                println!("resumed {}", session.header.id);
                None
            }
            prompt => Some(prompt.to_owned()),
        };
        let Some(prompt) = user_turn else { continue };
        let pending_continuation = if session.pending_question.is_some() {
            Some(session.answer_pending_question(&prompt)?)
        } else {
            None
        };
        session.add_message("user", &prompt)?;
        let kind = ProviderKind::parse(&config.provider)?;
        let token = usable_token(&TokenStore::new(paths.clone()), kind)
            .await?
            .context(format!(
                "not logged in; run vera auth login {}",
                kind.as_str()
            ))?;
        let provider = ResponsesProvider::new(kind, token)?;
        let plan_mode = policy.mode() == PermissionMode::Plan;
        let active_specs = active_mcp_specs(&available_mcp, &active_mcp);
        if let Err(error) = refresh_mcp_tools(
            &mut registry,
            root,
            &active_specs,
            &mut mcp_clients,
            &mut policy,
            &mut approval,
            &mut session,
        )
        .await
        {
            let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
            return Err(error);
        }
        registry.set_subagent_runner(Arc::new(InProcessSubagentRunner::new(
            Arc::new(provider.clone()),
            paths.clone(),
            config.model.clone(),
            model_info.context_window,
            registry.clone(),
            policy.clone(),
            config.shell_timeout_seconds,
        )));
        let hooks = active_hooks(&config, &plugins);
        if let Err(error) = run_hooks(
            &hooks,
            "before_turn",
            root,
            &mut session,
            &mut policy,
            &mut approval,
        )
        .await
        {
            let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
            return Err(error);
        }
        let skill_snapshot = skills.lock().await.clone();
        let turn_context = build_context(root, None, &skill_snapshot)?;
        let instructions = if plan_mode {
            format!(
                "{}\n\n# ACTIVE PLAN\n{}\n\n# PLAN MODE\nResearch and inspect only. Do not modify files, run mutating commands, or claim implementation. Produce a structured plan with findings, assumptions, affected files, validation steps, and risks.",
                turn_context.system,
                session.plan_context()
            )
        } else {
            format!(
                "{}\n\n# ACTIVE PLAN\n{}",
                turn_context.system,
                session.plan_context()
            )
        };
        let mut input = session
            .messages
            .iter()
            .map(|message| ProviderInput::message(message.role.clone(), message.content.clone()))
            .collect::<Vec<_>>();
        if let Some(continuation) = pending_continuation {
            let answer = input.pop().context("pending answer was not recorded")?;
            input = continuation;
            input.push(answer);
        }
        let response = match run_agent_turn(
            &provider,
            ProviderRequest {
                model: config.model.clone(),
                input,
                tools: if plan_mode {
                    registry.read_only_schemas()
                } else {
                    registry.schemas()
                },
                instructions,
                effort: resolve_effort(
                    &model_info,
                    session
                        .selection
                        .effort
                        .as_deref()
                        .or(config.effort.as_deref()),
                )?,
            },
            AgentRunContext {
                registry: &registry,
                guard: &guard,
                policy: &mut policy,
                approval: &mut approval,
                session: &mut session,
                output: OutputFormat::Text,
                shell_timeout: config.shell_timeout_seconds,
                hooks: &hooks,
                root,
                skills: Some(skills.clone()),
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
                return Err(error);
            }
        };
        if let Err(error) = run_hooks(
            &hooks,
            "after_turn",
            root,
            &mut session,
            &mut policy,
            &mut approval,
        )
        .await
        {
            let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
            return Err(error);
        }
        if !response.text.is_empty() {
            session.add_message("assistant", response.text.clone())?;
            if plan_mode {
                print_plan_prompts(&prompt, &response.text);
            }
        }
        if let Err(error) = session.compact_if_needed(config.context_window_tokens) {
            let _ = cleanup_runtime(&mcp_clients, &registry, &mut session).await;
            return Err(error);
        }
    }
    cleanup_runtime(&mcp_clients, &registry, &mut session).await?;
    run_hooks(
        &active_hooks(&config, &plugins),
        "session_end",
        root,
        &mut session,
        &mut policy,
        &mut approval,
    )
    .await?;
    Ok(())
}

fn active_mcp_specs(
    all: &[crate::extensions::McpSpec],
    active: &[String],
) -> Vec<crate::extensions::McpSpec> {
    all.iter()
        .filter(|spec| active.contains(&spec.name))
        .cloned()
        .collect()
}

async fn cleanup_runtime(
    mcp_clients: &[Arc<crate::extensions::McpClient>],
    registry: &ToolRegistry,
    session: &mut crate::sessions::Session,
) -> Result<()> {
    let mcp_names = mcp_clients
        .iter()
        .map(|client| client.name().to_owned())
        .collect::<Vec<_>>();
    for client in mcp_clients {
        let _ = client.shutdown().await;
    }

    let surviving = registry.processes().list(Some(&session.header.id)).await;
    if !surviving.is_empty() {
        eprintln!(
            "[vera] background processes still attached: {}",
            surviving
                .iter()
                .map(|process| process.process_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    registry.shutdown_processes(&session.header.id).await;

    for name in mcp_names {
        session.record_mcp_lifecycle(
            name,
            crate::sessions::LifecycleState {
                state: "stopped".into(),
                detail: None,
            },
        )?;
    }
    for process in surviving {
        session.record_process_lifecycle(
            process.process_id,
            crate::sessions::LifecycleState {
                state: "shutdown_requested".into(),
                detail: Some(process.status),
            },
        )?;
    }
    Ok(())
}

async fn refresh_mcp_tools(
    registry: &mut ToolRegistry,
    root: &Path,
    specs: &[crate::extensions::McpSpec],
    clients: &mut Vec<Arc<crate::extensions::McpClient>>,
    policy: &mut PermissionPolicy,
    approval: &mut dyn ApprovalHandler,
    session: &mut crate::sessions::Session,
) -> Result<()> {
    for spec in specs {
        let existing = clients
            .iter()
            .find(|client| client.name() == spec.name)
            .cloned();
        let client = if let Some(client) = existing.clone()
            && client.is_started().await
        {
            client
        } else {
            policy
                .authorize_action(
                    crate::safety::ActionSignature {
                        permission_kind: PermissionKind::Mcp,
                        tool_name: Some("mcp_server_start".into()),
                        mcp_server: Some(spec.name.clone()),
                        ..crate::safety::ActionSignature::default()
                    },
                    &format!("start MCP server {}", spec.name),
                    approval,
                    Some(session),
                )
                .await?;
            if spec.network {
                policy
                    .authorize_action(
                        crate::safety::ActionSignature {
                            permission_kind: PermissionKind::Network,
                            tool_name: Some("mcp_server_network".into()),
                            mcp_server: Some(spec.name.clone()),
                            ..crate::safety::ActionSignature::default()
                        },
                        &format!("allow network for MCP server {}", spec.name),
                        approval,
                        Some(session),
                    )
                    .await?;
            }
            let client = existing.unwrap_or_else(|| {
                Arc::new(crate::extensions::McpClient::new(
                    spec.clone(),
                    root.to_path_buf(),
                ))
            });
            if clients.iter().all(|entry| !Arc::ptr_eq(entry, &client)) {
                clients.push(client.clone());
            }
            client
        };
        let tools = client.tools().await?;
        session.record_mcp_lifecycle(
            spec.name.clone(),
            crate::sessions::LifecycleState {
                state: "active".into(),
                detail: Some(
                    crate::auth::redact(&format!(
                        "{} tool(s); server_info={}",
                        tools.len(),
                        client.server_info().await
                    ))
                    .chars()
                    .take(2_000)
                    .collect(),
                ),
            },
        )?;
        for description in tools {
            if !spec.allowed_tools.is_empty() && !spec.allowed_tools.contains(&description.name) {
                continue;
            }
            let tool = crate::tools::McpToolAdapter::new(client.clone(), description);
            registry.add_tool(Arc::new(tool));
        }
    }
    Ok(())
}

async fn usable_token(store: &TokenStore, kind: ProviderKind) -> Result<Option<TokenRecord>> {
    let Some(token) = store.get(kind.auth_provider())? else {
        return Ok(None);
    };
    if token.is_valid(now_seconds()) {
        return Ok(Some(token));
    }
    let oauth = crate::auth::OAuthClient::new()?;
    match oauth.refresh(&token).await {
        Ok(refreshed) => {
            store.put(refreshed.clone())?;
            Ok(Some(refreshed))
        }
        Err(error) => {
            let _ = store.remove(Some(kind.auth_provider()));
            Err(error)
        }
    }
}

fn print_help() {
    println!(
        "vera [path]\nvera -p \"prompt\" [--provider <id>] [--model <id>] [--effort <level>] --output text|jsonl\nvera --prompt-template <name> --prompt-args \"...\"\n\nCommands:\n  auth login|status|logout\n  models [--refresh]\n  session list|resume <id>\n  inspect\n  mcp list|tools|enable|disable|test <name>\n  permissions list|check <kind>\n  plugin add|list|remove\n\nInteractive capability commands:\n  /skills  /skill load|unload <name>\n  /prompts  /prompt <name> [arguments]\n  /extensions  /extension enable|disable <name>\n  /mcp  /mcp enable|disable <name>\n  /processes  /agents  /agent <id>\n  /models  /model [<id>]  /effort [<level>]  /provider <id>\n  update|upgrade"
    );
}

fn print_interactive_help() {
    println!(
        "shift+tab cycle modes  /plan  /confirm  /auto  /yolo  /provider <id>  /models  /model <id>  /effort <level>  /permissions [deny|ask|allow] <kind>  /processes  /compact  /context  /diff  /undo  /resume <id>  /skills  /skill load|unload <name>  /prompts  /prompt <name> [args]  /extensions  /extension enable|disable <name>  /mcp enable|disable <name>  /agents  /agent <id>  /quit"
    );
}

fn print_plan_prompts(objective: &str, draft: &str) {
    let implementation = format!(
        "Implement the following Vera plan. Do not improvise beyond it without explaining why.\n\nObjective: {objective}\n\nPlan draft:\n{draft}"
    );
    println!("\n[Plan Draft]\n{draft}");
    println!("\n[Prompt 1 — implement]\n{implementation}");
    println!(
        "\n[Prompt 2 — implement and clear context]\n{implementation}\n\nAfter implementation, clear the active context and continue verification from a fresh context."
    );
    println!(
        "\n[Prompt 3 — implement, clear context, and invoke /goal]\n{implementation}\n\nAfter implementation, clear the active context, then invoke /goal with the objective above and continue until fully verified."
    );
}

struct PendingToolCall {
    name: String,
    arguments: String,
}

struct AgentSink {
    terminal: TerminalEventSink,
    calls: std::collections::BTreeMap<String, PendingToolCall>,
}

impl AgentSink {
    fn new(output: OutputFormat) -> Self {
        Self {
            terminal: TerminalEventSink::new(output),
            calls: std::collections::BTreeMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl EventSink for AgentSink {
    async fn emit(&mut self, event: Event) -> Result<()> {
        if let Event::ToolCallDelta {
            id,
            name,
            arguments,
        } = &event
        {
            let call = self.calls.entry(id.clone()).or_insert(PendingToolCall {
                name: String::new(),
                arguments: String::new(),
            });
            if !name.is_empty() {
                call.name = name.clone();
            }
            call.arguments.push_str(arguments);
        }
        self.terminal.emit(event).await
    }
}

struct AgentRunContext<'a> {
    registry: &'a ToolRegistry,
    guard: &'a PathGuard,
    policy: &'a mut PermissionPolicy,
    approval: &'a mut dyn ApprovalHandler,
    session: &'a mut crate::sessions::Session,
    output: OutputFormat,
    shell_timeout: u64,
    hooks: &'a [HookSpec],
    root: &'a Path,
    skills: Option<Arc<Mutex<SkillCatalog>>>,
}

struct AgentTurnResult {
    text: String,
    control: Option<crate::tools::ToolResult>,
}

async fn run_agent_turn(
    provider: &dyn Provider,
    request: ProviderRequest,
    mut context: AgentRunContext<'_>,
) -> Result<AgentTurnResult> {
    context.policy.begin_turn();
    let result = run_agent_turn_inner(provider, request, &mut context).await;
    context.policy.end_turn();
    result
}

async fn run_agent_turn_inner(
    provider: &dyn Provider,
    mut request: ProviderRequest,
    context: &mut AgentRunContext<'_>,
) -> Result<AgentTurnResult> {
    let mut answer = String::new();
    for _ in 0..8 {
        let local_estimate = approximate_tokens(&request.instructions)
            + request
                .input
                .iter()
                .map(|item| match item {
                    ProviderInput::Message { content, .. }
                    | ProviderInput::FunctionCall {
                        arguments: content, ..
                    }
                    | ProviderInput::FunctionCallOutput {
                        output: content, ..
                    } => approximate_tokens(content),
                    ProviderInput::ImageMessage { text, .. } => approximate_tokens(text),
                })
                .sum::<usize>()
            + request
                .tools
                .iter()
                .map(|tool| {
                    approximate_tokens(&format!(
                        "{} {} {}",
                        tool.name, tool.description, tool.parameters
                    ))
                })
                .sum::<usize>();
        // This is deliberately recorded before the provider responds so the
        // dashboard can label a full-request local estimate. A later provider
        // usage event replaces it as authoritative.
        context.session.record_estimate(local_estimate)?;
        let mut sink = AgentSink::new(context.output);
        let result = provider.stream(request.clone(), &mut sink).await?;
        if result.input_tokens > 0 {
            context.session.record_usage(
                result.input_tokens,
                result.output_tokens,
                local_estimate,
            )?;
        }
        answer.push_str(&result.text);
        if sink.calls.is_empty() {
            return Ok(AgentTurnResult {
                text: answer,
                control: None,
            });
        }
        for (id, call) in sink.calls {
            let arguments =
                serde_json::from_str(&call.arguments).unwrap_or_else(|_| serde_json::json!({}));
            let tool_name = if call.name.is_empty() {
                "unknown"
            } else {
                &call.name
            };
            run_hooks(
                context.hooks,
                "before_tool",
                context.root,
                context.session,
                context.policy,
                context.approval,
            )
            .await?;
            let image_capability_error = !provider.supports_image_input()
                && matches!(tool_name, "image_inspect" | "browser_screenshot");
            let tool_result = if image_capability_error {
                crate::tools::ToolResult::complete(
                    "provider does not support image input; image inspection was not executed",
                    true,
                )
            } else if context.registry.find(tool_name).is_some() {
                match execute(
                    context.registry,
                    ToolCall {
                        name: tool_name.into(),
                        arguments: arguments.clone(),
                    },
                    ToolContext {
                        guard: context.guard,
                        policy: context.policy,
                        approval: context.approval,
                        session: Some(&mut *context.session),
                        shell_timeout: context.shell_timeout,
                    },
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => crate::tools::ToolResult::complete(
                        crate::auth::redact(&format!("tool {tool_name} failed: {error}")),
                        true,
                    ),
                }
            } else {
                crate::tools::ToolResult::complete(format!("unknown tool {tool_name}"), true)
            };
            let image_input = if tool_result.is_error() {
                None
            } else {
                image_input_for_tool(tool_name, &arguments, context.guard)?
            };
            run_hooks(
                context.hooks,
                "after_tool",
                context.root,
                context.session,
                context.policy,
                context.approval,
            )
            .await?;
            context
                .session
                .append(crate::sessions::SessionRecord::ToolCall {
                    id: id.clone(),
                    name: tool_name.into(),
                    arguments,
                    result: Some(tool_result.content()),
                })?;
            request.input.push(ProviderInput::FunctionCall {
                id: id.clone(),
                name: tool_name.into(),
                arguments: call.arguments.clone(),
            });
            request.input.push(ProviderInput::FunctionCallOutput {
                call_id: id,
                output: tool_result.content(),
            });
            if let Some(image) = image_input {
                if !provider.supports_image_input() {
                    anyhow::bail!("provider does not support image input");
                }
                request.input.push(image);
            }
            if let crate::tools::ToolResult::NeedsInput {
                question_id,
                prompt,
                choices,
            } = &tool_result
            {
                context.session.record_pending_question(
                    question_id.clone(),
                    prompt.clone(),
                    choices.clone(),
                    request.input.clone(),
                )?;
                sink.terminal
                    .emit(Event::NeedsInput {
                        question_id: question_id.clone(),
                        prompt: prompt.clone(),
                        choices: choices.clone(),
                    })
                    .await?;
                return Ok(AgentTurnResult {
                    text: answer,
                    control: Some(tool_result),
                });
            }
            if let Some(skills) = context.skills.as_ref() {
                let snapshot = skills.lock().await.clone();
                let mut instructions = format!(
                    "{}\n\n# ACTIVE PLAN\n{}",
                    build_context(context.root, None, &snapshot)?.system,
                    context.session.plan_context()
                );
                if request.instructions.contains("# PLAN MODE") {
                    instructions.push_str("\n\n# PLAN MODE\nResearch and inspect only. Do not modify files, run mutating commands, or claim implementation.");
                }
                request.instructions = instructions;
            }
        }
    }
    anyhow::bail!("agent exceeded the eight-step tool-call bound")
}

fn image_input_for_tool(
    tool_name: &str,
    arguments: &serde_json::Value,
    guard: &PathGuard,
) -> Result<Option<ProviderInput>> {
    if !matches!(tool_name, "image_inspect" | "browser_screenshot") {
        return Ok(None);
    }
    let relative = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("vera-screenshot.png");
    let path = guard.resolve(Path::new(relative))?;
    if !path.exists() {
        return Ok(None);
    }
    let metadata = crate::browser::inspect_image(&path, guard.root())?;
    let mime_type = metadata
        .get("mime_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("application/octet-stream");
    let bytes = fs::read(&path)?;
    Ok(Some(ProviderInput::image_message(
        "user",
        format!("Image produced by {tool_name}: {}", path.display()),
        mime_type,
        base64::engine::general_purpose::STANDARD.encode(bytes),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ModelCatalog, ProviderResult};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct QuestionFixtureProvider {
        calls: AtomicUsize,
    }

    struct ShellFixtureProvider {
        calls: AtomicUsize,
    }

    struct ScriptedApproval;

    #[async_trait::async_trait]
    impl ApprovalHandler for ScriptedApproval {
        async fn ask(
            &mut self,
            _kind: PermissionKind,
            _description: &str,
        ) -> Result<crate::safety::ApprovalChoice> {
            Ok(crate::safety::ApprovalChoice::Once)
        }
    }

    #[async_trait::async_trait]
    impl Provider for QuestionFixtureProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::XaiOauth
        }

        async fn stream(
            &self,
            _request: ProviderRequest,
            sink: &mut dyn EventSink,
        ) -> Result<ProviderResult> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                sink.emit(Event::ToolCallDelta {
                    id: "question-call".into(),
                    name: "question".into(),
                    arguments: serde_json::json!({
                        "question_id":"fixture-question",
                        "question":"Choose a fixture path",
                        "choices":["one","two"]
                    })
                    .to_string(),
                })
                .await?;
                Ok(ProviderResult::default())
            } else {
                Ok(ProviderResult {
                    text: "resumed fixture answer".into(),
                    input_tokens: 2,
                    output_tokens: 3,
                })
            }
        }

        async fn models(&self) -> Result<ModelCatalog> {
            Ok(ModelCatalog::default())
        }
    }

    #[async_trait::async_trait]
    impl Provider for ShellFixtureProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::XaiOauth
        }

        async fn stream(
            &self,
            _request: ProviderRequest,
            sink: &mut dyn EventSink,
        ) -> Result<ProviderResult> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                sink.emit(Event::ToolCallDelta {
                    id: "shell-call".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command":"printf fixture"}).to_string(),
                })
                .await?;
                Ok(ProviderResult::default())
            } else {
                Ok(ProviderResult {
                    text: "shell completed".into(),
                    ..ProviderResult::default()
                })
            }
        }

        async fn models(&self) -> Result<ModelCatalog> {
            Ok(ModelCatalog::default())
        }
    }

    #[tokio::test]
    async fn agent_loop_persists_question_and_resumes_without_repeating_tool_call() -> Result<()> {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let paths = VeraPaths::from_home(temp.path().join("home")).unwrap();
        let mut session = SessionStore::new(paths).create_with_selection(
            root.clone(),
            CapabilitySelection {
                provider: "xai-oauth".into(),
                model: "fixture".into(),
                model_context_window: 10_000,
                ..CapabilitySelection::default()
            },
        )?;
        let guard = PathGuard::new(root.clone())?;
        let registry = ToolRegistry::standard();
        let provider = QuestionFixtureProvider {
            calls: AtomicUsize::new(0),
        };
        let mut policy = PermissionPolicy::default();
        let mut approval = ScriptedApproval;
        let first = run_agent_turn(
            &provider,
            ProviderRequest {
                model: "fixture".into(),
                input: vec![ProviderInput::message("user", "start")],
                tools: registry.schemas(),
                instructions: "fixture".into(),
                effort: None,
            },
            AgentRunContext {
                registry: &registry,
                guard: &guard,
                policy: &mut policy,
                approval: &mut approval,
                session: &mut session,
                output: OutputFormat::Jsonl,
                shell_timeout: 10,
                hooks: &[],
                root: &root,
                skills: None,
            },
        )
        .await?;
        assert!(matches!(
            first.control,
            Some(crate::tools::ToolResult::NeedsInput { .. })
        ));
        assert_eq!(
            session.pending_question.as_ref().unwrap().question_id,
            "fixture-question"
        );
        let mut continuation = session.answer_pending_question("one")?;
        continuation.push(ProviderInput::message("user", "one"));
        let second = run_agent_turn(
            &provider,
            ProviderRequest {
                model: "fixture".into(),
                input: continuation,
                tools: registry.schemas(),
                instructions: "fixture".into(),
                effort: None,
            },
            AgentRunContext {
                registry: &registry,
                guard: &guard,
                policy: &mut policy,
                approval: &mut approval,
                session: &mut session,
                output: OutputFormat::Jsonl,
                shell_timeout: 10,
                hooks: &[],
                root: &root,
                skills: None,
            },
        )
        .await?;
        assert_eq!(second.text, "resumed fixture answer");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        assert!(session.pending_question.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agent_loop_uses_scripted_approval_for_sandboxed_shell() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let paths = VeraPaths::from_home(temp.path().join("home"))?;
        let mut session = SessionStore::new(paths).create_with_selection(
            root.clone(),
            CapabilitySelection {
                provider: "xai-oauth".into(),
                model: "fixture".into(),
                model_context_window: 10_000,
                ..CapabilitySelection::default()
            },
        )?;
        let guard = PathGuard::new(root.clone())?;
        let registry = ToolRegistry::standard();
        let provider = ShellFixtureProvider {
            calls: AtomicUsize::new(0),
        };
        let mut policy = PermissionPolicy::default();
        let mut approval = ScriptedApproval;
        let result = run_agent_turn(
            &provider,
            ProviderRequest {
                model: "fixture".into(),
                input: vec![ProviderInput::message("user", "run the fixture")],
                tools: registry.schemas(),
                instructions: "fixture".into(),
                effort: None,
            },
            AgentRunContext {
                registry: &registry,
                guard: &guard,
                policy: &mut policy,
                approval: &mut approval,
                session: &mut session,
                output: OutputFormat::Jsonl,
                shell_timeout: 10,
                hooks: &[],
                root: &root,
                skills: None,
            },
        )
        .await?;
        assert_eq!(result.text, "shell completed");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_runtime_terminates_session_processes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let paths = VeraPaths::from_home(temp.path().join("home"))?;
        let mut session = SessionStore::new(paths).create_with_selection(
            root.clone(),
            CapabilitySelection {
                provider: "fixture".into(),
                model: "fixture".into(),
                model_context_window: 10_000,
                ..CapabilitySelection::default()
            },
        )?;
        let processes = Arc::new(crate::processes::ProcessManager::for_fixture());
        let snapshot = processes
            .start(crate::processes::ProcessStartRequest {
                command: "sleep 10".into(),
                cwd: root,
                environment: std::collections::BTreeMap::new(),
                network: false,
                columns: 80,
                rows: 24,
                session_id: session.header.id.clone(),
            })
            .await?;
        let registry = ToolRegistry::standard_with_skills_and_processes(None, processes.clone());
        cleanup_runtime(&[], &registry, &mut session).await?;
        let values = processes.list(Some(&session.header.id)).await;
        assert!(
            values
                .iter()
                .find(|value| value.process_id == snapshot.process_id)
                .is_some_and(|value| value.status != "running")
        );
        assert_eq!(
            session.process_lifecycle[&snapshot.process_id].state,
            "shutdown_requested"
        );
        Ok(())
    }
}
