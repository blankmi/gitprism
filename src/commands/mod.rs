//! One module per subcommand. Each is a stub for now — scaffolding only,
//! no git logic yet (see design/log.md for what's been decided so far).

pub mod policy_hash;
pub mod resolve;
pub mod setup;
pub mod sync;

use anyhow::Result;

use crate::marker::{self, StateKey};
use crate::policy;

/// Where a command's two run-time secrets — the pair's HMAC key and the
/// pinned policy digest — come from. Each is read on demand, at the exact
/// point production code already reads it today, so introducing this
/// abstraction changes no command's observable error-precedence order
/// (design/decisions/0026-protected-versioned-policy.md addendum).
pub(crate) trait SecretSource {
    fn state_key(&self) -> Result<StateKey>;
    fn expected_policy_digest(&self) -> Result<String>;
}

/// Production secrets: `GITPRISM_STATE_KEY` and `GITPRISM_POLICY_SHA256`,
/// read fresh from the environment on every call.
pub(crate) struct EnvSecrets;

impl SecretSource for EnvSecrets {
    fn state_key(&self) -> Result<StateKey> {
        marker::load_key_from_env()
    }

    fn expected_policy_digest(&self) -> Result<String> {
        policy::expected_digest_from_env()
    }
}

/// Test secrets: a fixed key, and a digest recomputed from a fixture's own
/// control files on every call — never gathered once upfront — so a test
/// that mutates those files between calls sees the same self-consistent
/// digest a real deployment's pin would see for that exact content. Built
/// via `crate::testutil::FixedSecrets::for_fixture`.
#[cfg(test)]
pub(crate) struct FixedSecrets {
    pub(crate) key: StateKey,
    pub(crate) config_path: std::path::PathBuf,
    pub(crate) ignore_path: std::path::PathBuf,
}

#[cfg(test)]
impl SecretSource for FixedSecrets {
    fn state_key(&self) -> Result<StateKey> {
        Ok(self.key)
    }

    fn expected_policy_digest(&self) -> Result<String> {
        policy::hash_files(&self.config_path, &self.ignore_path)
    }
}
