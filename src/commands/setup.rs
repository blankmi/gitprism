//! `gitprism setup` — see design/decisions/0006-setup-uses-real-shared-history.md.
//!
//! Not yet implemented: this is scaffolding only. The real logic will fetch
//! dest into source's local object database, then create source's initial
//! commit as a real child of dest's tip commit.

use std::path::Path;

use anyhow::Result;

pub fn run(_config: &Path) -> Result<()> {
    anyhow::bail!("gitprism setup: not yet implemented")
}
