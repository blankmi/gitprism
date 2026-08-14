//! `gitprism setup` — see design/decisions/0006-setup-uses-real-shared-history.md
//! and design/decisions/0012-config-versioned-in-source.md.
//!
//! Not yet implemented past config loading: this is scaffolding only. The
//! real logic will fetch dest into source's local object database, then
//! create source's initial commit — dest's tree, plus this same config file
//! and `.gitprismignore` — as a real child of dest's tip commit.

use std::path::Path;

use anyhow::Result;

use crate::config::Config;

pub fn run(config_path: &Path) -> Result<()> {
    // setup is the one command that reads config off disk rather than from
    // a checked-out tree — there's no source commit to check it out from
    // yet (decisions/0012's bootstrap note).
    let config = Config::load(config_path)?;

    anyhow::bail!(
        "gitprism setup: config loaded for dest {:?} ({} pair(s)), but the graft itself is not yet implemented",
        config.dest.url,
        config.pairs.len()
    )
}
