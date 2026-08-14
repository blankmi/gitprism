//! `.gitprismignore` — see design/decisions/0011-exclude-list-is-gitignore-syntax.md.
//!
//! What must never reach dest, in exactly `.gitignore`'s pattern syntax. The
//! `ignore` crate (used by ripgrep) already implements that syntax correctly,
//! so this module is a thin wrapper: parse patterns, and always treat
//! gitprism's own control files — `.gitprismignore` and `.gitprism.toml`
//! (decisions/0012) — as excluded, whether or not they're listed.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// The exclude-list's own filename, at source's repo root.
pub const FILENAME: &str = ".gitprismignore";

pub struct ExcludeList {
    matcher: Gitignore,
}

impl ExcludeList {
    /// Parse patterns from `.gitprismignore`'s raw contents (one pattern per
    /// line, exactly `.gitignore` syntax).
    pub fn from_contents(contents: &str) -> Result<ExcludeList> {
        let mut builder = GitignoreBuilder::new("");
        for line in contents.lines() {
            builder
                .add_line(None, line)
                .with_context(|| format!("parsing .gitprismignore pattern {line:?}"))?;
        }
        let matcher = builder
            .build()
            .context("building .gitprismignore matcher")?;

        Ok(ExcludeList { matcher })
    }

    /// Load `.gitprismignore` from `root` (the directory it lives in). A
    /// missing file means nothing is excluded beyond the file's own automatic
    /// self-exclusion — analogous to a repo with no `.gitignore` at all.
    pub fn load(root: &Path) -> Result<ExcludeList> {
        let path = root.join(FILENAME);

        match fs::read_to_string(&path) {
            Ok(contents) => Self::from_contents(&contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::from_contents(""),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Whether `path` (relative to source's root) must not reach dest.
    ///
    /// `.gitprismignore` and `.gitprism.toml` are always excluded —
    /// including against a `!` negation line naming either inside the file,
    /// since the whole point of automatic self-exclusion (decisions/0011,
    /// decisions/0012) is not depending on the file getting that right.
    ///
    /// `Gitignore::matched` only tests the exact path handed to it — it
    /// doesn't know a path is inside an excluded directory unless asked about
    /// that directory directly. A real `.gitignore` gets that behavior for
    /// free because a directory walker stops descending once it excludes a
    /// directory; we're filtering already-known full paths instead, so we
    /// walk the ancestors ourselves.
    pub fn is_excluded(&self, path: &Path, is_dir: bool) -> bool {
        if path == Path::new(FILENAME) || path == Path::new(crate::config::FILENAME) {
            return true;
        }
        if self.matcher.matched(path, is_dir).is_ignore() {
            return true;
        }
        path.ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .any(|ancestor| self.matcher.matched(ancestor, true).is_ignore())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn excludes_a_matching_pattern() {
        let list = ExcludeList::from_contents("secret.txt").unwrap();

        assert!(list.is_excluded(Path::new("secret.txt"), false));
        assert!(!list.is_excluded(Path::new("public.txt"), false));
    }

    #[test]
    fn excludes_a_directory_and_its_contents() {
        let list = ExcludeList::from_contents("secrets/").unwrap();

        assert!(list.is_excluded(Path::new("secrets"), true));
        assert!(list.is_excluded(Path::new("secrets/inner.txt"), false));
    }

    #[test]
    fn respects_negation() {
        let list = ExcludeList::from_contents("*.log\n!keep.log").unwrap();

        assert!(list.is_excluded(Path::new("debug.log"), false));
        assert!(!list.is_excluded(Path::new("keep.log"), false));
    }

    #[test]
    fn self_excludes_the_gitprismignore_file_even_when_unlisted() {
        let list = ExcludeList::from_contents("").unwrap();

        assert!(list.is_excluded(Path::new(FILENAME), false));
    }

    #[test]
    fn self_exclusion_cannot_be_negated() {
        let list = ExcludeList::from_contents("!.gitprismignore").unwrap();

        assert!(list.is_excluded(Path::new(FILENAME), false));
    }

    #[test]
    fn excludes_the_config_file_even_when_unlisted() {
        let list = ExcludeList::from_contents("").unwrap();

        assert!(list.is_excluded(Path::new(crate::config::FILENAME), false));
    }

    #[test]
    fn config_exclusion_cannot_be_negated() {
        let list = ExcludeList::from_contents("!.gitprism.toml").unwrap();

        assert!(list.is_excluded(Path::new(crate::config::FILENAME), false));
    }

    #[test]
    fn load_treats_a_missing_file_as_no_exclusions() {
        let dir = tempdir().unwrap();

        let list = ExcludeList::load(dir.path()).expect("missing .gitprismignore is not an error");

        assert!(!list.is_excluded(Path::new("anything.txt"), false));
        assert!(list.is_excluded(Path::new(FILENAME), false));
    }

    #[test]
    fn load_reads_patterns_from_the_file_on_disk() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(FILENAME), "secret.txt\n").unwrap();

        let list = ExcludeList::load(dir.path()).unwrap();

        assert!(list.is_excluded(Path::new("secret.txt"), false));
        assert!(!list.is_excluded(Path::new("public.txt"), false));
    }
}
