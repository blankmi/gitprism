mod cli;
mod commands;
mod config;
mod exclude;

use clap::Parser;

use cli::{Cli, Commands};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Setup => commands::setup::run(&cli.config),
        Commands::Sync => commands::sync::run(&cli.config),
        Commands::Resolve { pair } => commands::resolve::run(&cli.config, pair),
    }
}
