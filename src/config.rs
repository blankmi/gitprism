//! `.gitprism.toml` — see design/decisions/0012-config-versioned-in-source.md.
//!
//! Holds what every command needs: dest's location, the committer identity
//! gitprism stamps on commits it creates (decisions/0010), and the configured
//! branch-pair list (decisions/0005).

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The config's own filename. Self-excluded from source→dest filtering by
/// convention (decisions/0011, decisions/0012) — never needs listing in
/// `.gitprismignore`.
pub const FILENAME: &str = ".gitprism.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub committer: Committer,
    pub dest: Dest,
    #[serde(default)]
    pub pairs: Vec<BranchPair>,
}

/// The identity gitprism stamps as committer on every commit it creates,
/// preserving the original author instead (decisions/0010).
#[derive(Debug, Deserialize)]
pub struct Committer {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct Dest {
    /// Path or remote URL — whatever git itself accepts as a remote.
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct BranchPair {
    pub source_branch: String,
    pub dest_branch: String,
}

impl Config {
    /// Load from an arbitrary filesystem path.
    ///
    /// `setup` reads this off disk before source has a first commit to
    /// version it in at all; every other command reads the same file once
    /// it's checked out from source's tree. The read mechanics don't differ
    /// between those two cases — only what's on the other end of the path
    /// does (decisions/0012).
    pub fn load(path: &Path) -> Result<Config> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing config at {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn write_config(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("create temp file");
        file.write_all(contents.as_bytes())
            .expect("write temp config");
        file
    }

    #[test]
    fn load_parses_a_well_formed_config() {
        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "git@example.com:group/dest.git"

            [[pairs]]
            source_branch = "main"
            dest_branch = "main"

            [[pairs]]
            source_branch = "release-2.0"
            dest_branch = "release-2.0"
            "#,
        );

        let config = Config::load(file.path()).expect("well-formed config should parse");

        assert_eq!(config.committer.name, "gitprism");
        assert_eq!(config.committer.email, "gitprism@example.com");
        assert_eq!(config.dest.url, "git@example.com:group/dest.git");
        assert_eq!(config.pairs.len(), 2);
        assert_eq!(config.pairs[0].source_branch, "main");
        assert_eq!(config.pairs[0].dest_branch, "main");
        assert_eq!(config.pairs[1].source_branch, "release-2.0");
        assert_eq!(config.pairs[1].dest_branch, "release-2.0");
    }

    #[test]
    fn load_defaults_pairs_to_empty_when_omitted() {
        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "git@example.com:group/dest.git"
            "#,
        );

        let config = Config::load(file.path()).expect("config without pairs should still parse");

        assert!(config.pairs.is_empty());
    }

    #[test]
    fn load_fails_loudly_on_a_missing_file() {
        let missing = Path::new("/nonexistent/does-not-exist.gitprism.toml");

        let err = Config::load(missing).expect_err("missing file must not silently succeed");

        assert!(err.to_string().contains("reading config"));
    }

    #[test]
    fn load_fails_loudly_on_malformed_toml() {
        let file = write_config("this is not valid toml {{{");

        let err = Config::load(file.path()).expect_err("malformed toml must not silently succeed");

        assert!(err.to_string().contains("parsing config"));
    }

    #[test]
    fn load_fails_loudly_on_a_missing_required_field() {
        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"
            "#,
        );

        let err = Config::load(file.path()).expect_err("config missing [dest] must not parse");

        assert!(err.to_string().contains("parsing config"));
    }
}
