mod cli;
mod commands;
mod config;
mod exclude;
mod git;
mod progress;

use std::path::Path;

use clap::Parser;

use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Setup => commands::setup::run(Path::new("."), &cli.config),
        Commands::Sync => commands::sync::run(Path::new("."), &cli.config),
        Commands::Resolve { branch, r#continue } => {
            commands::resolve::run(Path::new("."), &cli.config, branch, *r#continue)
        }
    }
}
