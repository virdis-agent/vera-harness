use std::io;
use std::path::Path;

use anyhow::{Context, Result};
use tokio::time::{Duration, sleep};

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
use crate::safety::{PathGuard, PermissionKind, PermissionPolicy, TerminalApproval};
use crate::sessions::SessionStore;
use crate::tools::{ToolCall, ToolContext, ToolRegistry, execute};
use crate::ui::{Dashboard, render_dashboard};

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
        Command::Update => println!(
            "Update Vera through Homebrew (`brew upgrade vera-harness`) or rerun the verified installer."
        ),
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
        render_dashboard(&Dashboard {
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
            plan_mode: policy.plan_mode,
        })?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line {
            "/quit" | "/exit" => break,
            "/help" | "/commands" => print_interactive_help(),
            "/plan" => {
                policy.set_plan_mode(!policy.plan_mode);
                println!("plan mode: {}", policy.plan_mode);
            }
            "/permissions" => println!(
                "read=automatic; writes/shell/network/hooks/plugins/MCP require approval; plan mode={}",
                policy.plan_mode
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
                        tools: registry.schemas(),
                        instructions: context.system.clone(),
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
                    session.add_message("assistant", response)?;
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
        "vera [path]\nvera -p \"prompt\" --output text|jsonl\n\nCommands:\n  auth login|status|logout\n  models [--refresh]\n  session list|resume <id>\n  inspect\n  mcp list|test <name>\n  plugin add|list|remove\n  update"
    );
}

fn print_interactive_help() {
    println!(
        "/provider <id>  /model <id>  /plan  /permissions  /compact  /context  /diff  /undo  /resume <id>  /skills  /mcp  /agents  /quit"
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
