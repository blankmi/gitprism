mod cli;
mod commands;
mod config;
mod exclude;
mod git;
mod limits;
mod lock;
mod marker;
mod policy;
mod progress;
#[cfg(test)]
pub(crate) mod testutil;

use std::path::Path;

use clap::Parser;

use cli::{Cli, Commands, ResolveDirection};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Setup => {
            commands::setup::run_with(Path::new("."), &cli.config, &commands::EnvSecrets)
        }
        Commands::Sync { recreate_dest } => commands::sync::run_with_options(
            Path::new("."),
            &cli.config,
            &commands::EnvSecrets,
            &commands::sync::SyncOptions::new(recreate_dest.iter().cloned()),
        ),
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
            &commands::EnvSecrets,
        ),
        Commands::PolicyHash => commands::policy_hash::run(Path::new("."), &cli.config),
    }
}
