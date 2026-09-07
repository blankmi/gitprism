# Plan TEST-001 — policy pin and state key are enforced under test

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 4, TEST-001 |
| Severity / priority | MEDIUM / P1 |
| Effort | Medium (mechanical churn across test fixtures) |
| Decision required | No behaviour change; a short addendum to decision 0026 recording how the pin is supplied to tests |
| Depends on | — |
| Status | Implemented 2026-09-04 (steps 1-6, including precedence and fixture-path follow-ups) |

## Problem

`policy::verify_expected_digest` (`src/policy.rs:282-291`) sets
`expected = actual` under `#[cfg(test)]`; `marker::load_key`
(`src/marker.rs:66-80`) returns a fixed key under `#[cfg(test)]`. The production
branches that read `GITPRISM_POLICY_SHA256` and `GITPRISM_STATE_KEY` are dead
code in every test build, and nothing asserts that a wrong or missing value
refuses before any fetch or ref mutation. There is no `tests/` directory, so the
built binary is never executed by CI beyond `--version`.

## Target shape

* `marker`: `load_key_from_env() -> Result<StateKey>` (no `cfg` fork) and
  `StateKey::from_hex(&str) -> Result<StateKey>` (today's `parse_key`, made
  `pub(crate)`). A `#[cfg(test)] pub(crate) fn test_key() -> StateKey` returns
  the fixed key tests use today.
* `policy`: `load(config_path, ignore_path, expected_digest: &str)` and
  `expected_digest_from_env() -> Result<String>`; `verify_expected_digest` is
  deleted; `verify_digest` is the only check.
* Commands: a `pub(crate) trait SecretSource { fn state_key(&self) ->
  Result<StateKey>; fn expected_policy_digest(&self) -> Result<String>; }` in
  `src/commands/mod.rs`. `EnvSecrets` reads the two variables on demand;
  `#[cfg(test)] FixedSecrets { key, digest }` returns constants. Each command's
  `run` becomes `run_with(..., &EnvSecrets)` and the secrets are read at the
  exact points `marker::load_key()` and `policy::load()` are called today, so
  repository/key ordering is preserved. One narrower precedence change is
  recorded in decision 0026's addendum: an unset policy pin now fails before
  a missing or unreadable control file, because the pin is read before
  `policy::load`. The `StateKey` newtype keeps its
  no-`Debug`/no-`Display` guarantees.
* Command-level ordering, with the pin/control-file exception above: `sync` and `resolve` discover the
  repository, then read the key, then load and verify the policy
  (`sync/mod.rs:197-214`, `resolve.rs:85-102`); `setup` discovers, then loads
  and verifies the policy, then reads the key (`setup.rs:95, 107`). Unifying
  `setup` with the others would be a visible change and needs its own line in
  the 0026 addendum if the owner wants it. This plan does not make it.
* Tests call `run_with` with `FixedSecrets` holding `test_key()` and a digest
  computed by `policy::hash_files` from the fixture's own files. Env-reading
  tests use the existing `config::ENV_VAR_LOCK`.

## Steps

### Step 1 — refusal tests that fail today

**Files.** `src/commands/sync/tests/run_entrypoint.rs`, `src/commands/setup.rs`
tests, `src/commands/resolve.rs` tests.

**Test first.** One test per command, under `ENV_VAR_LOCK`, calling the
env-reading entry point:

* `GITPRISM_STATE_KEY` unset → error names the variable; the dest bare repo's
  refs are unchanged; no `FETCH_HEAD` exists in the source clone; no
  `OperationLock` file was created.
* `GITPRISM_POLICY_SHA256` set to a valid-looking but wrong digest → error says
  "does not match"; same three no-side-effect assertions.
* Correct values → the command succeeds (proves the env path works at all).

These cannot pass until step 2 removes the forks, so mark them `#[ignore]` with a
reason until then, or land steps 1–2 in one commit.

### Step 2 — remove the compile-time forks

**Files.** `src/marker.rs:64-80`, `src/policy.rs:38-69, 282-291`.

**Change.** As in "Target shape". `load_from_bytes_with_expected` (test-only)
becomes the real `load_from_bytes` signature. Keep the exact error strings.

### Step 3 — thread secrets through the commands

**Files.** `src/main.rs`, `src/cli.rs`, `src/commands/{setup.rs, resolve.rs,
policy_hash.rs}`, `src/commands/sync/mod.rs:193-224` and `load_run_policy`.

**Change.** Introduce `SecretSource`, `EnvSecrets`, and `FixedSecrets` in
`src/commands/mod.rs`. Each command's `run` becomes `run_with(...,
&EnvSecrets)`. `policy::load` takes the expected digest as a parameter and the
caller obtains it from `secrets.expected_policy_digest()` immediately before
the call; `secrets.state_key()` replaces `marker::load_key()` in place.
`policy_hash` needs neither secret and is unchanged. Add one test per command
asserting today's precedence: a missing repository is reported before a
missing key; for `setup`, a missing config file is reported before a missing
key; for `sync`/`resolve`, a missing key is reported before a missing config
file.

### Step 4 — migrate fixtures

**Files.** every test calling `run(`, `run_with_direction(`, or
`marker::load_key().unwrap()` (eight sites in `resolve.rs` tests; the
`sync/tests/*` wrappers; `anchor.rs:195`).

**Change.** Replace with `run_with(..., &FixedSecrets::for_fixture(&config_path,
&ignore_path))` where the constructor (in `src/testutil.rs`) computes the
digest from the fixture files and uses `test_key()`. Replace
`marker::load_key()` in tests with `marker::test_key()`.

**Done when.** `cargo test` count is unchanged plus the step 1 tests, none
ignored.

### Step 5 — binary smoke test

**Files.** new `tests/cli.rs`.

**Change.** Use `env!("CARGO_BIN_EXE_gitprism")` (built-in, no dev-dependency):

* `--version` exits 0 and prints the crate version;
* `sync` in an empty temp dir with no env exits non-zero and names the
  repository/key requirement;
* `policy-hash` over two fixture files with fixed contents prints a known
  digest constant. The crate has only a binary target (`Cargo.toml` declares
  no `[lib]`), so `tests/cli.rs` cannot call `policy::digest_bytes`; instead a
  unit test inside `src/policy.rs` asserts `digest_bytes` of the same fixed
  bytes equals the same constant. The two tests share the constant by value,
  and the unit test is what guards it.

`cargo test --workspace --all-features --locked` already covers `tests/`; no
CI change needed.

### Step 6 — record

**Files.** `design/decisions/0026-protected-versioned-policy.md` (addendum:
"the pin is a parameter of the command, read from the environment only at the
CLI boundary; tests supply it explicitly"), `design/log.md`.

## Verification

Release gates; the six step 1 tests; the precedence tests from step 3;
`tests/cli.rs` runs on all three CI targets (Windows path: the binary name
carries `.exe`, which `CARGO_BIN_EXE_*` already handles).

## Revision history

* 2026-09-02, after plan review: the first draft read both secrets in the
  wrapper before `run_with`, which cannot keep repository discovery first and
  would have changed `setup`'s policy-before-key order. Replaced by an
  on-demand `SecretSource`. The binary smoke test now uses a known-answer
  constant because the crate exposes no library API.
