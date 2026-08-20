//! `gitprism policy-hash` — print the deployment pin for the checked-out
//! `.gitprism.toml` and root `.gitprismignore` bytes.

use std::path::Path;

use anyhow::{Context, Result};
use git2::Repository;

use crate::exclude;
use crate::policy;

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism policy-hash must be run inside an existing git repository (none found at or above {})",
            cwd.display()
        )
    })?;
    let source_root = repo
        .workdir()
        .context("gitprism policy-hash requires a repo with a working tree, not a bare repo")?;
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        source_root.join(config_path)
    };
    let digest = policy::hash_files(&config_path, &source_root.join(exclude::FILENAME))?;
    println!("{digest}");
    Ok(())
}
