//! `gitprism resolve` — see design/decisions/0008-ship-resolve-helper.md.
//!
//! Not yet implemented: this is scaffolding only. The real logic will fetch,
//! start the cherry-pick for the human, hand off to normal git conflict
//! resolution, then append the `Gitprism-Dest-Commit` trailer and push.

use std::path::Path;

use anyhow::Result;

pub fn run(_config: &Path, pair: &str) -> Result<()> {
    anyhow::bail!("gitprism resolve {pair}: not yet implemented")
}
