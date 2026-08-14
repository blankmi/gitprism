//! Command-line surface for gitprism.
//!
//! Subcommands mirror the tool's design directly, not a general-purpose CLI
//! convention:
//!
//! - `setup`   — the one-time graft in decisions/0006.
//! - `sync`    — the recurring job in playbooks/0001; handles both sync
//!               directions for every configured branch pair in one run.
//! - `resolve` — the conflict-resolution helper in decisions/0008.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "gitprism", version, about, long_about = None)]
pub struct Cli {
    /// Path to the gitprism config file.
    ///
    /// Config format (repo locations, branch-pair list, committer identity)
    /// is not yet decided — see design/decisions/ for what *is* settled.
    #[arg(short, long, global = true, default_value = "gitprism.toml")]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Graft source's initial commit onto dest's real tip (decisions/0006).
    Setup,

    /// Sync every configured branch pair, both directions, in one run.
    ///
    /// Safe to invoke redundantly: resume and no-op detection come from the
    /// trailer-based history scan (decisions/0003), not from anything about
    /// how this command was triggered (playbooks/0001).
    Sync,

    /// Reproduce a dest<->source conflict and hand off to normal git
    /// conflict-resolution UX (decisions/0007, decisions/0008).
    Resolve {
        /// Which branch pair the conflict is on.
        ///
        /// Exact identifier shape (config key vs. raw branch names) is
        /// deferred — see decisions/0008's "Consequences".
        pair: String,
    },
}
