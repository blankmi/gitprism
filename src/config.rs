//! `.gitprism.toml` — see design/decisions/0012-config-versioned-in-source.md
//! and design/decisions/0013-repo-urls-optional-fall-back-to-env-vars.md.
//!
//! Holds what every command needs: source's and dest's locations, the
//! committer identity gitprism stamps on commits it creates (decisions/0010),
//! and `branches` — one plain list of branch names serving two different jobs
//! (decisions/0005, decisions/0017). `setup` grafts every named branch onto
//! dest's same-named tip. `sync`'s dest→source direction treats this same
//! list as the explicit, small set of branches whose independent dest content
//! gets ported back to source. `sync`'s source→dest direction doesn't read
//! this list at all — it discovers every branch that exists on source at run
//! time and mirrors each one to dest under its own name, so a brand-new
//! branch needs no config entry to start syncing.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::limits;

/// The config's own filename. Self-excluded from source→dest filtering by
/// convention (decisions/0011, decisions/0012) — never needs listing in
/// `.gitprismignore`.
pub const FILENAME: &str = ".gitprism.toml";

/// Guards any test, in this module or elsewhere in the crate, that reads or
/// writes `GITPRISM_SOURCE_URL`/`GITPRISM_DEST_URL` — process-global env vars
/// that would otherwise race against each other under cargo test's default
/// parallel execution. Shared (not module-private) because `commands::sync`'s
/// own tests touch the same env vars too; two separate mutexes wouldn't
/// actually synchronize access to one shared process-global.
#[cfg(test)]
pub(crate) static ENV_VAR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Env var names read when the matching `.gitprism.toml` field is omitted
/// (decisions/0013) — never read when the TOML field is present.
const SOURCE_URL_ENV: &str = "GITPRISM_SOURCE_URL";
const DEST_URL_ENV: &str = "GITPRISM_DEST_URL";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub committer: Committer,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub dest: Dest,
    #[serde(default)]
    pub branches: Vec<String>,
}

/// The identity gitprism stamps as committer on every commit it creates,
/// preserving the original author instead (decisions/0010).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Committer {
    pub name: String,
    pub email: String,
}

/// Where dest→source's result gets pushed (decisions/0013). Optional because
/// it's committed *inside* source itself — a credential-bearing or
/// per-environment URL belongs in `GITPRISM_SOURCE_URL`, not source's history.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub url: Option<String>,
}

/// Where source→dest pushes to. Optional for the same reason as [`Source`]
/// (decisions/0013) — falls back to `GITPRISM_DEST_URL`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dest {
    pub url: Option<String>,
}

impl Config {
    /// Parse already-read config contents.
    ///
    /// Split out from [`Config::load`] so `setup` can reuse the exact raw
    /// bytes it read off disk for the graft commit's blob (decisions/0012),
    /// without reading the file a second time.
    pub fn parse(raw: &str, path: &Path) -> Result<Config> {
        let config: Config =
            toml::from_str(raw).with_context(|| format!("parsing config at {}", path.display()))?;
        if config.committer.name.is_empty() || config.committer.email.is_empty() {
            anyhow::bail!(
                "config at {} requires non-empty committer name and email",
                path.display()
            );
        }
        for (field, url) in [
            ("[source].url", config.source.url.as_deref()),
            ("[dest].url", config.dest.url.as_deref()),
        ] {
            if let Some(url) = url
                && (url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control))
            {
                anyhow::bail!("config at {} contains an invalid {field}", path.display());
            }
        }
        if config.branches.len() > limits::MAX_CONFIG_BRANCHES {
            anyhow::bail!(
                "config at {} contains more than the {} configured-branch limit",
                path.display(),
                limits::MAX_CONFIG_BRANCHES
            );
        }
        let mut seen = HashSet::with_capacity(config.branches.len());
        for branch in &config.branches {
            if branch.is_empty() {
                anyhow::bail!("config at {} contains an empty branch name", path.display());
            }
            crate::git::validate_branch_name(branch).with_context(|| {
                format!(
                    "validating configured branch {branch:?} in {}",
                    path.display()
                )
            })?;
            if !seen.insert(branch) {
                anyhow::bail!(
                    "config at {} contains duplicate branch name {branch:?}",
                    path.display()
                );
            }
        }
        Ok(config)
    }

    /// Load from an arbitrary filesystem path.
    ///
    /// `setup` reads this off disk before source has a first commit to
    /// version it in at all; every other command reads the same file once
    /// it's checked out from source's tree. The read mechanics don't differ
    /// between those two cases — only what's on the other end of the path
    /// does (decisions/0012).
    #[cfg(test)]
    pub fn load(path: &Path) -> Result<Config> {
        let raw = limits::read_regular_file(path, limits::MAX_CONTROL_FILE_BYTES, "config")?;
        let raw = std::str::from_utf8(&raw)
            .with_context(|| format!("config at {} is not valid UTF-8", path.display()))?;
        Self::parse(raw, path)
    }

    /// Source's remote URL: `[source].url` if the committed config sets it,
    /// else `GITPRISM_SOURCE_URL` (decisions/0013). Resolved lazily, per use,
    /// rather than at parse time — a config that omits this and relies
    /// entirely on the env var is completely valid.
    pub fn source_url(&self) -> Result<String> {
        Self::resolve_url(self.source.url.as_deref(), SOURCE_URL_ENV, "[source].url")
    }

    /// Dest's remote URL: `[dest].url` if the committed config sets it, else
    /// `GITPRISM_DEST_URL` (decisions/0013).
    pub fn dest_url(&self) -> Result<String> {
        Self::resolve_url(self.dest.url.as_deref(), DEST_URL_ENV, "[dest].url")
    }

    fn resolve_url(configured: Option<&str>, env_var: &str, field: &str) -> Result<String> {
        if let Some(url) = configured {
            return Ok(url.to_string());
        }
        std::env::var(env_var).with_context(|| {
            format!("{field} isn't set in .gitprism.toml, and its fallback env var {env_var} isn't set either")
        })
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
            branches = ["main", "release-2.0"]

            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "git@example.com:group/dest.git"
            "#,
        );

        let config = Config::load(file.path()).expect("well-formed config should parse");

        assert_eq!(config.committer.name, "gitprism");
        assert_eq!(config.committer.email, "gitprism@example.com");
        assert_eq!(config.dest_url().unwrap(), "git@example.com:group/dest.git");
        assert_eq!(config.branches, vec!["main", "release-2.0"]);
    }

    #[test]
    fn load_defaults_branches_to_empty_when_omitted() {
        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "git@example.com:group/dest.git"
            "#,
        );

        let config = Config::load(file.path()).expect("config without branches should still parse");

        assert!(config.branches.is_empty());
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
    fn load_rejects_empty_invalid_and_duplicate_branches_before_use() {
        for branches in [
            r#"branches = [""]"#,
            r#"branches = ["-upload-pack=touch /tmp/pwned"]"#,
            r#"branches = ["main", "main"]"#,
        ] {
            let file = write_config(&format!(
                "{branches}\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\""
            ));
            let err = Config::load(file.path()).expect_err("invalid branch config must fail");
            assert!(err.to_string().contains("branch"));
        }
    }

    #[test]
    fn parse_rejects_too_many_configured_branches() {
        let branches = (0..=limits::MAX_CONFIG_BRANCHES)
            .map(|index| format!("\"branch-{index}\""))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!(
            "branches = [{branches}]\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\""
        );
        let error = Config::parse(&raw, Path::new(".gitprism.toml")).unwrap_err();
        assert!(error.to_string().contains("configured-branch limit"));
    }

    #[test]
    fn load_rejects_unknown_fields_empty_identity_and_invalid_configured_remotes() {
        for raw in [
            "extra = true\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"",
            "[committer]\nname = \"\"\nemail = \"gitprism@example.com\"",
            "[committer]\nname = \"gitprism\"\nemail = \"\"",
            "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n[dest]\nurl = \"\"",
            "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n[dest]\nurl = \"--upload-pack=printf\"",
        ] {
            let file = write_config(raw);
            Config::load(file.path()).expect_err("invalid semantic config must fail");
        }
    }

    #[test]
    fn load_fails_loudly_on_a_missing_required_field() {
        // [committer] itself is still required — unlike [source]/[dest],
        // which default to empty now that their only field, url, is optional
        // (decisions/0013).
        let file = write_config(
            r#"
            [dest]
            url = "git@example.com:group/dest.git"
            "#,
        );

        let err = Config::load(file.path()).expect_err("config missing [committer] must not parse");

        assert!(err.to_string().contains("parsing config"));
    }

    #[test]
    fn load_defaults_source_and_dest_to_no_url_when_omitted() {
        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"
            "#,
        );

        let config =
            Config::load(file.path()).expect("a config with no [source]/[dest] should still parse");

        assert!(config.source.url.is_none());
        assert!(config.dest.url.is_none());
    }

    #[test]
    fn url_prefers_the_configured_toml_value_over_the_env_var() {
        let _guard = ENV_VAR_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("GITPRISM_SOURCE_URL", "from-env");
        }

        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [source]
            url = "from-toml"
            "#,
        );
        let config = Config::load(file.path()).unwrap();

        let result = config.source_url();
        unsafe {
            std::env::remove_var("GITPRISM_SOURCE_URL");
        }
        assert_eq!(result.unwrap(), "from-toml");
    }

    #[test]
    fn url_falls_back_to_the_env_var_when_the_toml_field_is_omitted() {
        let _guard = ENV_VAR_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("GITPRISM_DEST_URL", "from-env");
        }

        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"
            "#,
        );
        let config = Config::load(file.path()).unwrap();

        let result = config.dest_url();
        unsafe {
            std::env::remove_var("GITPRISM_DEST_URL");
        }
        assert_eq!(result.unwrap(), "from-env");
    }

    #[test]
    fn url_fails_loudly_when_neither_toml_nor_env_var_is_set() {
        let _guard = ENV_VAR_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("GITPRISM_SOURCE_URL");
        }

        let file = write_config(
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"
            "#,
        );
        let config = Config::load(file.path()).unwrap();

        let err = config.source_url().expect_err(
            "neither [source].url nor GITPRISM_SOURCE_URL set must not silently succeed",
        );

        assert!(err.to_string().contains("GITPRISM_SOURCE_URL"));
    }
}
