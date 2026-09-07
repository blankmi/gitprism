# Plan OPS-001 — resolution worktree lives inside the repository's common dir

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, OPS-001 |
| Severity / priority | LOW / P2 |
| Effort | Small |
| Decision required | Yes — addendum to decision 0033 (it fixes "a canonical temporary-directory root") |
| Depends on | — |
| Status | Proposed |

## Problem

`resolution_worktree_path` (`src/commands/resolve.rs:738-754`) places the
source→dest resolution worktree under `std::env::temp_dir()` with a predictable
name `gitprism-resolve-<pid>-<nanos>`. The worktree must survive between
`resolve <branch>` and `resolve <branch> --continue`, which can be hours apart.
Temp cleaners (systemd-tmpfiles, macOS periodic cleanup, CI runner resets)
delete it in between. A same-host user can pre-create the path to make start
fail; `create_dir` prevents hijack, so this is availability only.

## Verified option

`git worktree add <repo>/.git/gitprism/resolve/<name>` succeeds on git 2.50.1
(checked in a scratch repository during planning). The common dir is owned by
the same user, is never swept by temp cleaners, and is where
`OperationLock` (decision 0028) already lives.

## Steps

### Step 1 — addendum to 0033

**Files.** `design/decisions/0033-authenticated-resolution-worktree-and-safe-file-recovery.md`,
`index.md`, `log.md`.

**Change.** The worktree root becomes `<common_dir>/gitprism/resolve/`. Record:
lifetime (from start until `--continue` finishes or the operator aborts);
uniqueness (`create_dir` on `<pid>-<nanos>`, unchanged); that the path is still
authenticated by the state ref (`Resolve-Worktree-Path`), so moving the root
changes nothing in the trust model; and that a stale directory left by a killed
process is reported, not deleted (AGENTS.md: operator over automation).

### Step 2 — failing test

**Files.** `src/commands/resolve.rs` tests.

**Test first.** After `start_source_to_dest`, the recorded worktree path starts
with `repo.commondir().join("gitprism/resolve")`; `git worktree list` (via
`git2::Repository::worktrees`) names it; after `--continue` completes it is
removed. Second test: a pre-existing directory at the reserved name makes the
next offset be chosen (existing behaviour, now inside the common dir).

### Step 3 — change the root

**Files.** `src/commands/resolve.rs:738-754`.

**Change.** Take `repo: &Repository`, use `fs::canonicalize(repo.commondir())`,
`create_dir_all` the `gitprism/resolve` parent (permissions inherited from
`.git`), keep the per-attempt `create_dir` reservation. Update the operator
messages that print the path.

### Step 4 — check the recovery paths

`git::worktree_remove` and the "worktree registered to another repository"
check must work with a path inside the common dir; add that path to the
test-gap plan's worktree tests (TEST-GAPS step B).

## Rejected alternatives

* Random suffix in `temp_dir()`: still swept by cleaners.
* Sibling of the source root (`../<name>`): pollutes the operator's parent
  directory and may not be writable.
* Inside the source working tree: shows up in `git status`.

## Verification

`cargo test resolve`; a manual `gitprism resolve` start / `--continue` on
macOS and Linux; fmt and clippy clean.
