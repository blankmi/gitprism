# gitprism Repository Review

Review date: 2026-08-20

## 1. Executive Summary

gitprism has a coherent core design, strong repository-level tests, idiomatic
Rust, and careful fast-forward and operator-driven conflict handling. The
hybrid `git2`/Git CLI approach remains reasonable for this tool.

The original review's confirmed critical/high implementation findings have
been remediated. The project is still **not ready for production distribution**:
it has not run against a real production repository pair, cross-platform CI has
not yet executed in this environment, and release-tag, artifact-signing and
packaging decisions remain intentionally open.

| Area | Score | Rationale |
|---|---:|---|
| Architecture | 8/10 | Decisions, authenticated policy, operation locking and bounded work are explicit; `sync.rs`/`resolve.rs` remain large. |
| Rust Code Quality | 8/10 | Contextual `Result` propagation, no production `unsafe`, byte-aware boundaries and static limits; final CI still needs to execute. |
| Git Integration | 8/10 | Git arguments, refs, porcelain push status, Git version checks and subprocess limits are centralized and tested. |
| Security | 8/10 | Original command-injection, marker-forgery, policy-integrity and credential-disclosure paths are mitigated; Git itself remains trusted. |
| Reliability | 8/10 | CAS ref updates, common-directory locking, rollback aggregation, deadlines and resource limits cover the major failure paths. |
| Testing | 8/10 | 186 tests cover hostile inputs and rollback/concurrency cases; real-pair and cross-platform execution remain pending. |
| Cross-Platform Robustness | 7/10 | Unix byte paths are preserved, unsupported Windows bytes fail clearly, and CI covers three OSes; CI has not run here yet. |
| Maintainability | 6/10 | Rationale is strong, but the two command modules and inline tests remain oversized. |
| Production Readiness | 6/10 | CI, dependency policy, MIT licensing and repository metadata are present; tagging, packaging, signing and real deployment validation remain open. |

Release recommendation: **Conditional no-go for public production distribution.**
No confirmed CRITICAL or HIGH implementation finding remains in the reviewed
paths, but distribution should wait for the remaining release-process
decisions, executed Linux/macOS/Windows CI, and a real repository-pair pilot.

Verification performed:

- `cargo test --workspace --all-features --locked`: 186 passed locally.
- `cargo build --release --locked`: passed locally.
- Workflow and Dependabot YAML parsed successfully; `deny.toml` parsed successfully.
- `cargo tree --duplicates`: no duplicate versions reported.
- `cargo audit` and `cargo deny` are not installed locally; CI now invokes both,
  so no local vulnerability-clean claim is made.
- The local host is `aarch64-apple-darwin`; Linux and Windows CI legs are
  configured but have not executed in this environment.
- `cargo fmt --all -- --check`: passed locally after filesystem hardening
  commit `2202885`.
- Strict locked Clippy with all workspace targets, features and warnings
  denied: passed locally after `2202885`.

## 2. Architecture Overview

The actual execution flow is:

```text
Clap CLI
  -> repository discovery and external policy authentication
  -> mutating-operation lock (setup/sync/resolve)
  -> setup / sync / resolve orchestration
  -> git2 object/ref/tree operations
  -> centralized, bounded Git subprocess runner
  -> libgit2 checkout or terminal reporting
```

Primary entry point: `src/main.rs` (`main`).

Commands:

- `setup`: discovers a non-bare source repository, loads bootstrap control files, fetches every configured destination branch, plans grafts/merges, creates commits and branch refs, then checks out the first branch. See `commands::setup::run`.
- `sync`: lists source branches, processes configured dest->source branches first, then mirrors every local source branch source->dest. See `commands::sync::run`.
- `resolve`: handles both directions. Dest->source uses a real `git cherry-pick`
  in the source checkout; source->dest uses an isolated linked worktree and a
  real no-commit cherry-pick. In both cases the operator edits/stages conflicts
  and explicitly continues; no conflict is automatically resolved. See
  `src/commands/resolve.rs`.

Git integration is hybrid:

- `git2`: discovery, commits, refs, trees, revwalks, merge-base, checkout.
- Real `git`: fetch, ls-remote, push, worktree management, cherry-pick,
  commit, ls-files, merge-tree and version checks. Production process paths
  are centralized in `src/git.rs` through a bounded, timed runner.
- No Rust shell invocation is present.
- No application database or state file exists. Mapping state is encoded in commit trailers.

Filesystem interaction is limited mainly to config/control-file loading, setup's control-file replacement, libgit2 checkout, and `CHERRY_PICK_HEAD`. Filtering operates against Git objects rather than traversing the working directory.

## 3. Threat Model

Trusted by the current implementation:

- The `git` executable selected through `PATH`.
- Process environment and global/system Git configuration.
- Repository-local `.git/config`, hooks, and repository metadata.
- The deployment-controlled `GITPRISM_POLICY_SHA256` and
  `GITPRISM_STATE_KEY` values, including their secure delivery and approval.
- Git's own transport/credential boundary and the operator who invokes the
  tool.

Untrusted or repository-controlled:

- Branches, refs, commit graphs, commit messages, authors and filenames.
- Git object contents, including malformed or very large trees/histories.
- Configured URLs and branch strings.
- `.gitprism.toml` and `.gitprismignore` until their exact bytes match the
  externally approved policy digest.
- All `Gitprism-*` marker text until the authenticated state block verifies it.
- Remote Git stderr and hook/server messages.
- Working-tree state and symlinks.

Primary boundaries:

- Rust -> Git CLI argument parsing.
- Git CLI -> credential helpers, SSH, hooks, transport helpers and remote servers.
- Repository object graph -> commit filtering and sync-state selection.
- Checkout/ref updates -> the user's local working tree and history.
- Configured remote -> source/destination confidentiality boundary.

The implementation does not sandbox the Git installation or `.git` metadata.
Ordinary cloned content can supply a malicious policy, but it is rejected
unless an operator/deployment explicitly approves its digest. A manually
supplied/shared `.git` directory can still influence Git transport and hooks;
those remain trusted process boundaries rather than repository-content inputs
that gitprism attempts to sandbox.

## 4. Critical and High Findings

### GPR-001 - Git option injection permits command execution

- Category: Subprocess security
- Severity: **CRITICAL**
- Confidence: High
- Status: **Fixed** in `b4ff279`.
- File/symbol: `src/git.rs`, `remote_ref_exists` and `fetch`
- Description: This was a confirmed Git option-injection path. It is now
  closed by rejecting empty, control-bearing and leading-dash remotes and by
  placing `--` before remote operands.
- Evidence: `src/git.rs::validate_remote` runs before every remote operation;
  `fetch`, `remote_ref_exists` and `push` use `--`. Regression tests cover a
  leading-dash remote before Git starts.
- Residual scenario: Git's executable, transport helpers, configuration and
  hooks remain an explicit trusted boundary; this finding no longer permits a
  configured remote value to become a Git option.
- Impact: The confirmed arbitrary-command path is removed.
- Recommended remediation: Retain the centralized remote validation,
  option terminators and leading-dash regression tests.

This is argument injection into Git, not shell interpolation by `Command::arg()`.

### GPR-002 - `fetch` accepts a raw refspec capable of modifying local refs

- Category: Git safety
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed** in `b4ff279`.
- File/symbol: `src/git.rs`, `src/commands/setup.rs`
- Description: Configured branch values are now validated as branch names and
  converted to `refs/heads/<validated-name>` before fetching.
- Evidence: `Config::parse` and `git::fetch` both validate branch names; fetch
  passes a constructed source ref after `--`, never a caller-supplied refspec.
  Tests reject `+`, `:` and other raw-refspec shapes without touching
  `FETCH_HEAD`.
- Residual scenario: A valid branch can still point to repository-controlled
  Git content, which is expected input rather than refspec injection.
- Impact: The local-ref overwrite path from a malicious refspec is removed.
- Recommended remediation: Retain branch validation and constructed source
  refs; extend the regression matrix when new Git transport commands are added.

### GPR-003 - Repository authors can forge synchronization state

- Category: Git correctness/security
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed** in `6016c3f`.
- File/symbol: `src/commands/sync.rs`, marker/state discovery and advancement paths
- Description: Marker text is now only accepted when accompanied by a final,
  authenticated state block bound to the branch, direction, counterpart,
  parent IDs, tree, identities and message body.
- Evidence: `src/marker.rs` verifies the HMAC state key and canonical marker
  structure; duplicate or user-supplied marker lines are stripped from
  generated messages. Sync tests cover forged, duplicate and mismatched state.
- Residual scenario: Losing the deployment state key makes existing markers
  unusable and requires operator recovery; that is an intentional trust
  boundary, not silent acceptance of forged state.
- Impact: The original silent-divergence path is mitigated by authenticated
  provenance.
- Recommended remediation: Protect the external state key, retain authenticated
  marker tests, and document key-loss recovery for operators.

### GPR-004 - Versioned config and exclusion policy are consumed from an untrusted checkout

- Category: Architecture/security
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed with an explicit deployment boundary** in `83d66d8`.
- File/symbol: `src/config.rs`, `src/commands/sync.rs`, configuration loading and mutation paths
- Description: Repository-controlled policy remains versioned and reviewable,
  but mutating commands now require an external SHA-256 digest over the exact
  `.gitprism.toml` and `.gitprismignore` bytes before parsing or mutation.
- Evidence: `policy::load` authenticates both files before setup/sync/resolve;
  configuration rejects unknown fields, invalid URLs, duplicate/invalid
  branches and oversized control files. The deployment playbook documents the
  protected `GITPRISM_POLICY_SHA256` variable.
- Residual scenario: A deployment that deliberately approves a malicious
  policy digest, or trusts an unreviewed source branch, has approved that
  policy by definition. CI must protect the digest and checkout policy.
- Impact: Ordinary branch content can no longer silently change the active
  remotes or exclusion policy during a mutating run.
- Recommended remediation: Protect `GITPRISM_POLICY_SHA256`, approve it only
  for reviewed source revisions, and keep policy-change review in deployment
  controls.

### GPR-005 - Source->dest conflict recovery directs users to an incompatible command

- Category: Correctness/UX
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed** in `25b2250`; conflict policy remains fail-fast and
  operator-driven.
- File/symbol: `src/commands/sync.rs`, `src/commands/resolve.rs`, `design/decisions/0015-resolve-real-git-cherry-pick-explicit-continue.md`
- Description: `resolve --direction source-to-dest` now reproduces the
  conflict in an isolated linked worktree. The operator edits and stages the
  result and explicitly continues; `sync` never auto-resolves or chooses a
  side.
- Evidence: `src/commands/resolve.rs` authenticates resumable state, checks
  excluded paths, authenticates the exact registered linked-worktree path,
  and supports source-to-dest continuation (`2202885`). README and decision
  0027 document the manual workflow. End-to-end tests cover the linked-worktree
  path and stale/non-fast-forward recovery.
- Scenario: A real conflict still stops the branch and requires operator
  action, as intentionally decided in decisions 0007/0008.
- Impact: The incorrect recovery command is removed; intentional fail-fast
  behavior remains.
- Recommended remediation: Retain the explicit direction and fail-fast/manual
  operator workflow; do not add automatic conflict selection.

### GPR-006 - Credential-bearing URLs are copied into errors and CI logs

- Category: Secrets handling
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed** in `b4ff279`.
- File/symbol: `src/git.rs`, `src/commands/sync.rs`
- Description: Remote values are now redacted before byte escaping and bounded
  terminal presentation; application-authored errors identify configured
  remotes by role rather than printing the URL.
- Evidence: `git::git_diagnostic` redacts raw bytes before framing, and tests
  cover credentials, invalid bytes, controls and URL-like diagnostics.
- Residual scenario: Git's own trusted helper/hook process may observe the URL
  through normal Git configuration; gitprism removes its fallback URL
  variables from child environments.
- Impact: Credential-bearing URL values are no longer copied into gitprism's
  error output or CI logs.
- Recommended remediation: Retain centralized redaction and ensure future Git
  subprocess paths use the same diagnostic runner.

### GPR-007 - Local branch advancement is not atomic with its safety check

- Category: Concurrency/data integrity
- Severity: **HIGH**
- Confidence: High
- Status: **Fixed** in `ba86206`.
- File/symbol: `src/commands/sync.rs`, local compare-and-swap advancement
- Description: Local advancement now uses compare-and-swap against the exact
  observed OID and is serialized by a non-blocking lock in Git's common
  directory. Preflight rejects tracked/index conflicts before remote push and
  reports the recovery path if a remote push succeeds but local advancement
  cannot complete.
- Evidence: `src/lock.rs`, `sync::preflight_local_source_branch` and
  `reference_matching` implement the boundary; tests cover concurrent ref
  movement, dirty worktrees, colliding untracked files and unrelated untracked
  files.
- Residual scenario: A process that bypasses Git's ref locking can still race,
  but CAS refuses to overwrite its move. The remote/local partial-success case
  requires ordinary Git reconciliation by the operator.
- Impact: gitprism no longer overwrites a concurrent local ref move.
- Recommended remediation: Retain CAS, common-directory locking, preflight
  checks and the documented remote-success recovery procedure.

## 5. Medium Findings

### GPR-008 - `resolve` depends on unrelated Git identity configuration

- Category: Reliability/test isolation
- Severity: **MEDIUM**
- Confidence: High
- Status: **Fixed** in `b064afa`.
- File/symbol: `src/git.rs`, cherry-pick and empty-resolution subprocess paths
- Description: Real cherry-pick and empty-resolution Git subprocesses now
  receive the verified configured committer identity while preserving the
  picked commit's author.
- Evidence: `git::cherry_pick_start`, `cherry_pick_continue` and the empty
  commit path set `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL`; tests exercise
  identity-independent resolution.
- Impact: Resolve no longer depends on unrelated global Git identity settings.
- Recommended remediation: Retain configured identity injection and keep
  Git hooks/helpers documented as trusted rather than attempting a partial
  cross-platform sandbox.

### GPR-009 - Setup rollback and restoration errors are silently discarded

- Category: Error handling/filesystem safety
- Severity: **MEDIUM**
- Confidence: High
- Status: **Fixed** in `01d647` and `2202885`.
- File/symbol: `src/commands/setup.rs`, rollback and control-file restoration paths
- Description: Rollback now attempts every cleanup action and aggregates
  failures with the primary setup error, including control-file restoration,
  ref rollback, branch cleanup and HEAD restoration. Control-file restoration
  uses create-new handles and does not follow replacement symlinks; Unix
  hard-link reads are rejected (`2202885`).
- Evidence: `setup::restore_control_files`, `rollback_branches` and the setup
  recovery path return combined diagnostics; failure-injection tests assert
  that all cleanup failures remain visible.
- Impact: Partial setup failures are no longer silently reported as a single
  unrelated primary error.
- Recommended remediation: Retain aggregated rollback reporting; add
  platform-specific permission/crash-recovery tests before distribution.

### GPR-010 - Push-race classification parses localized human-readable stderr

- Category: Git integration
- Severity: **MEDIUM**
- Confidence: High
- Status: **Fixed** in `b4ff279`.
- File/symbol: `src/git.rs`, porcelain push status parser
- Description: Push uses porcelain output and classifies only the structured
  `! ... [rejected]` ref-status record as a retryable non-fast-forward race.
- Evidence: `git::push` passes `--porcelain`; `is_non_fast_forward_rejection`
  parses tab-separated bytes, with tests distinguishing non-fast-forward and
  hook rejection statuses.
- Impact: Localization and arbitrary hook prose no longer control retry
  classification.
- Recommended remediation: Retain porcelain parsing and add coverage if the
  supported Git minimum changes.

### GPR-011 - Valid non-UTF-8 Git objects stop synchronization

- Category: Git compatibility
- Severity: **MEDIUM**
- Confidence: High
- Status: **Partially fixed with an explicit compatibility boundary** in
  `f283455`.
- File/symbol: `src/commands/sync.rs`, `src/commands/resolve.rs`, byte-path and message handling
- Description: Raw Git path bytes are now used for comparisons, diagnostics and
  Unix filesystem conversion; invalid conflict paths are escaped rather than
  dropped. Non-UTF-8 commit messages, branch names and tree entry names are
  rejected before a generated commit/ref can be advanced because the current
  mapping and configuration interfaces require text.
- Evidence: `git::escape_bytes`, `path_from_git_bytes`, `name_bytes` and
  `message_bytes` checks are covered by Unix byte-path, invalid-index and
  non-UTF-8-message tests. README documents the platform boundary.
- Residual scenario: A Unix repository containing a non-UTF-8 branch/tree name
  or commit message still fails clearly rather than syncing that object.
- Impact: Silent metadata loss and unsafe display are fixed; full arbitrary-byte
  synchronization remains intentionally unsupported.
- Recommended remediation: Keep the explicit byte-compatibility boundary;
  only implement arbitrary-byte commit/tree-name support if a real workflow
  requires it.

### GPR-012 - Work and subprocess output are unbounded

- Category: Performance/availability
- Severity: **MEDIUM**
- Confidence: High
- Status: **Fixed for the identified resource-exhaustion paths** in `f91c5b4`
  and `aaf31db`.
- File/symbol: `src/commands/sync.rs`, `src/git.rs`, bounded traversal and subprocess runner
- Description: Repository-controlled work and Git subprocesses now have static
  budgets. Pending histories, marker scans, branch discovery, tree traversal,
  conflict records, control files and commit messages are bounded. Git children
  are captured concurrently with per-stream/total output limits, closed stdin,
  `GIT_TERMINAL_PROMPT=0`, and a configurable 1..3600 second deadline.
- Evidence: `src/limits.rs`, `git::run_git_output_with_timeout` and the bounded
  merge-tree/conflict parsers fail rather than truncating data. README records
  the limits; tests cover timeout, output-limit and resource-limit behavior.
- Residual scenario: Valid large repositories above the documented budgets are
  rejected, and filtering still performs real object work per pending commit.
  These are explicit capacity limits, not unbounded execution.
- Impact: The original unbounded memory/output/hang paths are mitigated.
- Recommended remediation: Measure real repositories and tune limits from
  evidence; do not silently truncate data or remove deadlines.

### GPR-013 - Some untrusted output reaches terminals unsafely

- Category: Terminal safety
- Severity: **MEDIUM**
- Confidence: High
- Status: **Fixed** in `b064afa` and `f283455`.
- File/symbol: `src/git.rs`, `src/progress.rs`
- Description: Git diagnostics and repository-controlled byte values are now
  captured, escaped and bounded before application-authored terminal output.
  Valid branch names remain readable; controls, invalid UTF-8 and backslashes
  use deterministic escapes.
- Evidence: `git::escape_bytes`, `git_diagnostic`, bounded subprocess capture
  and progress rendering tests cover ANSI/control/newline and hostile branch
  values. Remote stderr is no longer inherited by production Git runners.
- Residual scenario: Trusted Git hooks and helpers may print directly because
  they remain part of the explicit Git installation trust boundary.
- Impact: Repository/remote values no longer provide an application-level
  terminal escape or forged status line.
- Recommended remediation: Retain byte escaping/framing for application
  diagnostics and document the trusted Git hook boundary.

### GPR-014 - Release engineering is incomplete

- Category: Distribution
- Severity: **MEDIUM**
- Confidence: High
- Status: **Partially fixed** in `d512c91` plus the owner-approved metadata
  update.
- Files: `Cargo.toml`, `LICENSE`, `README.md`, `.github/workflows/ci.yml`,
  `.github/dependabot.yml`, `deny.toml`
- Evidence:
  - Pinned-SHA CI now runs Rust 1.89 formatting, Clippy, locked tests and a
    locked release build, plus Linux/macOS/Windows tests.
  - Dependabot, `deny.toml`, `cargo audit` and `cargo deny` checks are present.
  - The owner-approved MIT `LICENSE`, canonical repository URL, Cargo
    `license = "MIT"`, `repository` metadata and `publish = false` are now
    present. README documents source installation and the no-crates.io status.
  - Release-tag/version policy, signed artifact workflow, release archives,
    installer and package-manager integration remain intentionally open.
- Impact: Licensing and project identity are now explicit, but public
  distribution is not yet operationally complete.
- Recommended remediation: Decide the release tag/version and signing
  policies, then add reproducible release archives/checksums and installation
  channels. Retain `publish = false` unless the owner explicitly changes the
  no-crates.io decision.

## 6. Low and Informational Findings

- **LOW - Explicit external config paths remain operator-controlled.** A user
  can deliberately pass a relative `--config` containing `..`; this is not
  repository-filename traversal. Policy files used by mutating commands are
  regular-file checked and authenticated before use.
- **INFO - Configuration validation is now explicit.** Unknown TOML fields,
  empty identity/URLs, invalid or duplicate branches and oversized control
  files are rejected (`b4ff279`, `aaf31db`).
- **INFO - No production unsafe Rust.** The only `unsafe` blocks are test-only
  environment mutation required under Edition 2024.
- **INFO - No direct shell invocation.** There is no `sh -c`, `bash -c` or
  PowerShell execution in production. Direct `Command::arg` use is not shell
  injection; Git's own hooks/helpers remain a trusted boundary.
- **INFO - Dependency shape is restrained.** Ten direct runtime dependencies,
  no Git dependencies, no project `build.rs`, and no duplicate versions in the
  default `cargo tree --duplicates` check. `git2` brings native libgit2/zlib
  build dependencies.
- **INFO - The two existing untracked root files are pager-help output and were present before review; they were not treated as repository source.**

## 7. Git and Subprocess Security Assessment

Positive properties:

- Production subprocess calls are centralized through a runner with bounded
  concurrent capture, closed stdin, `GIT_TERMINAL_PROMPT=0` and a deadline.
- Arguments use `Command::arg`, not shell strings.
- Remote values are validated and separated from Git options with `--`.
- OIDs passed to cherry-pick and merge-tree are structurally safe.
- Branch names are validated before branch/refspec construction.
- Push uses a full destination ref and never passes `--force`.
- Exit status is checked everywhere.
- Merge conflicts use Git's real merge engine.
- Source and destination push races are retried with recomputation.
- Push race classification parses porcelain ref-status bytes, not localized
  prose.
- Git version `>=2.45` is checked before merge-tree synchronization.
- Git child environments remove the state key and resolved fallback URLs while
  retaining normal authentication variables.

Problems:

- `git` is resolved through `PATH`, normal for a CLI but therefore part of the
  trusted execution environment.
- Repository-local `core.sshCommand`, transport settings, credential helpers
  and hooks can execute programs. Normal clones do not transfer hooks or
  `.git/config`, but a shared or attacker-prepared `.git` directory cannot be
  treated as safe.
- `resolve` deliberately runs ordinary Git porcelain and may execute local
  Git hooks; the configured identity is deterministic, not a sandbox.
- A timeout is enforced, but a valid large operation can still consume the
  configured deadline and must be retried or investigated by an operator.

Edge-case behavior:

- Detached HEAD: setup rejects; sync operates on local refs; dest-to-source
  resolve requires its source branch checked out, while source-to-dest resolve
  uses its authenticated linked worktree.
- Bare repositories: intentionally rejected for mutating source commands,
  though tests use bare remotes.
- Worktrees: repository discovery and linked worktree state use Git's common
  directory; the operation lock is shared by linked worktrees.
- Unborn/no-commit repositories: setup supports a fresh source when destination branches exist; empty destination repositories are untested.
- Missing/deleted branches: explicit behavior exists for round-tripped and mirror-only branches.
- Submodules: gitlinks are preserved and not traversed; a dedicated end-to-end
  submodule workflow remains an open test gap.
- Corruption: libgit2/Git failures propagate with context, resource limits
  reject oversized/malformed work, and rollback failures are aggregated.

## 8. Filesystem Safety Assessment

Strengths:

- Repository discovery and root-relative config resolution are clear.
- Filtering uses Git trees, not a filesystem walker.
- Excluded directories are pruned without descending.
- Symlink blobs and executable modes are preserved.
- Submodule gitlinks are not traversed.
- Checkout is non-forced.
- Policy/control files must be regular files, are size-bounded and are
  authenticated before mutation.
- Source/destination local ref advancement uses a safe checkout and CAS.
- Authenticated source-to-destination resolution records the exact temporary
  worktree path, verifies that it is a registered worktree for the same Git
  common directory, and checks its HEAD before continuing (`2202885`).
- Control-file reads open a handle and reject symlink/substitution and
  Unix hard-link cases; rollback restores entries by removing the existing
  directory entry and creating a new regular file rather than following it.
  The create-new restoration strategy is portable; hard-link rejection is
  Unix-specific (`2202885`).
- No general recursive deletion exists.
- No repository-derived path is directly passed to `remove_dir_all` or similar destructive APIs.

Risks:

- An explicit relative `--config` containing `..` can read outside the
  repository. This is direct user intent, not traversal by a repository
  filename, and is bounded by the operator-selected path.
- Setup still performs several filesystem/ref operations that cannot be made a
  single filesystem transaction. Rollback now attempts every action and
  reports all failures, but a hard crash can leave ordinary Git recovery work.
- Checkout can materialize repository-controlled symlinks, as Git normally
  does; gitprism itself does not subsequently follow those symlinks for
  arbitrary writes.
- The control-file replacement sequence is deliberately fail-closed for
  symlinks and Unix hard links, but it is still a sequence of filesystem
  operations rather than a cross-platform atomic no-follow primitive; a
  hostile local process racing the operator's repository remains outside
  gitprism's trust boundary.

No confirmed path was found where a tree filename alone causes gitprism to
delete or overwrite a filesystem object outside the repository.

## 9. Testing Gaps

Covered by the current 186-test suite:

- Leading-dash and malformed remote values, validated branches and raw
  refspec rejection.
- Authenticated/forged/duplicate marker state and protected policy digests.
- Credential redaction, terminal control escaping and invalid Git bytes.
- Source-to-dest and dest-to-source conflict reproduction, explicit operator
  continuation, excluded-path protection and stale state.
- CAS local ref movement, operation-lock behavior, dirty worktrees and
  untracked-file collisions.
- Configured Git identity, rollback aggregation, bounded Git output/deadlines
  and repository-controlled resource limits.
- Unicode/non-UTF-8 path diagnostics and Unix byte-path preservation.
- Authenticated exact resolution-worktree identity, registered common-directory
  checks, and symlink/Unix-hard-link-safe control-file read/restore behavior
  (`2202885`).

Still-open tests and validation:

1. Execute the pinned GitHub Actions matrix on Linux, macOS and Windows; the
   workflow is present but has not run in this local environment.
2. Add dedicated worktree, submodule/gitlink, nested-repository and malformed
   object integration scenarios.
3. Add empty-destination and unborn-branch workflow tests.
4. Exercise missing `git`, hostile Git configuration and permission failures
   on each supported platform.
5. Add large-history benchmarks and verify practical behavior at each static
   resource limit.
6. Decide and test a release artifact smoke-test/install workflow after the
   owner selects tags, signing and distribution channels.

The current suite is strongest around merge correctness, filtering, races at remote push, branch deletion policy, setup rollback under ref locking, rename behavior and conflict detection.

## 10. Architecture Assessment

Strengths:

- Architecture decisions are unusually well recorded and connected to prior art.
- `run(cwd, config_path)` functions make repository-level tests straightforward.
- Subprocess construction is centralized in `git.rs`.
- Filtering, config and progress concerns have dedicated modules.
- The two sync directions share `merge-tree`, avoiding divergent conflict semantics.
- Object-only sync avoids unnecessary working-tree mutation.
- The system uses fast-forward pushes and compare-and-swap correctly in `resolve`.
- Setup/sync/resolve mutations share a non-blocking lock in Git's common
  directory, including linked worktrees.
- Repository-controlled policy and generated mapping state have separate
  external authentication/verification boundaries.
- Static repository/work/process budgets fail clearly rather than truncating or
  silently dropping data.
- Error propagation is generally contextual and user-oriented.
- No production global mutable state.

Weaknesses:

- `sync.rs` combines orchestration, state discovery, filtering, marker parsing, commit construction, branch policy and local ref mutation. Its production portion is about 1,500 lines; its test module takes the file beyond 4,700.
- The deployment policy digest and state key are external trust inputs; a
  deployment that approves the wrong digest/key can still authorize the wrong
  repository policy by design.
- Git subprocess execution is centralized and hardened, but the Git executable,
  configuration, credential helpers and hooks remain intentionally trusted.
- `anyhow` is appropriate at the CLI boundary, but a few more typed outcomes would remove the need to parse Git prose and improve failure testing.
- Several design documents remain drafts despite implemented behavior; those
  status labels should be reconciled before a public release.

A rewrite is not warranted. The current structure can be strengthened by
introducing validated domain types and extracting marker/state and
branch-advancement logic from `sync.rs`.

## 11. Recommended Action Plan

### P0 - Immediate

| Recommendation | Impact | Effort |
|---|---|---|
| Protect `GITPRISM_POLICY_SHA256` and `GITPRISM_STATE_KEY` in the deployment system, and require approved source revisions. | High | Medium |
| Run a pilot against a real source/destination pair with backup/recovery procedures before enabling unattended mutation. | High | Medium |

### P1 - Before Release

| Recommendation | Impact | Effort |
|---|---|---|
| Execute and require the pinned Linux/macOS/Windows Rust 1.89 CI matrix, including `cargo audit` and `cargo deny`. | High | Small |
| Add worktree, submodule, malformed-object and empty-repository integration coverage. | Medium | Medium |
| Choose and document the release tag/version policy and signing policy. | High | Medium |

### P2 - Near Term

| Recommendation | Impact | Effort |
|---|---|---|
| Extract validated branch/remote/state/ref-update components from the large command modules. | Medium | Medium |
| Add release artifact packaging, checksums and installation instructions after owner decisions. | Medium | Medium/Large |
| Add practical large-history benchmarks and tune documented static budgets from real workloads. | Medium | Medium |
| Add a dedicated test for trusted Git hook/helper behavior and document that it is outside gitprism's sandbox. | Low | Small |

### P3 - Opportunistic

| Recommendation | Impact | Effort |
|---|---|---|
| Split large inline test modules into integration/support modules. | Low | Medium |
| Add platform-specific artifact smoke tests and installer/package-manager integrations. | Low | Medium/Large |
| Revisit full arbitrary-byte commit-message/tree-name support only if a real workflow requires it. | Low | Large |

## 12. Top 10 Recommendations

1. Protect the external policy digest and authenticated state key in CI.
2. Run a real source/destination pilot with documented rollback and recovery.
3. Execute and require the pinned Linux/macOS/Windows Rust 1.89 workflow.
4. Add worktree, submodule, malformed-object and empty-repository integration tests.
5. Decide and document the release version/tag policy.
6. Decide whether and how release artifacts are signed and verified.
7. Add reproducible platform archives and SHA-256 checksums after those decisions.
8. Add a dedicated test for the trusted Git hook/helper boundary.
9. Extract state/ref-update/validated-input components from the large command modules.
10. Benchmark real large repositories and tune the documented resource budgets.
