mod cli;
mod commands;
mod config;
mod exclude;
mod git;
mod lock;
mod marker;
mod policy;
mod progress;

use std::path::Path;

use clap::Parser;

use cli::{Cli, Commands, ResolveDirection};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Setup => commands::setup::run(Path::new("."), &cli.config),
        Commands::Sync => commands::sync::run(Path::new("."), &cli.config),
        Commands::Resolve {
            branch,
            direction,
            r#continue,
        } => commands::resolve::run_with_direction(
            Path::new("."),
            &cli.config,
            branch,
            *r#continue,
            match direction {
                ResolveDirection::DestToSource => commands::resolve::Direction::DestToSource,
                ResolveDirection::SourceToDest => commands::resolve::Direction::SourceToDest,
            },
        ),
        Commands::PolicyHash => commands::policy_hash::run(Path::new("."), &cli.config),
    }
}
