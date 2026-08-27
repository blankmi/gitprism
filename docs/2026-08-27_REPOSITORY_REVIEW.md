# gitprism Repository Review

Review date: 2026-08-27
Reviewed revision: package version 0.1.5, branch
`fix/mirror-only-nearest-ancestor-anchor`, HEAD `86430b2` (the `v0.1.5` tag
itself points to `9575158`).

No project files were modified as part of this review. The three reproduction
tests referenced below (F-01, F-02, F-04) were written against a scratch copy of
the crate; they are the primary evidence for those findings and are to be
preserved as regression tests in the corresponding fixes so the review remains
independently auditable.

## 1. Executive Summary

gitprism is a small, unusually well-reasoned Rust CLI (~5,000 production lines,
~15,000 test lines). The subprocess boundary, the authenticated marker scheme,
the byte-safe diagnostic/terminal layer and the compare-and-swap ref updates are
sound and well tested. The defects found cluster in the newest logic — decision
0043's sibling-anchor search and decision 0039's rewrite detection — where
**three confirmed, reproduced behavioural bugs** can move a destination ref
backwards, permanently break a routinely created branch, or duplicate already
imported commits. No memory-safety, command-injection or filesystem-escape
issue exists.

| Area | Score | Rationale |
|---|---:|---|
| Architecture | 7/10 | Clear layering (CLI → policy/auth → lock → command orchestration → git2/subprocess runner), decisions cited at code sites. `sync.rs` (2,893 production lines, 460-line main loop, three copies of one marker revwalk) is the weak point. |
| Rust code quality | 8/10 | Idiomatic; contextual `anyhow` errors; no production `unsafe`; no reachable panic on repo/CLI input found; fmt and clippy `-D warnings` clean. |
| Git integration | 6/10 | Plumbing layer is excellent (validated operands, `--`, porcelain parsing, version floor). Higher-level state logic has three confirmed correctness defects (F-01, F-02, F-04) and inconsistent abort semantics (F-05). |
| Security | 8/10 | No shell, no option injection, credentials redacted, policy and marker authentication, bounded work. One policy-bypass path via `resolve` (F-03); C1/U+2028 terminal gap in branch names (F-09). |
| Reliability | 6/10 | CAS, locking and bounded subprocesses are solid; F-01 and F-02 are regressions introduced by the two most recent decisions. |
| Testing | 7/10 | 240 tests, strong on hostile input, races and rollback. No setup→sync integration test, zero Windows-path tests, 7 of 12 limits untested, host git config leaks in, fixtures duplicated three ways. |
| Cross-platform robustness | 7/10 | Byte paths on Unix, explicit UTF-8 rejection elsewhere, verbatim-prefix handling for MSYS git; CI matrix present; Windows branches have no unit tests. |
| Maintainability | 6/10 | Excellent rationale density, but `sync.rs` is 10.7k lines with inline tests. |
| Production readiness | 5/10 | Release pipeline, licensing and dependency policy exist. Blocked on the three HIGH findings and on a real-pair pilot; no published artifact release has been produced yet (tags v0.1.0–v0.1.5 exist). |

**Recommendation: no-go for unattended production use until F-01, F-02 and
F-03 are fixed.**

Verification performed locally (aarch64-apple-darwin, git 2.50.1, Rust 1.89):

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: pass.
- `cargo test --workspace --all-features`: 240 passed, 0 failed, 0 ignored (5.4 s).
- `cargo tree --duplicates`: no duplicate versions.
- `cargo audit` / `cargo deny`: not installed locally; CI runs both, so no local
  vulnerability-clean claim is made.
- F-01, F-02 and F-04 were each reproduced by a new test in a scratch copy of
  the crate; all three fail against current code.
- `ext::`/`fd::` git transports verified blocked by git's default
  `protocol.allow` (`fatal: transport 'ext' not allowed`).

## 2. Architecture Overview

```text
clap (src/cli.rs) -> main.rs dispatch -> command::run(cwd=".", --config)
  ├ marker::load_key()        GITPRISM_STATE_KEY (64 hex) — validated before repo discovery
  ├ Repository::discover      git2 upward discovery; bare repo rejected
  ├ policy::load()            exact bytes of .gitprism.toml + .gitprismignore must
  │                           SHA-256-match GITPRISM_POLICY_SHA256 before parsing
  ├ lock::OperationLock       flock on <commondir>/gitprism.lock (shared by linked worktrees)
  ├ git::ensure_merge_tree_supported   git >= 2.45
  └ setup.rs / sync.rs / resolve.rs
       ├ git2: refs, revwalks, merge-base, treebuilder, commit, safe checkout, CAS ref update
       └ git.rs subprocess runner: fetch, ls-remote, push --porcelain,
          merge-tree --write-tree, cherry-pick, worktree add/remove, --version
          • Command::arg only, no shell  • stdin null  • GIT_TERMINAL_PROMPT=0
          • per-stream + total output caps  • 300 s deadline  • kill + reap
          • GITPRISM_STATE_KEY / GITPRISM_*_URL scrubbed from the child environment
       └ progress::Reporter -> stderr (indicatif bar on a tty, plain lines otherwise)
```

- **Entry points:** `setup`, `sync`, `resolve <branch> [--direction] [--continue]`,
  `policy-hash` (read-only). Global `--config` resolves relative to the
  discovered repo root.
- **Git strategy:** hybrid by decision 0002 — git2 for object-database work,
  real `git` for network, working-tree and merge-engine work. Direct `.git`
  reads are limited to `CHERRY_PICK_HEAD`, `worktrees/*/gitdir` and the linked
  worktree's `.git` pointer, all through a symlink/hardlink/size-checked reader.
- **State:** no standalone application state files; durable state lives in Git
  objects and refs. Mapping state is HMAC-authenticated trailers in
  commit messages; resolve's source→dest state is a signed commit under
  `refs/gitprism/resolve/source-to-dest/<branch>`.
- **Filesystem writes:** libgit2 safe (non-forced) checkout, byte-exact
  re-materialisation of the two control files (remove + `create_new`, mode via
  the open handle), setup's control-file snapshot/restore, a linked worktree
  under `temp_dir()` for source→dest resolution, and the lock file.
- **Platform-specific:** Unix byte paths, hard-link rejection and umask-aware
  creation modes; Windows verbatim-prefix stripping for MSYS git; non-UTF-8
  paths rejected off-Unix.

## 3. Threat Model

Trusted:

- `git` on `PATH`, its global/system/repo config, hooks, credential helpers and
  SSH — explicitly documented as a trusted boundary.
- Deployment variables `GITPRISM_POLICY_SHA256`, `GITPRISM_STATE_KEY`,
  `GITPRISM_SOURCE_URL`, `GITPRISM_DEST_URL`, `GITPRISM_GIT_TIMEOUT_SECONDS`.
- The operator invoking the tool and the working directory they run it in.

Untrusted / repository-controlled:

- Branch and tag names, commit graphs, messages, authors, tree entry names,
  blob contents, file modes, gitlinks and symlink targets — on both source and
  dest.
- `.gitprism.toml` / `.gitprismignore` bytes until digest-matched.
- All `Gitprism-*` trailer text until the HMAC block verifies.
- Remote stderr/stdout, hook and server messages.
- Working-tree contents, including symlinks materialised by checkout.

Boundaries: Rust → git argv (validated, `--`-separated); git → transports and
hooks; object graph → marker verification → resume/anchor selection → push
mode; checkout/CAS → operator's working tree; source ↔ dest confidentiality via
`filter_tree` before any merge.

Primary failure scenarios: (a) a wrong boundary/anchor decision leading to a
backwards push on dest (F-01), a permanently refusing branch (F-02) or
duplicated history (F-04); (b) excluded content reaching dest through a path
that skips the policy check (F-03); (c) a whole run aborting on one discovered
branch and starving every later-sorted branch (F-02, F-05); (d) terminal/log
forging via branch names (F-09). Command execution and out-of-repository
filesystem writes driven by repository content were not found.

## 4. Critical & High Findings

No CRITICAL findings.

### F-01 — HIGH — A stale (behind, non-shallow) clone is classified as a mirror-only rewrite and force-pushes dest backwards

- Category: Git safety / destructive operation
- Confidence: High (reproduced)
- File: `src/commands/sync.rs:1395-1401` (`mirror_only_rewrite_detected`);
  caller `src/commands/sync.rs:420-482`
- Description: When dest's newest `Gitprism-Source-Commit` marker names a
  source commit this clone does not have, `dest_resume_point_for_branch`
  (`:1323-1332`) correctly refuses and documents that this is what a *behind*
  clone looks like. `mirror_only_rewrite_detected` re-inspects the same
  condition and, on a non-shallow clone, returns `true` — a positive rewrite —
  so the caller selects `PushMode::ForceMirrorOnly { expected_dest: fetched_dest_tip }`.
  The lease matches (the clone fetched the current tip moments earlier), so
  the compare-and-swap passes and dest's newer projection is replaced by a
  rebuild from the stale clone's tip.
- Evidence:
  ```rust
  Err(error) if error.code() == git2::ErrorCode::NotFound => {
      return Ok(!repo.is_shallow());
  }
  ```
- Trigger (reproduced): clone A syncs mirror-only `task` at T1; a full
  `git clone` B of source is taken; A fast-forwards `task` to T2 and syncs
  (dest = D(T2)); B runs sync → "source branch was rewritten (mirror-only) —
  rebuilding dest's projection", returns `Ok(false)`, dest `task` moves from
  D(T2) to a chain containing only T1. Realistic: two CI pipelines for one
  branch finishing out of order on non-shallow runners; an out-of-date
  workstation clone.
- Impact: dest's published projection regresses; an open PR head jumps
  backwards; can flip-flop between runners. This is the steady-state
  force-push the project's hard constraint forbids.
- Remediation: "not in this clone" must not be positive evidence of a rewrite.
  Treat a missing boundary object as the ordinary refusal regardless of
  shallowness. Probing source's remote for the boundary oid is *not* an
  acceptable substitute: servers may reject direct OID fetches or hide
  unadvertised objects, so "remote also lacks it" is not positive evidence
  either. Any stronger recovery needs a separately established Git primitive
  and its own decision. Amend decision 0039's addendum and add the reproduction
  as a regression test.

### F-02 — HIGH — A branch created at a sibling's anchor with no commits of its own becomes permanently unsyncable and aborts every subsequent run

- Category: Correctness / availability
- Confidence: High (reproduced)
- File: `src/commands/sync.rs:675-678`, `:1219-1258` (`dest_tip_accounted_for`),
  `:483-485` (`bail!`), `run()` `:187-199`
- Description: under decision 0043 a new branch's `dest_tip` may be a sibling's
  dest commit. If the branch has no commits beyond the fork point (or they all
  filter to no-ops), `new_dest_tip = build.new_tip.or((!dest_ref_exists).then_some(dest_tip))`
  creates dest `task` pointing at that sibling commit. On the next run all
  three cases of `dest_tip_accounted_for` fail: the tip's marker is for the
  *sibling's* branch name (rejected by `marker::verify`, `src/marker.rs:320`);
  the merge-base is not the tip; and no `Gitprism-Dest-Commit` on `task` names
  it. `mirror_only_rewrite_detected` returns false, so the code reaches the
  unconditional `anyhow::bail!("dest branch … isn't at a point this clone can
  safely build on")`, which `run()` propagates with `?`.
- Trigger (reproduced): `feature` (mirror-only) synced;
  `git checkout -b task feature && git push` with no commit; sync creates dest
  `task`. Every later `sync` returns `Err` for `task` — still after a real
  commit is added. The same shape arises from the rewrite-rebuild arm and from
  a step-5 round-tripped anchor.
- Impact: one routinely created branch turns every subsequent run red and,
  because branches are processed in sorted order and the error is fatal, every
  branch sorting after it (including `main` when the name sorts before it)
  never gets its source→dest phase until an operator deletes the dest branch.
  New with 0043 — before it the baseline anchor was the setup graft, which
  case 2 accepts.
- Remediation: never create a dest ref that points at a commit gitprism cannot
  later recognise for *this* branch — when `build.new_tip` is `None` for a
  brand-new branch, build a branch-scoped empty marker commit on top of the
  anchor (as dest→source already does for no-op imports, `sync.rs:2813-2829`).
  Skipping the ref instead is not an option under decision 0017 (every source
  branch is mirrored) without a new architecture decision. Independently, make
  the "not safe to build on" refusal a per-branch `Outcome::Error` + `Ok(true)`
  (F-05).

### F-03 — HIGH — `resolve --direction source-to-dest` bypasses decision 0037's per-commit `.gitprismignore` policy halt and pushes to dest

- Category: Security / policy enforcement
- Confidence: High
- File: `src/commands/resolve.rs:287-353` (`start_source_to_dest`); compare
  `src/commands/sync.rs:591-612`
- Description: `sync` refuses to build or push anything for a branch whose
  replayed commits carry a `.gitprismignore` that differs from the pinned
  policy (fail-closed, decision 0037). `resolve`'s source→dest path recomputes
  the same pending list, builds `build_dest_commit` for every clean commit and
  at line 341 pushes the clean prefix (`PushMode::FastForwardOnly`) before
  setting up the conflict — with no call to `find_control_file_policy_mismatch`
  (private to `sync.rs`) in either `start_source_to_dest` or
  `finish_source_to_dest`.
- Trigger: a branch has a pending commit whose committed `.gitprismignore` adds
  an exclusion for a newly added sensitive directory, followed by a commit that
  conflicts on dest. `sync` halts the branch. The operator, following the
  conflict message, runs `gitprism resolve <branch> --direction source-to-dest`;
  the clean prefix — filtered only by the *pinned* list — is pushed to dest.
- Impact: reopens the disclosure path 0036/0037 were written to close, via a
  command documented as "recomputing exactly what `sync` would build next".
- Remediation: make the policy pre-pass `pub(crate)` and call it in
  `start_source_to_dest` before any build/push and in `finish_source_to_dest`
  for the resolved commit. Add a test mirroring `sync.rs:7593` for the resolve
  path.

## 5. Medium Findings

### F-04 — MEDIUM — A task branch forked after a dest→source import re-projects the imported commit as a duplicate on dest

- Category: Correctness (decision 0043)
- Confidence: High (reproduced)
- File: `src/commands/sync.rs:2569-2573`, `:1002-1012`, `src/marker.rs:320`
- Description: a `DestToSource` marker *M* on `main` (naming dest-native *X*)
  is rejected both by `scan_for_dest_marker(…, "task")` (branch mismatch) and
  by step 5's `newest_source_marker_at_or_before` (only accepts
  `SourceToDest`), so `task`'s anchor becomes `main`'s last source→dest commit
  *before X*, and *M* is not loop-prevented for `task`.
- Trigger (reproduced): dest-native *X* on dest `main`; `run()` imports it;
  `task` forked from `main`'s tip with one commit; `run()` → dest `task` has
  two commits not on dest `main`: the task commit and a duplicate "dest native X".
- Impact: exactly the "already-merged commits appear in the PR diff" symptom
  0043 set out to fix, for the most common topology (task off `main`). Content
  is merge-clean.
- Remediation: make loop prevention and the step-5 scan recognise
  `DestToSource` markers regardless of branch name when the marker commit is
  an ancestor of the branch tip, or resolve the anchor to the dest commit *M*
  names.

### F-05 — MEDIUM — Source→dest refusals for discovered branches are whole-run aborts; a same-named dest-native branch stalls all later branches

- Category: Reliability / availability
- Confidence: High
- File: `src/commands/sync.rs:483-485`, `:1181-1185` (`graft_point`), `:1253`,
  `run()` `:187-199`
- Description: decisions 0024/0037/0043 deliberately make structural surprises
  on discovered branches per-branch halts (`Outcome::Error`/`Warning`, run
  continues). But "isn't at a point this clone can safely build on" and
  `graft_point`'s "no shared history — has setup been run?" are `bail!`/`?`,
  propagated by `run()` as fatal.
- Trigger: dest developers create `hotfix` on dest; source has a different
  `hotfix`. `dest_ref_exists` → fetch → case 2's `graft_point` errors on no
  merge-base (or case 3 → `No` → bail). Every branch sorting after it is
  skipped every run. The message also misleads: dest→source never runs for a
  mirror-only branch, so "hasn't reflected its content yet" cannot apply.
- Remediation: convert these refusals for non-`config.branches` branches to
  per-branch `Outcome::Error` + `Ok(true)`; keep the fatal path for configured
  branches.

### F-06 — MEDIUM — README contradicts implemented conflict semantics

- Category: Documentation / operator expectations
- Confidence: High
- File: `README.md:323-324`; `src/commands/sync.rs:159-161, 736-741, 1865-1870`
- Description: README says "On a real conflict, it stops that branch's sync,
  leaves everything else unaffected". Both conflict paths `anyhow::bail!`,
  which `run()` propagates — a dest→source conflict on the first configured
  branch aborts the run before any source→dest mirroring happens for any
  branch. Decision 0007 says "fails the pipeline run loudly", so the code
  matches the decision and the README is wrong.
- Impact: an operator planning around the README will be surprised that one
  customer PR conflict freezes all mirroring.
- Remediation: fix the README, or (design change) make conflicts per-branch
  halts consistent with 0037/0043. Either is compatible with the
  operator-intervention principle; they must agree.

### F-07 — MEDIUM — Setup rollback after a partially applied checkout leaves index and working tree at the graft

- Category: Partial-failure consistency
- Confidence: High (behaviour), Medium (frequency)
- File: `src/commands/setup.rs:349-367`, `checkout_branch` `:709-713`, `:691-698`
- Description: `checkout_tree` succeeds, then `restore_control_files_exact` or
  `set_head` fails (I/O, permissions). The caller rolls back refs and control
  files, but the index already points at the graft tree and the working tree
  contains dest's files. Ref state says "unborn/original"; index/workdir say
  "dest tree". Non-destructive (safe checkout), but the aggregated error does
  not mention the residue and a re-run then trips checkout conflicts. Also
  `checkout_branch` uses `config.branches[0]`'s pre-run tip as the checkout
  baseline even when HEAD was on a different configured branch, which can
  leave `main` checked out with another branch's content as unstaged
  modifications.
- Remediation: on checkout-phase failure, check out the original HEAD tree (or
  reset the index) as part of recovery and report residue explicitly; use the
  real pre-run HEAD as the baseline.

### F-08 — MEDIUM — Cargo dependencies are outside Dependabot's scope; `cargo deny` never checks licenses

- Category: Supply chain / policy
- Confidence: High
- File: `.github/dependabot.yml`, `.github/workflows/ci.yml:104-109`, `deny.toml`
- Description: Dependabot only tracks `github-actions`; Cargo crates (including
  vendored libgit2 1.9.6 via `libgit2-sys 0.18.7`) are not bumped
  automatically, so a RustSec advisory surfaces only as a red `audit-check`
  with no PR. `cargo deny check bans sources` is explicit, so the absent
  `[licenses]` section is never evaluated. No known vulnerability is asserted
  (`cargo audit` was not runnable locally). Dependency shape is otherwise
  restrained: 10 direct runtime crates, 99 lock entries, no duplicate
  versions, git2 with `default = []` (no libssh2/openssl), no `build.rs`.
- Remediation: add a `cargo` ecosystem to Dependabot; add a `[licenses]`
  allowlist and run the full `cargo deny check`.

## 6. Low and Informational Findings

| ID | Severity | Finding | Where |
|---|---|---|---|
| F-09 | Low | Branch names reach stderr unescaped via `Display`. `git check-ref-format` only rejects bytes ≤ 0x20 and 0x7F; U+0080–U+009F (C1 controls incl. CSI) and U+2028/U+2029 are valid branch names. `git::escape_bytes` would handle C1; apply it (and consider Cf/Zl) at the `Reporter` boundary. `{branch:?}` sites in error messages are already safe. | `src/progress.rs:103-105, 201-212, 222-236, 255-285` |
| F-10 | Low | Directory-only exclude patterns (`vendor-secret/`) do not match submodule gitlinks — git maps gitlinks to `DT_DIR` for ignore matching; `filter_tree` passes `is_dir=false`. (Symlinks are *not* affected: git's own `vendor-secret/` does not ignore a symlink of that name, verified with `git check-ignore`.) Leak is metadata only (submodule oid) but contradicts decision 0011's "exact .gitignore syntax". Document or test the gitlink case separately. | `src/commands/sync.rs:1523-1527, 1557-1559` |
| F-11 | Low | `let Ok(cbase) = repo.merge_base(..) else { continue }` treats every libgit2 error (corrupt object, I/O) as "no shared history". Failure direction is safe; match only `ErrorCode::NotFound`. | `src/commands/sync.rs:1117, 2386` |
| F-12 | Low | Cleanup errors swallowed with `let _ =` in `start_source_to_dest`'s error paths (state-ref delete, `remove_dir`, `worktree remove`). Next start self-heals (ref overwritten with `force=true`), but a stale `/tmp` dir and `.git/worktrees/*` entry may remain unreported — inconsistent with setup's aggregated `RecoveryError`. | `src/commands/resolve.rs:433-455` |
| F-13 | Low | `list_source_branches` fails the whole run on any local branch with a non-UTF-8 name; operator-controlled, but a per-branch warning would match 0024's precedent. | `src/commands/sync.rs:231-259` |
| F-14 | Low | `sync`/`resolve` validate `GITPRISM_STATE_KEY` before repository discovery, so running in the wrong directory reports a key error (observed: `Error: GITPRISM_STATE_KEY must be set…` outside any repo). | `src/commands/sync.rs:99`, `src/commands/resolve.rs:79` |
| F-15 | Low | Resolution worktrees live under `std::env::temp_dir()`; tmp-aging cleaners can sweep a resolution left open for days (fails closed with "worktree not registered", losing operator work). Consider `<commondir>/gitprism-resolve/`. | `src/commands/resolve.rs:667-684` |
| F-16 | Info | `-c credential.interactive=true` is forced on every git invocation, overriding a deliberate operator setting; justified in a comment for GitLab Runner but undocumented in README. | `src/git.rs:65` |
| F-17 | Info | Misleading detached-HEAD message in setup ("already has commits and/or branches") — existing branches are allowed since 0023; the real condition is a detached HEAD. | `src/commands/setup.rs:134-136` |
| F-18 | Info | dest→source imports dest-authored paths unfiltered (by design). A customer can add net-new files under paths source excludes (e.g. CI config directories) — add/add conflicts protect existing files, but new files land in the private repo and may execute in its CI. Document as a trust consequence. | `src/commands/sync.rs:2773-2838` |
| F-19 | Info | No production `unsafe`; the six `unsafe { set_var/remove_var }` sites are tests guarded by `ENV_VAR_LOCK`. Holding the guard across `assert!` poisons the mutex on first failure and cascades. | `src/config.rs:325-374`, `src/commands/sync.rs:6170` |
| F-20 | Info | `ext::`/`fd::` transports verified blocked by git's default `protocol.allow`, so a policy-approved malicious URL cannot execute commands through that vector. Setting `GIT_PROTOCOL_FROM_USER=0` on child processes would make this independent of user config. | `src/git.rs:53-67` |

## 7. Git and Subprocess Security Assessment

- **Construction:** every subprocess is `Command::new("git")` + `.arg()`; no
  shell anywhere (grep-verified). Remote URL is validated (non-empty, no
  leading `-`, no control characters) and placed after `--`; branch names are
  validated with `git2::Branch::name_is_valid` and wrapped as
  `refs/heads/<name>`; OIDs are program-generated; `--force-with-lease`
  precedes `--`. No option-injection path was found.
- **Runner:** stdin null, both pipes drained concurrently with per-stream and
  total caps, 300 s deadline (1–3600 s override), kill + reap on every failure,
  drain grace after exit. Exit codes are matched explicitly per command;
  everything else is a redacted, escaped, 8 KiB-framed diagnostic error.
- **Ambiguity:** none — refs are always fully qualified; paths never share a
  command line with revisions.
- **Push safety:** dest→source and both resolve paths are unconditionally
  `FastForwardOnly`; `ForceMirrorOnly` requires `!round_tripped` and a lease on
  the just-fetched tip. The remaining risk is the *decision* to force (F-01),
  not the mechanism.
- **Environment:** `GITPRISM_STATE_KEY`, `GITPRISM_SOURCE_URL`,
  `GITPRISM_DEST_URL` scrubbed; `GIT_TERMINAL_PROMPT=0`, `GIT_EDITOR=true`,
  committer identity injected. `PATH` resolution of `git` is inherent to a CLI.
- **Edge cases:** bare repo, detached HEAD (setup), unborn HEAD, missing dest
  ref, deleted round-tripped ref, shallow clone and non-UTF-8 names all fail
  closed with clear messages. Linked worktrees share the lock. Submodule
  gitlinks are preserved, not traversed (but see F-10). Corrupt objects
  propagate as errors except at the two `merge_base` sites (F-11).

## 8. Filesystem Safety Assessment

- Filtering operates on Git trees, never the working directory; no
  repository-derived path is joined onto a filesystem path except tree names
  via `prefix.join(name)` for pattern matching only.
- Control files are read through an open handle with regular-file, symlink,
  dev/ino and (Unix) hard-link checks; written via remove + `create_new` with
  the mode applied through the handle. TOCTOU on these paths requires a hostile
  local process racing the operator — outside the trust boundary and documented.
- Checkouts are libgit2 SAFE (non-forced); untracked collisions are detected
  explicitly (`reject_colliding_untracked_paths`); local refs move only by CAS.
- The source→dest resolution worktree path is HMAC-bound, must equal its own
  `canonicalize`, be a real directory with a regular `.git` pointer that
  round-trips to a registered `worktrees/*` entry of the same common dir.
  `git worktree remove --force` only ever targets that validated path.
  Reservation via `create_dir` is atomic.
- No `remove_dir_all` or recursive deletion exists. No path derived from Git
  output is written to.
- Residual: setup's checkout phase is not transactional (F-07); resolve
  cleanup failures are silent (F-12); tmp-dir aging (F-15); an explicit
  `--config ../x` is operator intent, not traversal.

## 9. Testing Gaps

240 tests, all inline `#[cfg(test)]` modules; no `#[ignore]`. Strong coverage:
hostile/forged/duplicate markers, credential redaction, control/invalid-byte
escaping, push races (including stale lease), CAS ref moves, dirty/untracked
worktrees, policy pinning on replayed control files, Unix byte paths, rollback
aggregation. Most important gaps, in priority order:

1. Regression tests for F-01, F-02 and F-04 (reproductions exist and can be
   lifted).
2. No test chains `setup::run` into `sync::run`; sync and resolve fabricate the
   graft via a duplicated `source_grafted_onto` helper.
3. Zero tests for the `#[cfg(windows)]` branches (`path_from_git_bytes`,
   `subprocess_path`); every raw-byte test is `cfg(unix)`.
4. Seven of twelve `limits.rs` constants are never exercised
   (`MAX_STATE_FILE_BYTES`, `MAX_SOURCE_BRANCHES`, `MAX_COLLISION_PATHS`,
   `MAX_PENDING_COMMITS`, `MAX_MARKER_SCAN_COMMITS`, `MAX_CONFLICT_RECORDS`,
   `MAX_CONFLICT_PATH_BYTES`).
5. Bare-repo-as-cwd, detached HEAD, cwd inside a linked worktree, submodule
   gitlinks, nested repositories and an empty dest repo through `run` are not
   covered.
6. Host coupling: the suite reads the developer's global git config (no
   `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM` pinning);
   `ensure_merge_tree_supported_accepts_the_git_on_this_machine` asserts the
   host's git version; the 50 ms timeout test is tight on loaded CI.
7. CLI: 3 clap tests, no dispatch test, no invalid-combination cases.
8. Fixture duplication: `bare_repo_with_a_commit_on`, `source_grafted_onto`,
   `add_commit`, `write_config` exist in two or three copies with divergent
   details (e.g. the `core.autocrlf` workaround).

## 10. Architecture Assessment

Strengths:

- The decision log is the source of truth and is cited at the exact code sites
  it governs; intent never has to be inferred.
- A single subprocess runner with uniform limits, deadlines, diagnostics and
  environment scrubbing.
- Both sync directions and both resolve directions share one merge primitive
  (`git merge-tree`) and one commit builder per direction, so conflict
  semantics cannot diverge.
- Separate external trust roots for policy (digest) and state (HMAC);
  repository content can never raise its own limits.
- `run(cwd, config_path)` signatures keep every command end-to-end testable.

Weaknesses:

- `sync.rs` mixes orchestration, marker scanning, anchor search, filtering,
  commit construction, preflight and CAS advancement in one 2,893-line
  production section plus ~7,800 lines of tests; the main loop has 9 parameters
  and two duplicated `DestAnchor` match blocks.
- Three near-identical first-parent marker revwalks;
  `newest_dest_marker_opt_for_branch` is a pure alias; `#[cfg(test)]` shims live
  in the production section.
- Failure semantics are inconsistent across decisions (per-branch halt vs
  whole-run abort) and the README disagrees with both (F-05, F-06).
- Policy enforcement (`find_control_file_policy_mismatch`) is private to
  `sync.rs`, which is how `resolve` ended up without it (F-03).
- Test fixtures have no shared module.

A rewrite is not warranted. Extracting `marker_scan` (the three walks),
`anchor` (0043), `local_advance` (preflight + CAS) and a shared `policy_check`
from `sync.rs`, plus a `testutil` module, would address most of the above
without changing behaviour.

## 11. Recommended Action Plan

### P0 — Immediate

| Action | Impact | Effort |
|---|---|---|
| F-01: stop treating a missing boundary object on a non-shallow clone as a positive rewrite; refuse or require positive evidence. Amend decision 0039; add the regression test. | High | Small–Medium |
| F-02: never create a dest ref at a sibling's commit without a branch-scoped marker (empty marker commit or skip); add the second-run regression test. | High | Medium |
| F-03: run the 0037 policy pre-pass in `resolve --direction source-to-dest` (start and finish). | High | Small |

### P1 — Before Release

| Action | Impact | Effort |
|---|---|---|
| F-04: make loop prevention and step 5 recognise `DestToSource` markers on ancestor branches so imports are not re-projected. | Medium | Medium |
| F-05: make discovered-branch refusals per-branch halts (consistent with 0024/0037/0043). F-06: correct the README only — real conflicts abort the run by decision 0007, which stays as is unless the owner explicitly reopens it. The two semantics are intentionally different. | Medium | Medium |
| F-07: recover index/working tree on setup checkout-phase failure; use the real pre-run HEAD as the checkout baseline. | Medium | Small |
| F-08: Dependabot `cargo` ecosystem; `[licenses]` in `deny.toml`; full `cargo deny check`. | Medium | Small |
| Add a setup→sync integration test and tests for bare cwd, detached HEAD, linked-worktree cwd and gitlinks. | Medium | Medium |
| Run a real source/dest pilot and the first published artifact release (first successful `release.yml` run; tags v0.1.0–v0.1.5 already exist). Still open from the 2026-08-20 review. | High | Medium |

### P2 — Near Term

| Action | Impact | Effort |
|---|---|---|
| Split `sync.rs` into marker-scan, anchor, local-advance and policy-check modules; move inline tests to `tests/` with a shared fixture module. | Medium | Large |
| F-09: escape branch names (C1/Cf/Zl) at the `Reporter` boundary. | Low–Medium | Small |
| F-10: treat gitlinks as directories for dir-only exclude patterns (or document the gap); symlinks are unaffected. | Low | Small |
| Pin `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_NOSYSTEM` in test fixtures; add Windows-path unit tests; cover the seven untested limits. | Medium | Medium |

### P3 — Opportunistic

| Action | Impact | Effort |
|---|---|---|
| F-11, F-12, F-13, F-14, F-17: narrow error matching, aggregate resolve cleanup failures, per-branch warning for odd local names, discover the repo before key validation, fix the detached-HEAD message. | Low | Small |
| F-15: move resolution worktrees under the common dir; F-16/F-18/F-20: document. | Low | Small |

## 12. Top 10 Recommendations

1. Fix F-01 — a stale clone must never force-push dest backwards; a missing
   object is not proof of a rewrite.
2. Fix F-02 — never leave a dest ref gitprism cannot recognise for its own
   branch; make the refusal per-branch.
3. Fix F-03 — apply the 0037 policy pre-pass in
   `resolve --direction source-to-dest`.
4. Fix F-04 — stop re-projecting dest→source imports on branches forked after
   the import.
5. Unify failure semantics for discovered branches and conflicts, then make
   README and decision 0007 match the code.
6. Lift the three reproduction tests into the suite and add a setup→sync
   integration test.
7. Extract marker scanning, anchor search, local-advance and policy checking
   out of `sync.rs`; share test fixtures.
8. Add Dependabot for Cargo and enable license checking in `cargo deny`.
9. Isolate tests from host git config; add Windows-path and limit-boundary
   tests.
10. Run the real-pair pilot and first published artifact release before any unattended
    deployment.
