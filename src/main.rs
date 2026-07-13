#![allow(dead_code)]

mod app;
mod auth;
mod browser;
mod cli;
mod config;
mod error;
mod events;
mod extensions;
mod keybindings;
mod paths;
mod processes;
mod prompt;
mod providers;
mod safety;
mod sessions;
mod settings;
mod subagents;
mod tools;
mod ui;
mod worktrees;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    app::run(cli::CommandLine::parse()).await
}
