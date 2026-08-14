//! `gitprism sync` — see design/playbooks/0001-gitlab-pipeline-triggers.md
//! and design/decisions/0003-mapping-state-in-commit-trailers.md.
//!
//! Not yet implemented past config loading: this is scaffolding only. The
//! real logic will, per configured branch pair: scan for the trailer-based
//! resume point, run source→dest (filtered, fast-forward-only) and
//! dest→source (merge back), and hard-stop just that pair on a real conflict
//! (decisions/0007).

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub fn run(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;

    anyhow::bail!(
        "gitprism sync: config loaded ({} pair(s)), but syncing is not yet implemented",
        config.pairs.len()
    )
}
