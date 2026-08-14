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
    /// Path to the gitprism config file (decisions/0012).
    ///
    /// For every command except `setup`, this resolves against source's
    /// checked-out tree, since the config is versioned there. `setup` is the
    /// exception: it reads this same path off disk *before* source has a
    /// first commit to version it in — see decisions/0012's bootstrap note.
    #[arg(short, long, global = true, default_value = crate::config::FILENAME)]
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
    /// conflict-resolution UX (decisions/0007, decisions/0008, decisions/0015).
    Resolve {
        /// Which branch pair the conflict is on, identified by its
        /// configured `source_branch` — the same identifier `sync`'s own
        /// conflict error prints (decisions/0015).
        pair: String,

        /// Finish a cherry-pick already started by a prior `gitprism resolve
        /// <pair>` run, once the human has resolved its conflicts and `git
        /// add`ed them. Explicit, matching git's own `rebase`/`cherry-pick`/
        /// `merge --continue` convention rather than having the bare command
        /// guess start-vs-resume from repo state (decisions/0015).
        #[arg(long)]
        r#continue: bool,
    },
}
