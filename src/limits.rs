//! Static resource limits for repository-controlled input.
//!
//! These limits are intentionally process constants, not repository policy:
//! an untrusted checkout cannot raise them. Callers fail clearly when a
//! checked-out repository exceeds a limit rather than silently dropping data.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};

pub(crate) const MAX_CONTROL_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_STATE_FILE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COMMIT_MESSAGE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_CONFIG_BRANCHES: usize = 1_024;
pub(crate) const MAX_SOURCE_BRANCHES: usize = 4_096;
pub(crate) const MAX_TREE_ENTRIES: usize = 1_000_000;
pub(crate) const MAX_TREE_DEPTH: usize = 256;
pub(crate) const MAX_COLLISION_PATHS: usize = 1_000_000;
pub(crate) const MAX_PENDING_COMMITS: usize = 10_000;
pub(crate) const MAX_MARKER_SCAN_COMMITS: usize = 100_000;
pub(crate) const MAX_MAPPING_ENTRIES: usize = 100_000;
pub(crate) const MAX_CONFLICT_RECORDS: usize = 100_000;
pub(crate) const MAX_CONFLICT_PATH_BYTES: usize = 8 * 1024 * 1024;

/// Read a regular, non-symlink file without allocating based on an
/// attacker-controlled length. The metadata size is checked before the first
/// allocation and every read is checked again in case the file grows.
pub(crate) fn read_regular_file(path: &Path, limit: usize, description: &str) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("reading {description} at {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("reading {description} at {}", path.display()))?;
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {description} at {}", path.display()))?;
    if !metadata.file_type().is_file()
        || !path_metadata.file_type().is_file()
        || path_metadata.file_type().is_symlink()
    {
        anyhow::bail!("{description} at {} is not a regular file", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        if metadata.dev() != path_metadata.dev() || metadata.ino() != path_metadata.ino() {
            anyhow::bail!(
                "{description} at {} changed while it was being opened",
                path.display()
            );
        }
        if metadata.nlink() > 1 {
            anyhow::bail!(
                "{description} at {} is hard-linked and cannot be read safely",
                path.display()
            );
        }
    }
    if metadata.len() > limit as u64 {
        anyhow::bail!(
            "{description} at {} exceeds the {} byte limit",
            path.display(),
            limit
        );
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    let mut buffer = [0; 8192];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("reading {description} at {}", path.display()))?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > limit {
            anyhow::bail!(
                "{description} at {} exceeds the {} byte limit",
                path.display(),
                limit
            );
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

#[derive(Debug, Default)]
pub(crate) struct TraversalBudget {
    entries: usize,
}

impl TraversalBudget {
    pub(crate) fn visit(&mut self, description: &str) -> Result<()> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_TREE_ENTRIES {
            anyhow::bail!(
                "{description} exceeds the {} entry traversal limit",
                MAX_TREE_ENTRIES
            );
        }
        Ok(())
    }

    pub(crate) fn check_depth(depth: usize, description: &str) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            anyhow::bail!(
                "{description} exceeds the maximum recursion depth of {}",
                MAX_TREE_DEPTH
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn regular_file_reader_rejects_before_reading_an_oversized_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&[b'x'; 17]).unwrap();

        let error = read_regular_file(&path, 16, "state").unwrap_err();
        assert!(error.to_string().contains("16 byte limit"));
    }

    #[test]
    fn regular_file_reader_reads_a_regular_file_through_its_open_handle() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, b"state bytes").unwrap();

        assert_eq!(
            read_regular_file(&path, 64, "state").unwrap(),
            b"state bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_reader_rejects_hard_links() {
        let dir = tempdir().unwrap();
        let outside = dir.path().join("outside");
        let path = dir.path().join("state");
        fs::write(&outside, b"state bytes").unwrap();
        std::fs::hard_link(&outside, &path).unwrap();

        let error = read_regular_file(&path, 64, "state").unwrap_err();
        assert!(error.to_string().contains("hard-linked"));
    }

    #[test]
    fn traversal_budget_rejects_entry_and_depth_boundaries() {
        let mut budget = TraversalBudget {
            entries: MAX_TREE_ENTRIES,
        };
        assert!(budget.visit("test traversal").is_err());
        assert!(TraversalBudget::check_depth(MAX_TREE_DEPTH + 1, "test traversal").is_err());
        assert!(TraversalBudget::check_depth(MAX_TREE_DEPTH, "test traversal").is_ok());
    }
}
