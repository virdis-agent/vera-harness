use std::env;
use std::path::PathBuf;

use crate::error::VeraError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Text,
    Jsonl,
}

#[derive(Debug)]
pub struct CommandLine {
    pub command: Command,
    pub path: PathBuf,
    pub prompt: Option<String>,
    pub prompt_template: Option<String>,
    pub prompt_args: Option<String>,
    pub output: OutputFormat,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug)]
pub enum Command {
    Interactive,
    Prompt,
    Auth(AuthCommand),
    Models { refresh: bool },
    Session(SessionCommand),
    Inspect,
    Mcp(McpCommand),
    Permissions(PermissionsCommand),
    Plugin(PluginCommand),
    Update,
    Help,
    Version,
}

#[derive(Debug)]
pub enum AuthCommand {
    Login { provider: String, no_browser: bool },
    Status,
    Logout { provider: Option<String> },
}

#[derive(Debug)]
pub enum SessionCommand {
    List,
    Resume { id: String },
}

#[derive(Debug)]
pub enum McpCommand {
    List,
    Tools { server: String },
    Enable { server: String },
    Disable { server: String },
    Test { name: String },
}

#[derive(Debug)]
pub enum PermissionsCommand {
    List,
    Check { kind: String },
}

#[derive(Debug)]
pub enum PluginCommand {
    Add { source: PathBuf },
    List,
    Remove { name: String },
}

impl CommandLine {
    pub fn parse() -> Self {
        match Self::try_parse(env::args().skip(1).collect()) {
            Ok(cli) => cli,
            Err(error) => {
                eprintln!("vera: {error}");
                std::process::exit(2);
            }
        }
    }

    pub fn try_parse(args: Vec<String>) -> Result<Self, VeraError> {
        let mut path = PathBuf::from(".");
        let mut prompt = None;
        let mut prompt_template = None;
        let mut prompt_args = None;
        let mut output = OutputFormat::Text;
        let mut provider = None;
        let mut model = None;
        let mut effort = None;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "-p" | "--prompt" => {
                    index += 1;
                    prompt = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing prompt".into()))?
                            .clone(),
                    );
                }
                "--prompt-template" | "--prompt-name" => {
                    index += 1;
                    prompt_template = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing prompt template name".into()))?
                            .clone(),
                    );
                }
                "--prompt-args" | "--args" => {
                    index += 1;
                    prompt_args = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing prompt arguments".into()))?
                            .clone(),
                    );
                }
                "--output" => {
                    index += 1;
                    output = match args.get(index).map(String::as_str) {
                        Some("text") => OutputFormat::Text,
                        Some("jsonl") => OutputFormat::Jsonl,
                        Some(other) => {
                            return Err(VeraError::Cli(format!("unknown output format {other}")));
                        }
                        None => return Err(VeraError::Cli("missing output format".into())),
                    };
                }
                "--provider" => {
                    index += 1;
                    provider = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing provider".into()))?
                            .clone(),
                    );
                }
                "--model" => {
                    index += 1;
                    model = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing model".into()))?
                            .clone(),
                    );
                }
                "--effort" => {
                    index += 1;
                    effort = Some(
                        args.get(index)
                            .ok_or_else(|| VeraError::Cli("missing effort".into()))?
                            .clone(),
                    );
                }
                "-h" | "--help" => {
                    return Ok(Self {
                        command: Command::Help,
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                    });
                }
                "--version" | "-V" => {
                    return Ok(Self {
                        command: Command::Version,
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                    });
                }
                "auth" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                        parse_auth(&args[index + 1..])?,
                    ));
                }
                "models" => {
                    let refresh = args.get(index + 1).is_some_and(|arg| arg == "--refresh");
                    return Ok(Self {
                        command: Command::Models { refresh },
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                    });
                }
                "session" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                        parse_session(&args[index + 1..])?,
                    ));
                }
                "inspect" => {
                    return Ok(Self {
                        command: Command::Inspect,
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                    });
                }
                "mcp" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                        parse_mcp(&args[index + 1..])?,
                    ));
                }
                "permissions" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                        parse_permissions(&args[index + 1..])?,
                    ));
                }
                "plugin" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                        parse_plugin(&args[index + 1..])?,
                    ));
                }
                "update" | "upgrade" => {
                    return Ok(Self {
                        command: Command::Update,
                        path,
                        prompt,
                        prompt_template,
                        prompt_args,
                        output,
                        provider,
                        model,
                        effort,
                    });
                }
                arg if !arg.starts_with('-') && index == args.len() - 1 => {
                    path = PathBuf::from(arg)
                }
                arg => return Err(VeraError::Cli(format!("unexpected argument {arg}"))),
            }
            index += 1;
        }
        Ok(Self {
            command: if prompt.is_some() || prompt_template.is_some() {
                Command::Prompt
            } else {
                Command::Interactive
            },
            path,
            prompt,
            prompt_template,
            prompt_args,
            output,
            provider,
            model,
            effort,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn with_subcommand(
        path: PathBuf,
        prompt: Option<String>,
        prompt_template: Option<String>,
        prompt_args: Option<String>,
        output: OutputFormat,
        provider: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        command: Command,
    ) -> Self {
        Self {
            command,
            path,
            prompt,
            prompt_template,
            prompt_args,
            output,
            provider,
            model,
            effort,
        }
    }
}

fn parse_auth(args: &[String]) -> Result<Command, VeraError> {
    match args.first().map(String::as_str) {
        Some("login") => {
            let provider = args
                .get(1)
                .ok_or_else(|| VeraError::Cli("auth login requires a provider".into()))?
                .clone();
            Ok(Command::Auth(AuthCommand::Login {
                provider,
                no_browser: args.iter().any(|arg| arg == "--no-browser"),
            }))
        }
        Some("status") => Ok(Command::Auth(AuthCommand::Status)),
        Some("logout") => Ok(Command::Auth(AuthCommand::Logout {
            provider: args.get(1).cloned(),
        })),
        _ => Err(VeraError::Cli("use auth login|status|logout".into())),
    }
}

fn parse_session(args: &[String]) -> Result<Command, VeraError> {
    match args.first().map(String::as_str) {
        Some("list") => Ok(Command::Session(SessionCommand::List)),
        Some("resume") => Ok(Command::Session(SessionCommand::Resume {
            id: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("session resume requires an id".into()))?
                .clone(),
        })),
        _ => Err(VeraError::Cli("use session list|resume <id>".into())),
    }
}

fn parse_mcp(args: &[String]) -> Result<Command, VeraError> {
    match args.first().map(String::as_str) {
        Some("list") => Ok(Command::Mcp(McpCommand::List)),
        Some("tools") => Ok(Command::Mcp(McpCommand::Tools {
            server: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("mcp tools requires a server".into()))?
                .clone(),
        })),
        Some("enable") => Ok(Command::Mcp(McpCommand::Enable {
            server: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("mcp enable requires a server".into()))?
                .clone(),
        })),
        Some("disable") => Ok(Command::Mcp(McpCommand::Disable {
            server: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("mcp disable requires a server".into()))?
                .clone(),
        })),
        Some("test") => Ok(Command::Mcp(McpCommand::Test {
            name: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("mcp test requires a name".into()))?
                .clone(),
        })),
        _ => Err(VeraError::Cli(
            "use mcp list|tools|enable|disable|test <name>".into(),
        )),
    }
}

fn parse_permissions(args: &[String]) -> Result<Command, VeraError> {
    match args.first().map(String::as_str) {
        Some("list") => Ok(Command::Permissions(PermissionsCommand::List)),
        Some("check") => Ok(Command::Permissions(PermissionsCommand::Check {
            kind: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("permissions check requires a kind".into()))?
                .clone(),
        })),
        _ => Err(VeraError::Cli("use permissions list|check <kind>".into())),
    }
}

fn parse_plugin(args: &[String]) -> Result<Command, VeraError> {
    match args.first().map(String::as_str) {
        Some("add") => {
            Ok(Command::Plugin(PluginCommand::Add {
                source: PathBuf::from(args.get(1).ok_or_else(|| {
                    VeraError::Cli("plugin add requires a local directory".into())
                })?),
            }))
        }
        Some("list") => Ok(Command::Plugin(PluginCommand::List)),
        Some("remove") => Ok(Command::Plugin(PluginCommand::Remove {
            name: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("plugin remove requires a name".into()))?
                .clone(),
        })),
        _ => Err(VeraError::Cli("use plugin add|list|remove".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headless_jsonl_prompt() {
        let cli = CommandLine::try_parse(vec![
            "-p".into(),
            "inspect this".into(),
            "--output".into(),
            "jsonl".into(),
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Prompt));
        assert_eq!(cli.prompt.as_deref(), Some("inspect this"));
        assert_eq!(cli.output, OutputFormat::Jsonl);
    }

    #[test]
    fn parses_auth_login_without_browser() {
        let cli = CommandLine::try_parse(vec![
            "auth".into(),
            "login".into(),
            "xai-oauth".into(),
            "--no-browser".into(),
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Auth(AuthCommand::Login {
                no_browser: true,
                ..
            })
        ));
    }

    #[test]
    fn parses_upgrade_alias() {
        let cli = CommandLine::try_parse(vec!["upgrade".into()]).unwrap();
        assert!(matches!(cli.command, Command::Update));
    }

    #[test]
    fn parses_prompt_template_flags() {
        let cli = CommandLine::try_parse(vec![
            "--prompt-template".into(),
            "review".into(),
            "--prompt-args".into(),
            "the diff".into(),
        ])
        .unwrap();
        assert!(matches!(cli.command, Command::Prompt));
        assert_eq!(cli.prompt_template.as_deref(), Some("review"));
        assert_eq!(cli.prompt_args.as_deref(), Some("the diff"));
    }

    #[test]
    fn parses_headless_effort_override() {
        let cli = CommandLine::try_parse(vec![
            "-p".into(),
            "hello".into(),
            "--model".into(),
            "gpt-5".into(),
            "--effort".into(),
            "low".into(),
        ])
        .unwrap();
        assert_eq!(cli.effort.as_deref(), Some("low"));
        assert_eq!(cli.model.as_deref(), Some("gpt-5"));
    }
}
