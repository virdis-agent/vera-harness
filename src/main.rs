#![allow(dead_code)]

mod app;
mod auth;
mod cli;
mod config;
mod error;
mod events;
mod extensions;
mod paths;
mod prompt;
mod providers;
mod safety;
mod sessions;
mod tools;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    app::run(cli::CommandLine::parse()).await
}
