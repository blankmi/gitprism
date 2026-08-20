# gitprism Repository Review

Review date: 2026-08-20

## 1. Executive Summary

gitprism has a coherent core design, strong normal-path tests, idiomatic Rust, and careful fast-forward and conflict handling. The hybrid `git2`/Git CLI approach is reasonable for this tool.

It is **not ready for production distribution**. One confirmed Git option-injection path can execute an attacker-selected command, and several high-severity integrity/confidentiality issues remain around raw refspecs, forgeable state trailers, repository-controlled policy, credential-bearing URL disclosure, and incomplete conflict recovery.

| Area | Score | Rationale |
|---|---:|---|
| Architecture | 7/10 | Clear decisions and separation, but repository-controlled policy/state needs a stronger trust model. |
| Rust Code Quality | 8/10 | Idiomatic `Result` use, contextual errors, no production `unsafe`, clean Clippy. |
| Git Integration | 6/10 | Good merge-tree and fast-forward model; unsafe argument boundaries and textual status parsing remain. |
| Security | 4/10 | Confirmed command execution through Git option injection; credential and policy risks. |
| Reliability | 6/10 | Strong happy-path behavior, but partial-failure and concurrency gaps remain. |
| Testing | 8/10 | 120 passing tests with realistic repositories; hostile-input and failure-injection coverage is missing. |
| Cross-Platform Robustness | 5/10 | Mostly portable code, but only macOS verified, non-UTF-8 objects fail, and Git >=2.45 is not documented prominently. |
| Maintainability | 6/10 | Excellent rationale documentation; `sync.rs` and embedded test modules are very large. |
| Production Readiness | 4/10 | Security blockers plus no CI/release/packaging/license metadata. |

Release recommendation: **No-go until P0 and P1 findings are resolved.**

Verification performed:

- `cargo fmt --all -- --check`: passed.
- Strict workspace Clippy with all targets/features: passed.
- `cargo test --workspace --all-features`: 120 passed.
- Locked release build: passed.
- `cargo audit` and `cargo deny`: not installed; no vulnerability-clean claim can be made.
- Only `aarch64-apple-darwin` is installed, so cross-target compilation was not verified.
- No tracked files other than this report were modified.

## 2. Architecture Overview

The actual execution flow is:

```text
Clap CLI
  -> repository discovery and config loading
  -> setup / sync / resolve orchestration
  -> git2 object/ref/tree operations
  -> centralized Git subprocess helpers
  -> libgit2 checkout or terminal reporting
```

Primary entry point: `src/main.rs:14`.

Commands:

- `setup`: discovers a non-bare source repository, loads bootstrap control files, fetches every configured destination branch, plans grafts/merges, creates commits and branch refs, then checks out the first branch. See `src/commands/setup.rs:45`.
- `sync`: lists source branches, processes configured dest->source branches first, then mirrors every local source branch source->dest. See `src/commands/sync.rs:79`.
- `resolve`: only resolves dest->source conflicts using real `git cherry-pick`, then replaces Git's temporary commit with a correctly stamped commit and pushes it. See `src/commands/resolve.rs:37`.

Git integration is hybrid:

- `git2`: discovery, commits, refs, trees, revwalks, merge-base, checkout.
- Real `git`: fetch, ls-remote, push, cherry-pick, commit, ls-files, merge-tree, version checks. These nine process paths are centralized in `src/git.rs`.
- No Rust shell invocation is present.
- No application database or state file exists. Mapping state is encoded in commit trailers.

Filesystem interaction is limited mainly to config/control-file loading, setup's control-file replacement, libgit2 checkout, and `CHERRY_PICK_HEAD`. Filtering operates against Git objects rather than traversing the working directory.

## 3. Threat Model

Trusted by the current implementation:

- The `git` executable selected through `PATH`.
- Process environment and global/system Git configuration.
- Repository-local `.git/config`, hooks, and repository metadata.
- `.gitprism.toml` and `.gitprismignore` in the current source checkout.
- The convention that only gitprism authors `Gitprism-*` trailers.

Untrusted or repository-controlled:

- Branches, refs, commit graphs, commit messages, authors and filenames.
- Git object contents, including malformed or very large trees/histories.
- Configured URLs and branch strings.
- Remote Git stderr and hook/server messages.
- Working-tree state and symlinks.

Primary boundaries:

- Rust -> Git CLI argument parsing.
- Git CLI -> credential helpers, SSH, hooks, transport helpers and remote servers.
- Repository object graph -> commit filtering and sync-state selection.
- Checkout/ref updates -> the user's local working tree and history.
- Configured remote -> source/destination confidentiality boundary.

The implementation does not satisfy a threat model where committed control files or `.git` metadata are fully hostile. In particular, ordinary cloned content can supply a malicious `.gitprism.toml`, while manually supplied/shared `.git` metadata can additionally influence Git transport and hooks.

## 4. Critical and High Findings

### GPR-001 - Git option injection permits command execution

- Category: Subprocess security
- Severity: **CRITICAL**
- Confidence: High
- File/symbol: `src/git.rs:46`, `remote_ref_exists`; also `src/git.rs:21`, `fetch`
- Description: Remote URLs are placed directly where Git still parses options, without `--` or validation.
- Evidence: `remote_ref_exists` constructs `git ls-remote --exit-code <url> <refname>`. A URL beginning with `--upload-pack=...` is interpreted as a Git option, not a repository. A read-only probe confirmed that Git executed the selected upload-pack command; the resulting output caused Git's expected protocol error.
- Scenario: A source checkout contains `[dest].url = "--upload-pack=<command>"`, or the corresponding environment variable supplies it. `sync` calls `remote_ref_exists` before fetching and Git executes the supplied command.
- Impact: Arbitrary command execution with gitprism's privileges.
- Recommended remediation:
  - Reject remote values beginning with `-`.
  - Insert `--`/`--end-of-options` before every remote argument where Git supports it.
  - Test all remote-taking helpers with leading-dash values.
  - Consider a typed `RemoteUrl` validated once during config loading.

This is argument injection into Git, not shell interpolation by `Command::arg()`.

### GPR-002 - `fetch` accepts a raw refspec capable of modifying local refs

- Category: Git safety
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/git.rs:21`, `src/commands/setup.rs:164`
- Description: Config entries described as branch names are passed to `git fetch` as unrestricted refspecs.
- Evidence: `fetch` passes `refspec` verbatim. Setup calls it before validating that the configured string is a branch name. Git refspecs can include `+<source>:<destination>` and therefore update local refs.
- Scenario: A malicious configuration uses a forced remote-to-local refspec. During `setup`, fetch can move an unrelated local branch before later libgit2 operations reject the configured "branch" as invalid. Setup rollback does not record or restore fetch-created ref changes.
- Impact: Local branch overwrite or unexpected ref creation; potentially lost or hidden local work.
- Recommended remediation:
  - Validate every configured branch using Git's branch-name rules before any subprocess.
  - Fetch a constructed source ref, such as `refs/heads/<validated-name>`, with no destination component.
  - Add `--` before the ref argument.
  - Test that colon, plus, leading-dash, empty and malformed values cause no repository mutation.

### GPR-003 - Repository authors can forge synchronization state

- Category: Git correctness/security
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/commands/sync.rs:802`, `src/commands/sync.rs:477`, `src/commands/sync.rs:1363`
- Description: Any matching line anywhere in any commit message is trusted as authoritative gitprism state.
- Evidence:
  - `trailer_value` returns the first matching line anywhere in the message, not a validated trailer block.
  - Any source commit containing `Gitprism-Dest-Commit` is skipped source->dest.
  - Any destination commit containing `Gitprism-Source-Commit` is skipped dest->source.
  - The test fixture explicitly allows independently authored destination commits to carry the marker, reinforcing that provenance is not checked.
- Scenario: A destination contributor adds `Gitprism-Source-Commit: <oid>` to a commit message. Its content is silently excluded from dest->source reconciliation. A source contributor can similarly suppress export of a source commit. Duplicate forged trailers can also override the genuine trailer gitprism appends later because parsing takes the first occurrence.
- Impact: Silent divergence and failure of the core bidirectional-sync guarantee.
- Recommended remediation:
  - At minimum, parse only a canonical final trailer block and reject duplicate marker keys.
  - Validate marker OIDs against expected ancestry and commit shape.
  - Do not use marker presence alone as proof that gitprism authored a commit.
  - Revisit authenticated or structurally verifiable state, such as a dedicated ref/notes plus commit-shape validation.

### GPR-004 - Versioned config and exclusion policy are consumed from an untrusted checkout

- Category: Architecture/security
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/config.rs:91`, `src/commands/sync.rs:91`, `src/commands/sync.rs:814`
- Description: The same repository being processed controls remote destinations and which protected paths may leave source.
- Evidence:
  - `sync` loads `.gitprism.toml` from the current working tree.
  - Each branch's `.gitprismignore` is loaded from that branch's tip.
  - Source->dest mirrors every local branch.
  - The deployment playbook recommends running `sync` on every source push.
- Scenario: A branch modifies `.gitprismignore` to remove protected patterns, or changes `.gitprism.toml` to an attacker-controlled destination. CI checks out that branch and invokes `sync` before the policy change has been reviewed.
- Impact: Source-only content can be published to the wrong destination or cease being excluded.
- Recommended remediation:
  - Define and document who is trusted to modify both control files.
  - In CI, load remote configuration and exclusion policy from a protected ref or external protected configuration.
  - Consider rejecting policy changes unless the run is on a configured protected branch.
  - Test branch-local policy changes explicitly.

If every source writer is fully trusted and CI only runs protected commits, the practical severity falls, but the malicious-repository threat model is not met.

### GPR-005 - Source->dest conflict recovery directs users to an incompatible command

- Category: Correctness/UX
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/commands/sync.rs:377`, `src/commands/resolve.rs:1`, `design/decisions/0015-resolve-real-git-cherry-pick-explicit-continue.md:109`
- Description: Source->dest conflicts tell users to run `gitprism resolve`, but `resolve` only handles dest->source.
- Evidence: The source->dest error names `gitprism resolve`; the resolve implementation recomputes pending destination commits and pushes to source. Decision 0015 explicitly says source->dest resolution is not implemented.
- Scenario: A mirror-only destination branch has an independent edit conflicting with an incoming filtered source commit. Sync stops and recommends a command that either rejects the branch as unconfigured or operates on the wrong direction.
- Impact: A supported sync state has no correct documented recovery path and can block indefinitely.
- Recommended remediation:
  - Implement source->dest resolution before release, or emit accurate manual recovery instructions and clearly state that automatic resolution is unavailable.
  - Add an end-to-end source->dest conflict recovery test.

### GPR-006 - Credential-bearing URLs are copied into errors and CI logs

- Category: Secrets handling
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/git.rs:30`, `src/git.rs:98`, `src/commands/sync.rs:271`
- Description: Error contexts interpolate complete source and destination URLs.
- Evidence: Fetch, ls-remote and push error strings include `{url}`. The design explicitly supports credential-bearing environment URLs such as tokenized HTTPS remotes.
- Scenario: Authentication, network or hook failure occurs with `https://user:token@host/repo.git`.
- Impact: Credentials can be written to terminal history or persistent CI logs.
- Recommended remediation:
  - Never include raw remote URLs in errors.
  - Use labels such as "configured source remote" and "configured destination remote."
  - If a location is necessary, redact userinfo and sensitive query parameters centrally.
  - Add redaction tests.

### GPR-007 - Local branch advancement is not atomic with its safety check

- Category: Concurrency/data integrity
- Severity: **HIGH**
- Confidence: High
- File/symbol: `src/commands/sync.rs:1125`
- Description: `advance_local_source_branch` checks the current ref and later force-writes it without compare-and-swap.
- Evidence: It reads `previous_tip`, verifies ancestry, optionally updates the checkout, then calls `repo.reference(..., force = true, ...)`. Another process can move the branch between those operations. By contrast, `resolve::finish` correctly uses `reference_matching`.
- Scenario: A concurrent Git operation or second gitprism instance creates a local commit in the check/write window.
- Impact: The concurrent ref update can be overwritten. Separately, the source remote is pushed before local checkout/ref advancement; a dirty-worktree checkout failure can therefore leave the remote advanced and the local branch stale.
- Recommended remediation:
  - Use `reference_matching` with the exact observed OID.
  - Add a per-repository run lock covering setup/sync/resolve mutations.
  - Decide and test recovery when remote push succeeds but local advancement fails.
  - Check working-tree/index suitability before performing a dest->source push when the branch is checked out.

## 5. Medium Findings

### GPR-008 - `resolve` depends on unrelated Git identity configuration

- Category: Reliability/test isolation
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/git.rs:161`, `src/git.rs:194`
- Description: Real cherry-pick/commit subprocesses are not given the committer identity already present in `.gitprism.toml`.
- Evidence: Only `GIT_EDITOR=true` is set. A clean cherry-pick or `--continue` needs a Git committer identity before gitprism can replace the temporary commit. The test suite passed because the review machine has a global Git identity.
- Scenario: A clean CI container has no `user.name`/`user.email`.
- Impact: Conflict resolution fails despite valid gitprism configuration; tests can fail based on global machine state.
- Recommended remediation: Supply `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL` from `Config`, or use a flow that never requires Git to author the temporary commit.

### GPR-009 - Setup rollback and restoration errors are silently discarded

- Category: Error handling/filesystem safety
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/commands/setup.rs:328`, `src/commands/setup.rs:374`
- Description: Failed control-file restoration, ref reset, branch deletion and HEAD restoration are ignored.
- Scenario: Setup fails because of permissions, locks or corruption--the same conditions likely to affect rollback.
- Impact: Partial branch mutations or missing control files can remain while the reported error describes only the original failure.
- Recommended remediation: Return an aggregated rollback error containing both the primary failure and every rollback failure. Avoid deleting control files until checkout can be made transactional.

### GPR-010 - Push-race classification parses localized human-readable stderr

- Category: Git integration
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/git.rs:104`
- Description: Non-fast-forward detection searches for English strings.
- Scenario: Git is localized, wording changes, or a remote hook emits matching text.
- Impact: Genuine races become fatal, or unrelated failures are retried and misreported.
- Recommended remediation: Use `git push --porcelain`/`--porcelain=v2` where available and parse ref-status records rather than prose.

### GPR-011 - Valid non-UTF-8 Git objects stop synchronization

- Category: Git compatibility
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/commands/sync.rs:160`, `src/commands/sync.rs:857`, `src/commands/resolve.rs:233`
- Description: Non-UTF-8 branch/tree names are rejected, conflict paths are dropped from diagnostics, and non-UTF-8 commit messages are replaced with an empty message before adding the trailer.
- Scenario: A Unix repository contains a legal byte-oriented filename or legacy-encoded commit message.
- Impact: Sync aborts or loses metadata.
- Recommended remediation: Use `TreeEntry::name_bytes` and byte-aware path matching on Unix, preserve `message_bytes`, and escape bytes only for display.

### GPR-012 - Work and subprocess output are unbounded

- Category: Performance/availability
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/commands/sync.rs:778`, `src/commands/sync.rs:847`, `src/git.rs:91`
- Description:
  - Entire pending histories are collected into vectors.
  - A `merge-tree` process is spawned per pending commit.
  - Source->dest recursively filters overlapping trees repeatedly.
  - Captured Git stdout/stderr has no size bound.
  - Network subprocesses have no timeout and inherited Git prompting behavior is inconsistent.
- Scenario: Large history, huge conflict list, noisy malicious remote, stalled SSH/authentication.
- Impact: Excessive runtime/memory use or indefinitely hanging CI jobs.
- Recommended remediation: Stream pending commits, bound captured diagnostic output, introduce configurable network timeouts/non-interactive mode, and benchmark large histories before optimizing tree reuse.

### GPR-013 - Some untrusted output reaches terminals unsafely

- Category: Terminal safety
- Severity: **MEDIUM**
- Confidence: High
- File/symbol: `src/git.rs:21`, `src/progress.rs:103`
- Description: Git fetch/ls-remote stderr is inherited, and raw URLs/stderr are embedded in final errors.
- Scenario: A remote server, hook, or malicious URL emits ANSI controls or newline-delimited misleading output.
- Impact: Terminal escape injection or forged-looking CI status lines.
- Recommended remediation: Keep valid branch names readable, but escape control characters in application-authored diagnostics and consider capturing/framing remote stderr. Conflict paths already use debug formatting in most important messages, which is appropriately conservative.

### GPR-014 - Release engineering is incomplete

- Category: Distribution
- Severity: **MEDIUM**
- Confidence: High
- Files: `Cargo.toml`, `README.md:55`
- Evidence:
  - No CI workflows, release configuration, packaging scripts or install artifacts.
  - No license file or Cargo `license`/`license-file`.
  - No `rust-version`, repository, description, keywords or categories metadata.
  - README does not prominently state the enforced Git >=2.45 requirement.
  - No Windows/Linux build or test matrix.
- Impact: Reproducibility, legal distribution and platform support are not release-grade.
- Recommended remediation: Add release metadata, license, CI matrices, locked release builds, artifact signing/checksums and documented compatibility requirements.

## 6. Low and Informational Findings

- **LOW - Configuration accepts unknown fields and lacks semantic validation.** A typo can silently default a section or branch list. Add `#[serde(deny_unknown_fields)]`, reject empty URLs/identity values, invalid branches and duplicates.
- **LOW - README test count is stale.** It says 82; the current suite has 120.
- **INFO - No production unsafe Rust.** The only `unsafe` blocks are test-only environment mutation required under Edition 2024.
- **INFO - No direct shell invocation.** There is no `sh -c`, `bash -c` or PowerShell. GPR-001 occurs because Git interprets an argument as an option and then launches its own helper.
- **INFO - Dependency shape is restrained.** Nine direct runtime dependencies, no Git dependencies, no project `build.rs`, and no reachable duplicate versions reported by `cargo tree --duplicates`. `git2` brings native libgit2/zlib build dependencies.
- **INFO - The two existing untracked root files are pager-help output and were present before review; they were not treated as repository source.**

## 7. Git and Subprocess Security Assessment

Positive properties:

- Subprocess calls are centralized.
- Arguments use `Command::arg`, not shell strings.
- OIDs passed to cherry-pick and merge-tree are structurally safe.
- Push uses a full destination ref and never passes `--force`.
- Exit status is checked everywhere.
- Merge conflicts use Git's real merge engine.
- Source and destination push races are retried with recomputation.

Problems:

- Remote operands need explicit option termination.
- Fetch needs a validated source ref, not an arbitrary refspec.
- Git environment/config is inherited wholesale.
- `git` is resolved through `PATH`, normal for a CLI but therefore part of the trusted execution environment.
- Repository-local `core.sshCommand`, transport settings, helpers and hooks can execute programs. Normal clones do not transfer hooks or `.git/config`, but a shared or attacker-prepared `.git` directory cannot be treated as safe.
- `resolve` runs porcelain operations that may execute local Git hooks.
- Network calls have no timeout or explicit non-interactive policy.
- Push status parsing is brittle.

Edge-case behavior:

- Detached HEAD: setup rejects; sync operates on local refs; resolve requires the target checked out.
- Bare repositories: intentionally rejected for commands, though tests use bare remotes.
- Worktrees: `Repository::discover`, `workdir()` and `repo.path()/CHERRY_PICK_HEAD` are structurally compatible, but untested.
- Unborn/no-commit repositories: setup supports a fresh source when destination branches exist; empty destination repositories are untested.
- Missing/deleted branches: explicit behavior exists for round-tripped and mirror-only branches.
- Submodules: gitlinks are preserved and not traversed, but there is no dedicated submodule integration test.
- Corruption: most libgit2/Git failures propagate with context; several rollback and `.ok()` paths weaken this.

## 8. Filesystem Safety Assessment

Strengths:

- Repository discovery and root-relative config resolution are clear.
- Filtering uses Git trees, not a filesystem walker.
- Excluded directories are pruned without descending.
- Symlink blobs and executable modes are preserved.
- Submodule gitlinks are not traversed.
- Checkout is non-forced.
- No general recursive deletion exists.
- No repository-derived path is directly passed to `remove_dir_all` or similar destructive APIs.

Risks:

- Setup follows config/control-file symlinks during `read_to_string`.
- An explicit relative `--config` containing `..` can read outside the repository. This is direct user intent, not traversal by a repository filename, but should be documented.
- Setup deletes control files before checkout, and partial failures are not fully transactional.
- Refspec injection is a more serious repository-boundary mutation than any ordinary filesystem path operation.
- Checkout can materialize repository-controlled symlinks, as Git normally does; gitprism itself does not subsequently follow those symlinks for arbitrary writes.

No confirmed path was found where a tree filename alone causes gitprism to delete or overwrite a filesystem object outside the repository.

## 9. Testing Gaps

Highest-priority missing tests:

1. Leading-dash source/destination URLs for every subprocess helper.
2. Branch values containing `+`, `:`, leading `-`, empty strings and malformed refs, asserting no ref mutation.
3. Forged, duplicated and malformed `Gitprism-*` trailers authored independently.
4. Source->dest conflict recovery end to end.
5. Credential URL redaction in every failure path.
6. Concurrent local ref update between validation and write.
7. Remote push success followed by dirty-worktree/local-ref update failure.
8. `resolve` with global/system Git identity disabled.
9. Missing `git` executable and hostile Git environment/config.
10. Non-UTF-8 filenames, commit messages and conflict paths on Unix.
11. Newline/ANSI content in remote diagnostics and config values.
12. Worktrees, submodules/gitlinks, nested repositories and malformed objects.
13. Empty destination repository and unborn branch behavior.
14. Permission failures during control-file deletion/restoration and rollback.
15. Large-history/resource-bound tests and subprocess timeout behavior.
16. CLI parsing tests for global `--config`, `resolve --continue`, and invalid combinations.
17. Linux and Windows CI integration tests.

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
- Error propagation is generally contextual and user-oriented.
- No production global mutable state.

Weaknesses:

- `sync.rs` combines orchestration, state discovery, filtering, marker parsing, commit construction, branch policy and local ref mutation. Its production portion is about 1,500 lines; its test module takes the file beyond 4,700.
- Marker parsing is too weak to serve as a security-sensitive source of truth.
- The configuration/exclusion-policy trust model conflicts with malicious-repository handling.
- Git subprocess execution is centralized but not modeled through a small runner that can enforce option termination, environment policy, redaction and output bounds consistently.
- `anyhow` is appropriate at the CLI boundary, but a few more typed outcomes would remove the need to parse Git prose and improve failure testing.
- Several design documents are drafts despite implemented behavior, while README descriptions occasionally overstate `resolve` coverage.

A rewrite is not warranted. The current structure can be strengthened by hardening `git.rs`, introducing validated domain types, and extracting marker/state and branch-advancement logic from `sync.rs`.

## 11. Recommended Action Plan

### P0 - Immediate

| Recommendation | Impact | Effort |
|---|---|---|
| Terminate Git option parsing and reject leading-dash remotes. | High | Small |
| Validate branch names before any subprocess and fetch only fully qualified source refs. | High | Small |
| Redesign or strictly verify trailer provenance; reject duplicates and forged marker shapes. | High | Medium/Large |
| Remove raw remote URLs from all errors and logs. | High | Small |
| Establish a protected source for remote and exclusion policy in CI. | High | Medium |

### P1 - Before Release

| Recommendation | Impact | Effort |
|---|---|---|
| Implement or accurately document source->dest conflict recovery. | High | Medium/Large |
| Make local ref advancement CAS-based and define post-push recovery. | High | Medium |
| Add a repository operation lock. | High | Medium |
| Use configured committer identity for resolve subprocesses. | Medium | Small |
| Stop discarding rollback failures. | Medium | Medium |
| Replace push prose parsing with porcelain output. | Medium | Medium |
| Add the hostile-input and failure-atomicity tests listed above. | High | Medium |

### P2 - Near Term

| Recommendation | Impact | Effort |
|---|---|---|
| Add bounded output, non-interactive mode and configurable subprocess timeouts. | Medium | Medium |
| Support or explicitly reject non-UTF-8 Git objects with documented behavior. | Medium | Medium/Large |
| Extract validated `BranchName`, redacted `Remote`, marker parser and ref-update components. | Medium | Medium |
| Add Linux/macOS/Windows CI plus worktree/submodule scenarios. | Medium | Medium |
| Add license, Cargo metadata, MSRV, Git minimum version and reproducible release artifacts. | Medium | Medium |
| Run `cargo audit`/`cargo deny` in CI with an explicit policy. | Medium | Small |

### P3 - Opportunistic

| Recommendation | Impact | Effort |
|---|---|---|
| Split large inline test modules into integration/support modules. | Low | Medium |
| Reject unknown TOML fields and duplicate branch entries. | Low | Small |
| Preserve non-UTF-8 commit messages byte-for-byte. | Low/Medium | Medium |
| Reconcile draft decision statuses and update the README test count. | Low | Small |

## 12. Top 10 Recommendations

1. Block leading-dash remote values and add option terminators to all Git commands.
2. Validate branch names and never pass config values as raw fetch refspecs.
3. Make synchronization markers unforgeable or structurally/provenance validated.
4. Redact remote URLs and credentials from every diagnostic.
5. Move config/exclusion policy to a protected trust boundary for CI execution.
6. Provide a real source->dest conflict-resolution path.
7. Use CAS ref updates and a repository-wide operation lock.
8. Make resolve independent of global Git identity and hooks where possible.
9. Add hostile-repository, non-UTF-8, dirty-worktree and partial-failure tests.
10. Establish cross-platform CI, dependency auditing and production release metadata.
