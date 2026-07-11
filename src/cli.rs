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
    pub output: OutputFormat,
    pub provider: Option<String>,
    pub model: Option<String>,
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
    Test { name: String },
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
        let mut output = OutputFormat::Text;
        let mut provider = None;
        let mut model = None;
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
                "-h" | "--help" => {
                    return Ok(Self {
                        command: Command::Help,
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                    });
                }
                "--version" | "-V" => {
                    return Ok(Self {
                        command: Command::Version,
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                    });
                }
                "auth" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                        parse_auth(&args[index + 1..])?,
                    ));
                }
                "models" => {
                    let refresh = args.get(index + 1).is_some_and(|arg| arg == "--refresh");
                    return Ok(Self {
                        command: Command::Models { refresh },
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                    });
                }
                "session" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                        parse_session(&args[index + 1..])?,
                    ));
                }
                "inspect" => {
                    return Ok(Self {
                        command: Command::Inspect,
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                    });
                }
                "mcp" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                        parse_mcp(&args[index + 1..])?,
                    ));
                }
                "plugin" => {
                    return Ok(Self::with_subcommand(
                        path,
                        prompt,
                        output,
                        provider,
                        model,
                        parse_plugin(&args[index + 1..])?,
                    ));
                }
                "update" => {
                    return Ok(Self {
                        command: Command::Update,
                        path,
                        prompt,
                        output,
                        provider,
                        model,
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
            command: if prompt.is_some() {
                Command::Prompt
            } else {
                Command::Interactive
            },
            path,
            prompt,
            output,
            provider,
            model,
        })
    }

    fn with_subcommand(
        path: PathBuf,
        prompt: Option<String>,
        output: OutputFormat,
        provider: Option<String>,
        model: Option<String>,
        command: Command,
    ) -> Self {
        Self {
            command,
            path,
            prompt,
            output,
            provider,
            model,
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
        Some("test") => Ok(Command::Mcp(McpCommand::Test {
            name: args
                .get(1)
                .ok_or_else(|| VeraError::Cli("mcp test requires a name".into()))?
                .clone(),
        })),
        _ => Err(VeraError::Cli("use mcp list|test <name>".into())),
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
}
