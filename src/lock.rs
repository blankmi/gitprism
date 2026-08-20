//! Per-repository serialization for gitprism mutations.
//!
//! The lock lives in Git's common directory rather than a worktree-specific
//! directory, so linked worktrees share the same operation lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;

use anyhow::{Context, Result};
use git2::Repository;

const LOCK_FILENAME: &str = "gitprism.lock";

/// An exclusive lock held for the lifetime of one mutating gitprism command.
/// The operating system releases it when this handle is dropped.
#[derive(Debug)]
pub struct OperationLock {
    _file: File,
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

impl OperationLock {
    /// Acquires the lock for the repository's common Git directory without
    /// truncating its lock file. A second gitprism process fails immediately
    /// rather than waiting behind a potentially interactive operation.
    pub fn acquire(repo: &Repository) -> Result<Self> {
        let path = lock_path(repo);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .context("opening the gitprism operation lock")?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!(
                    "another gitprism operation is already running for this repository; wait for it to finish and retry"
                )
            }
            Err(TryLockError::Error(error)) => {
                Err(error).context("acquiring the gitprism operation lock")
            }
        }
    }
}

fn lock_path(repo: &Repository) -> PathBuf {
    repo.commondir().join(LOCK_FILENAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquisition_fails_and_drop_releases_lock() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let first = OperationLock::acquire(&repo).unwrap();
        let error = OperationLock::acquire(&repo).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("another gitprism operation is already running")
        );
        drop(first);
        OperationLock::acquire(&repo).unwrap();
    }

    #[test]
    fn lock_is_in_the_repository_common_directory() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        assert_eq!(lock_path(&repo), repo.commondir().join(LOCK_FILENAME));

        let reopened = Repository::open(dir.path()).unwrap();
        let first = OperationLock::acquire(&repo).unwrap();
        assert!(OperationLock::acquire(&reopened).is_err());
        drop(first);
    }

    #[test]
    fn linked_worktrees_share_the_common_directory_lock() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("gitprism", "gitprism@example.com").unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "initial",
            &tree,
            &[],
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
        let linked_path = dir.path().join("linked");
        let worktree = repo.worktree("linked", &linked_path, None).unwrap();
        let linked_repo = Repository::open_from_worktree(&worktree).unwrap();

        let first = OperationLock::acquire(&repo).unwrap();
        assert!(OperationLock::acquire(&linked_repo).is_err());
        drop(first);
    }

    #[test]
    fn existing_lock_file_is_not_truncated() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = lock_path(&repo);
        std::fs::write(&path, b"diagnostic marker").unwrap();
        let lock = OperationLock::acquire(&repo).unwrap();
        drop(lock);
        assert_eq!(std::fs::read(&path).unwrap(), b"diagnostic marker");
    }
}
