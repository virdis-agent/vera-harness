use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VeraError {
    #[error("invalid command line: {0}")]
    Cli(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("unsafe path: {0}")]
    UnsafePath(PathBuf),
    #[error("session error: {0}")]
    Session(String),
    #[error("tool error: {0}")]
    Tool(String),
}
