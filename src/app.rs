use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use reqwest::header::LOCATION;
use sha2::{Digest, Sha256};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::auth::{AuthProvider, OAuthClient, TokenRecord, TokenStore, now_seconds, pkce_pair};
use crate::cli::{
    AuthCommand, Command, CommandLine, McpCommand, OutputFormat, PluginCommand, SessionCommand,
};
use crate::config::Config;
use crate::events::{Event, EventSink, TerminalEventSink};
use crate::extensions::{McpRegistry, PluginManager, SkillCatalog, discover_agents};
use crate::paths::{VeraPaths, repository_root};
use crate::prompt::{approximate_tokens, build_context};
use crate::providers::{
    Provider, ProviderKind, ProviderRequest, ResponsesProvider, provider_catalog,
};
use crate::safety::{
    PathGuard, PermissionKind, PermissionMode, PermissionPolicy, TerminalApproval,
};
use crate::sessions::SessionStore;
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
        Command::Session(command) => run_session(&paths, command)?,
        Command::Inspect => inspect(&paths, &root, &config)?,
        Command::Mcp(command) => run_mcp(&paths, command).await?,
        Command::Plugin(command) => run_plugin(&paths, command).await?,
        Command::Update => run_upgrade(&paths).await?,
        Command::Prompt => {
            let prompt = cli.prompt.context("prompt is required")?;
            run_headless(
                &paths,
                &root,
                &config,
                cli.provider.as_deref(),
                cli.model.as_deref(),
                &prompt,
                cli.output,
            )
            .await?;
        }
        Command::Interactive => {
            run_interactive(&paths, &root, config, cli.provider, cli.model).await?
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
            while now_seconds() < deadline {
                match oauth.poll(provider, &device, verifier.as_deref()).await {
                    Ok(token) => {
                        store.put(token)?;
                        println!("login complete");
                        return Ok(());
                    }
                    Err(error) => last_error = Some(error.to_string()),
                }
                sleep(Duration::from_secs(device.interval.max(2))).await;
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
    let store = TokenStore::new(paths.clone());
    if let Some(token) = usable_token(&store, kind).await? {
        let provider = ResponsesProvider::new(kind, token)?;
        if refresh {
            println!("{}", provider.models().await?.join("\n"));
            return Ok(());
        }
    }
    if refresh {
        println!(
            "No valid {} login; showing the versioned local catalog.",
            kind.as_str()
        );
    }
    for (provider, models) in provider_catalog() {
        println!("{provider}: {}", models.join(", "));
    }
    Ok(())
}

fn run_session(paths: &VeraPaths, command: SessionCommand) -> Result<()> {
    let store = SessionStore::new(paths.clone());
    match command {
        SessionCommand::List => {
            for header in store.list()? {
                println!(
                    "{}\t{}\t{}\t{}",
                    header.id,
                    header.created_at,
                    header.provider,
                    header.root.display()
                );
            }
        }
        SessionCommand::Resume { id } => {
            let session = store.open(&id)?;
            println!(
                "session {} ({}) — {} messages",
                session.header.id,
                session.header.model,
                session.messages.len()
            );
            for message in session.messages {
                println!("{}: {}", message.role, message.content);
            }
        }
    }
    Ok(())
}

fn inspect(paths: &VeraPaths, root: &Path, config: &Config) -> Result<()> {
    let skills = SkillCatalog::load(paths, root)?;
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
        println!("  {name}");
    }
    println!(
        "base prompt tokens: {}",
        approximate_tokens(crate::prompt::STATIC_SYSTEM_PROMPT)
    );
    println!(
        "tools: {}",
        ToolRegistry::standard()
            .schemas()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

async fn run_mcp(paths: &VeraPaths, command: McpCommand) -> Result<()> {
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
        McpCommand::Test { name } => {
            let mut policy = PermissionPolicy::default();
            let mut approval = TerminalApproval;
            println!(
                "{}",
                registry.test(&name, &mut policy, &mut approval).await?
            );
        }
    }
    Ok(())
}

async fn run_plugin(paths: &VeraPaths, command: PluginCommand) -> Result<()> {
    let manager = PluginManager::new(paths.clone());
    match command {
        PluginCommand::List => {
            for plugin in manager.list()? {
                println!("{}\t{}", plugin.name, plugin.version);
            }
        }
        PluginCommand::Remove { name } => {
            let mut policy = PermissionPolicy::default();
            let mut approval = TerminalApproval;
            policy
                .authorize(
                    PermissionKind::Plugin,
                    &format!("remove plugin {name}"),
                    &mut approval,
                    None,
                )
                .await?;
            manager.remove(&name)?;
            println!("removed {name}");
        }
        PluginCommand::Add { source } => {
            let mut policy = PermissionPolicy::default();
            let mut approval = TerminalApproval;
            policy
                .authorize(
                    PermissionKind::Plugin,
                    &format!("install plugin from {}", source.display()),
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

async fn run_headless(
    paths: &VeraPaths,
    root: &Path,
    config: &Config,
    provider_override: Option<&str>,
    model_override: Option<&str>,
    prompt: &str,
    output: OutputFormat,
) -> Result<()> {
    let provider_name = provider_override.unwrap_or(&config.provider);
    let model = model_override.unwrap_or(&config.model);
    let kind = ProviderKind::parse(provider_name)?;
    let store = TokenStore::new(paths.clone());
    let token = usable_token(&store, kind).await?.context(format!(
        "not logged in to {}; run vera auth login {}",
        kind.as_str(),
        kind.as_str()
    ))?;
    let provider = ResponsesProvider::new(kind, token)?;
    let skills = SkillCatalog::load(paths, root)?;
    let context = build_context(root, None, &skills)?;
    let mut session = SessionStore::new(paths.clone()).create(
        root.to_path_buf(),
        provider_name.into(),
        model.into(),
    )?;
    session.add_message("user", prompt)?;
    let guard = PathGuard::new(root.to_path_buf())?;
    let registry = ToolRegistry::standard();
    let mut approval = TerminalApproval;
    let mut policy = PermissionPolicy::default();
    let messages = session
        .messages
        .iter()
        .map(|message| crate::providers::ChatMessage {
            role: message.role.clone(),
            content: message.content.clone(),
        })
        .collect();
    let response = run_agent_turn(
        &provider,
        ProviderRequest {
            model: model.into(),
            messages,
            tools: registry.schemas(),
            instructions: context.system,
        },
        AgentRunContext {
            registry: &registry,
            guard: &guard,
            policy: &mut policy,
            approval: &mut approval,
            session: &mut session,
            output,
            shell_timeout: config.shell_timeout_seconds,
        },
    )
    .await?;
    if !response.is_empty() {
        session.add_message("assistant", response)?;
    }
    Ok(())
}

async fn run_interactive(
    paths: &VeraPaths,
    root: &Path,
    mut config: Config,
    provider_override: Option<String>,
    model_override: Option<String>,
) -> Result<()> {
    if let Some(provider) = provider_override {
        config.provider = provider;
    }
    if let Some(model) = model_override {
        config.model = model;
    }
    let store = SessionStore::new(paths.clone());
    let mut session = store.create(
        root.to_path_buf(),
        config.provider.clone(),
        config.model.clone(),
    )?;
    let skills = SkillCatalog::load(paths, root)?;
    let context = build_context(root, None, &skills)?;
    let guard = PathGuard::new(root.to_path_buf())?;
    let registry = ToolRegistry::standard();
    let mut policy = PermissionPolicy::default();
    let mut approval = TerminalApproval;
    let root_path = root.to_path_buf();
    let prompts = vec![
        "/commands".into(),
        "/provider <id>".into(),
        "/model <id>".into(),
        "/plan".into(),
        "/compact".into(),
        "/undo".into(),
    ];
    let mut ctrl_c_pending = false;
    loop {
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
        let skill_names = skills.names().cloned().collect::<Vec<_>>();
        let mut extensions = vec![
            "hooks".into(),
            "stdio MCP".into(),
            "bounded subagents".into(),
        ];
        extensions.extend(
            PluginManager::new(paths.clone())
                .list()?
                .into_iter()
                .map(|plugin| format!("plugin:{}", plugin.name)),
        );
        let mcp_servers = McpRegistry::new(paths.clone()).list()?.len();
        let frame = render_dashboard(&Dashboard {
            version: env!("CARGO_PKG_VERSION"),
            root: &root_path,
            instructions: &instructions,
            skills: &skill_names,
            prompts: &prompts,
            extensions: &extensions,
            mcp_servers,
            provider: &config.provider,
            model: &config.model,
            context_tokens: session.context_tokens(),
            context_limit: config.context_window_tokens,
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
        match line {
            "/quit" | "/exit" => break,
            "/help" | "/commands" => print_interactive_help(),
            "/plan" => {
                policy.set_mode(PermissionMode::Plan);
                println!("mode: {}", policy.mode().label());
            }
            "/confirm" => {
                policy.set_mode(PermissionMode::Confirm);
                println!("mode: {}", policy.mode().label());
            }
            "/auto" => {
                policy.set_mode(PermissionMode::Auto);
                println!("mode: {}", policy.mode().label());
            }
            "/yolo" => {
                policy.set_mode(PermissionMode::Yolo);
                println!("mode: {}", policy.mode().label());
            }
            "/permissions" => println!(
                "mode={}; Plan=read-only; Confirm=ask; Auto=non-risky/external auto; Yolo=never ask",
                policy.mode().label()
            ),
            "/context" => println!(
                "{} tokens; {} instruction file(s); {} skill(s)",
                session.context_tokens(),
                context.instructions.len(),
                context.skills.len()
            ),
            "/compact" => println!(
                "compacted: {}",
                session.compact(config.context_window_tokens)?
            ),
            "/diff" => println!(
                "Use the git_status_diff tool in the next turn; the repository remains unchanged by this command."
            ),
            "/undo" => println!("restored {} Vera-managed file(s)", session.undo()?),
            "/skills" => {
                for name in skills.names() {
                    println!("{name}");
                }
            }
            "/agents" => {
                for path in discover_agents(root, None)? {
                    println!("{}", path.display());
                }
            }
            "/mcp" => {
                for server in McpRegistry::new(paths.clone()).list()? {
                    println!("{}", server.name);
                }
            }
            command if command.starts_with("/provider ") => {
                config.provider = command[10..].trim().into();
                println!("provider: {}", config.provider);
            }
            command if command.starts_with("/model ") => {
                config.model = command[7..].trim().into();
                println!("model: {}", config.model);
            }
            command if command.starts_with("/resume ") => {
                session = store.open(line[8..].trim())?;
                println!("resumed {}", session.header.id);
            }
            prompt => {
                session.add_message("user", prompt)?;
                let kind = ProviderKind::parse(&config.provider)?;
                let token = usable_token(&TokenStore::new(paths.clone()), kind)
                    .await?
                    .context(format!(
                        "not logged in; run vera auth login {}",
                        kind.as_str()
                    ))?;
                let provider = ResponsesProvider::new(kind, token)?;
                let plan_mode = policy.mode() == PermissionMode::Plan;
                let instructions = if plan_mode {
                    format!(
                        "{}\n\n# PLAN MODE\nResearch and inspect only. Do not modify files, run mutating commands, or claim implementation. Produce a structured plan with findings, assumptions, affected files, validation steps, and risks.",
                        context.system
                    )
                } else {
                    context.system.clone()
                };
                let response = run_agent_turn(
                    &provider,
                    ProviderRequest {
                        model: config.model.clone(),
                        messages: session
                            .messages
                            .iter()
                            .map(|message| crate::providers::ChatMessage {
                                role: message.role.clone(),
                                content: message.content.clone(),
                            })
                            .collect(),
                        tools: if plan_mode {
                            registry.read_only_schemas()
                        } else {
                            registry.schemas()
                        },
                        instructions,
                    },
                    AgentRunContext {
                        registry: &registry,
                        guard: &guard,
                        policy: &mut policy,
                        approval: &mut approval,
                        session: &mut session,
                        output: OutputFormat::Text,
                        shell_timeout: config.shell_timeout_seconds,
                    },
                )
                .await?;
                if !response.is_empty() {
                    session.add_message("assistant", response.clone())?;
                    if plan_mode {
                        print_plan_prompts(prompt, &response);
                        session.append(crate::sessions::SessionRecord::Event {
                            event: serde_json::json!({
                                "kind": "plan_draft",
                                "objective": prompt,
                                "draft": response,
                            }),
                        })?;
                    }
                }
                session.compact_if_needed(config.context_window_tokens)?;
            }
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
        "vera [path]\nvera -p \"prompt\" --output text|jsonl\n\nCommands:\n  auth login|status|logout\n  models [--refresh]\n  session list|resume <id>\n  inspect\n  mcp list|test <name>\n  plugin add|list|remove\n  update|upgrade"
    );
}

fn print_interactive_help() {
    println!(
        "shift+tab cycle modes  /plan  /confirm  /auto  /yolo  /provider <id>  /model <id>  /permissions  /compact  /context  /diff  /undo  /resume <id>  /skills  /mcp  /agents  /quit"
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
    approval: &'a mut TerminalApproval,
    session: &'a mut crate::sessions::Session,
    output: OutputFormat,
    shell_timeout: u64,
}

async fn run_agent_turn(
    provider: &ResponsesProvider,
    mut request: ProviderRequest,
    context: AgentRunContext<'_>,
) -> Result<String> {
    let mut answer = String::new();
    for _ in 0..8 {
        let mut sink = AgentSink::new(context.output);
        let result = provider.stream(request.clone(), &mut sink).await?;
        answer.push_str(&result.text);
        if sink.calls.is_empty() {
            return Ok(answer);
        }
        for (id, call) in sink.calls {
            let arguments =
                serde_json::from_str(&call.arguments).unwrap_or_else(|_| serde_json::json!({}));
            let tool_name = if call.name.is_empty() {
                "unknown"
            } else {
                &call.name
            };
            let tool_result = if context.registry.find(tool_name).is_some() {
                execute(
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
                .await?
            } else {
                crate::tools::ToolResult {
                    content: format!("unknown tool {tool_name}"),
                    is_error: true,
                }
            };
            context
                .session
                .append(crate::sessions::SessionRecord::ToolCall {
                    id,
                    name: tool_name.into(),
                    arguments,
                    result: Some(tool_result.content.clone()),
                })?;
            request.messages.push(crate::providers::ChatMessage {
                role: "tool".into(),
                content: tool_result.content,
            });
        }
    }
    anyhow::bail!("agent exceeded the eight-step tool-call bound")
}
