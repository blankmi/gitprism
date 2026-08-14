//! `gitprism resolve` — see design/decisions/0008-ship-resolve-helper.md.
//!
//! Not yet implemented past config loading: this is scaffolding only. The
//! real logic will fetch, start the cherry-pick for the human, hand off to
//! normal git conflict resolution, then append the `Gitprism-Dest-Commit`
//! trailer and push.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub fn run(config_path: &Path, pair: &str) -> Result<()> {
    let config = Config::load(config_path)?;

    // Provisional: identifying a pair by its source_branch. decisions/0008's
    // "Consequences" explicitly defers the exact identifier shape, so this
    // isn't a design decision — just the simplest thing that lets this stub
    // demonstrate config validation.
    if !config.pairs.iter().any(|p| p.source_branch == pair) {
        anyhow::bail!("gitprism resolve {pair}: no configured pair has source_branch {pair:?}");
    }

    anyhow::bail!("gitprism resolve {pair}: not yet implemented")
}
