# Repository review — 2026-09-02

Reviewed tree: `main` at `7c5fa19` (version 0.1.8). Method: manual reading of
every production module (three of them, `resolve.rs`, `setup.rs`, and
`mapping_index.rs`, via a delegated pass), executed release gates, and one
executed reproduction in an isolated copy of the repository. No project files
were modified by the review itself.

Gates on this tree:

* `cargo fmt --all -- --check` — clean
* `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
* `cargo test --workspace --all-features` — 338 passed
* `cargo audit` / `cargo deny` — not installed in the review environment; CI
  runs both with pinned actions

## 1. Executive summary

gitprism is a well-engineered, deliberately conservative Git synchronization
CLI. The Git subprocess boundary, the HMAC-authenticated marker model, the
fail-closed resource limits, the operation lock, and the real-repository test
suite are all above the bar for a tool of this size. There is no production
`unsafe`, no shell, and no new exploitable security defect.

One HIGH correctness defect was confirmed by execution. A branch cut from
`main` after `setup`, mirrored to dest, and later added to `config.branches`
cannot be round-tripped: dest→source resumes from `main`'s setup graft, replays
`main`'s own mirrored commits, and hard-stops on a phantom conflict before the
customer's commit is imported. That is the release-branch workflow the README
is built around, and `setup` refuses such a branch too, so there is no
supported path.

**Verdict: Fix first.** Suitable for a deployment whose round-tripped branches
are all fixed at `setup` time and whose branch count is modest. Not ready for
general production until CODE-001 is fixed and the per-branch network cost is
addressed.

| Area | Verdict | Score | Rationale |
| --- | --- | ---: | --- |
| Architecture | Fix first | 7 | Boundaries and decision record are strong; dest→source's resume model ignores the mapping index the other direction relies on. |
| Code quality | Fix first | 7 | Careful error context throughout; one confirmed logic defect and one duplicated replay loop that has already diverged. |
| Rust idioms | Ready | 8 | Idiomatic ownership, enums for intent, zero production unwrap in the command modules, clippy clean. |
| Security | Ready | 8 | No injection, no shell, scrubbed child environment, byte-safe output; the documented Git hooks boundary stands. |
| Reliability | Fix first | 6 | Fails closed everywhere, but CODE-001 blocks a core workflow and network cost grows with every branch. |
| Testing | Fix first | 7 | Excellent real-Git coverage; the pin and key checks are compiled out under test, and several resolve paths are untested. |
| Maintainability | Fix first | 7 | Exceptional documentation; three 2,000+ line modules and unrecorded "F-A/F-B/F-C" behaviours. |
| Production readiness | Fix first | 6 | Ready for a constrained deployment; general readiness needs P0 and P1 below. |

## 2. System overview

Single-binary Rust CLI (`setup`, `sync`, `resolve`, `policy-hash`) run by a CI
job or an operator inside a checkout of the private *source* repository. Local
object, tree, ref and revwalk work goes through `git2`; every network, merge
and working-tree operation goes through a real `git` subprocess centralized in
`src/git.rs`. There is no server, database or state file. Sync state is carried
in HMAC-SHA256 authenticated commit trailers keyed by `GITPRISM_STATE_KEY`;
each run rebuilds a bounded in-memory mapping index from them.

```text
CLI (clap)  ->  commands/{setup,sync,resolve,policy_hash}
                    |
                    |- policy.rs   pinned .gitprism.toml + .gitprismignore (SHA-256 pin)
                    |- marker.rs   HMAC state blocks on generated commits
                    |- lock.rs     per common-dir operation lock
                    |- limits.rs   static fail-closed budgets
                    |- git2        local objects, refs, revwalks, checkout
                    '- git.rs  --> git fetch / push / ls-remote / merge-tree / cherry-pick
                                   (stdin null, timeout, bounded capture, scrubbed env)
```

**Assumptions where the docs are silent.** "Production" means unattended CI
runs against a private GitLab-style source and a customer-facing dest such as
Azure DevOps. Attackers are: anyone who can push to dest (untrusted refs,
commit messages, tree contents), anyone who can push to source branches
(repository-controlled control files, guarded by the pin), and network peers.
The invoking user's environment, the `git` binary, Git configuration and hooks
are trusted, per decision 0029. Platforms are the three CI targets; MSRV 1.89.

**Already known, not re-reported.** The 2026-08-31 review's hooks trust
boundary (H-01), Windows opened-file identity gap (L-01), unsigned releases
(I-01) and mapping-horizon trade-off (I-02). Its M-01 and M-02 are fixed in
this tree (verified in `list_source_branches` and `parse_ls_remote_heads`).

## 3. Critical and high findings

### CODE-001 — A mirror-only branch later added to `config.branches` replays its parent's mirrored history and hard-stops on a phantom conflict

* **Category:** correctness
* **Severity:** HIGH
* **Confidence:** High
* **Verification:** Executed
* **Files:** `src/commands/sync/marker_scan.rs:37-79` (`scan_for_dest_marker`),
  `src/commands/sync/mod.rs:1642-1676` (`pending_dest_commits`),
  `src/marker.rs:352` (Setup markers exempt from the branch check),
  `src/commands/setup.rs:229-241`

dest→source finds its resume boundary by walking the source branch's
first-parent history for a `Setup` marker (any branch) or a `DestToSource`
marker for this exact branch. A branch cut from `main` after setup carries only
`main`'s inherited `Setup` graft, so the boundary is dest's original tip from
setup time. Every dest commit since then on the branch's first-parent line
becomes "pending", including the commits gitprism itself mirrored from source
`main`. Loop prevention in `pending_dest_commits` is branch-scoped, so those
`Gitprism-Branch: main` markers do not exempt them. Each is 3-way merged
against the current source tree; any line changed twice since conflicts.

**Evidence.** Reproduced end-to-end in a copy of the repo with a new test:
dest `main` at v1; source commits s1 (v2) and s2 (v3) synced; `release` cut
from s2 and mirrored; customer commit on dest `release`; sync with
`branches = ["main", "release"]`. Result:

```text
release: error (dest -> source) — hit a real conflict at dest commit d0eb3f43… in ["shared.txt"] on branch "release"
```

That dest commit is the mirror of s1 carrying `Gitprism-Branch: main`. Zero
commits reached source's `release`. `setup` refuses the branch instead
(inherited marker), so no supported path exists.

**Impact.** The README's central workflow (release branches, created over
time, fed back from dest) fails on first use. With a long-lived `main`,
hundreds or thousands of phantom pending commits result, each a potential
spurious `gitprism resolve`, or the run fails outright at the 10,000
pending-commit limit. Fails closed; no data loss.

**Remediation.** Derive the dest→source boundary from the branch's nearest
authenticated `SourceToDest` mapping on its first-parent history (the
decision-0046 index already has it) whenever that mapping is newer than the
Setup/DestToSource marker; the boundary is then the branch-scoped marker commit
gitprism created on dest. Record it as a decision, add the reproduction as a
regression test, and until then document that `config.branches` must be
complete at `setup` time.

## 4. Medium findings

### CODE-002 — `resolve`'s source→dest replay uses branch-scoped loop prevention that `sync` abandoned

* **Category:** correctness
* **Severity:** MEDIUM
* **Confidence:** High
* **Verification:** Read-only
* **Files:** `src/commands/resolve.rs:329-339` vs `src/commands/sync/mod.rs:1050-1062`

Sync's `loop_prevented` uses `marker::verify_self` (decision 0043 addendum,
review finding F-04). Resolve's copy still calls `marker::verify` with the
branch, so a `DestToSource` marker inherited from a sibling is skipped by sync
but replayed by resolve.

**Scenario.** Branch B forked after a dest-native commit was reflected into
sibling A; `gitprism resolve B --direction source-to-dest` pushes that commit
again as part of the "clean prefix", or picks it as the conflict to resolve.

**Remediation.** Call the shared `loop_prevented`; see ARCH-001.

### ARCH-001 — The source→dest replay loop exists twice and has diverged twice

* **Category:** maintainability
* **Severity:** MEDIUM
* **Confidence:** High
* **Verification:** Read-only
* **Files:** `src/commands/resolve.rs:325-364`, `src/commands/sync/mod.rs:1077-1139`

Resolve hand-copies `build_pending_dest_tip`. The decision-0037 policy pre-pass
had to be back-ported once (finding F-03); CODE-002 is the second divergence.
The tool's invariant that "resolve and sync must never disagree about which
commit is next" is enforced only by discipline.

**Remediation.** One shared replay function returning the built prefix plus
the conflicting `(source_oid, parent, paths)`, consumed by both.

### TEST-001 — Policy pin and state key checks are compiled out under `cargo test`; the built binary is never exercised

* **Category:** test architecture
* **Severity:** MEDIUM
* **Confidence:** High
* **Verification:** Read-only
* **Files:** `src/policy.rs:282-291`, `src/marker.rs:66-80`; no `tests/` directory

`verify_expected_digest` sets `expected = actual` under `#[cfg(test)]`, and
`load_key` uses a fixed key. The env-reading production branches are dead code
in every test build. Only pure helpers (`verify_digest`, `parse_key`) are
unit-tested; nothing asserts that a wrong `GITPRISM_POLICY_SHA256` or a missing
`GITPRISM_STATE_KEY` refuses before any fetch or ref mutation.

**Scenario.** Someone reorders `marker::load_key()` below the fetch loop or
renames an env var; all 338 tests stay green.

**Remediation.** Resolve digest and key once at the command entry into a small
credentials value and pass it down, so tests supply explicit values; add one
integration test per command asserting "wrong pin ⇒ error, no fetch, no ref
created". Optionally add a `tests/` smoke test that runs the built binary.

### PERF-001 — One network fetch per dest branch plus one per source branch, every run

* **Category:** scalability / operations
* **Severity:** MEDIUM
* **Confidence:** High
* **Verification:** Read-only
* **Files:** `src/commands/sync/anchor.rs:432-446, 345`;
  `src/commands/sync/mod.rs:536, 1425-1437`

Reconstruction fetches every advertised dest head individually; source→dest
then fetches each branch again (kept deliberately for the lease); dest→source
adds an `ls-remote` and a fetch per configured branch. Each is a separate
transport handshake with its own 300 s deadline and no run-wide deadline.
Decision 0046 says heads are "fetched once"; the code does so per head.

**Scenario.** A dest with a few hundred customer branches makes a no-op sync
take many minutes over SSH; at the 4,096-branch limit roughly 8,000 sequential
round trips can exceed a CI job timeout.

**Remediation.** Fetch all listed heads in one subprocess
(`git fetch <url> refs/heads/*:refs/gitprism/dest/*` or a multi-refspec fetch of
the listed names) and read tips from those refs; keep the per-branch lease
fetch. Consider a run-wide deadline.

### DOC-001 — Behaviours cited in code as `decisions/0046, F-A/F-B/F-C` are not recorded in `design/`

* **Category:** documentation
* **Severity:** MEDIUM
* **Confidence:** High
* **Verification:** Read-only
* **Files:** `src/commands/sync/mapping_index.rs:215, 240, 342, 410, 590, 689`;
  `anchor.rs:396, 460`; `mod.rs:833, 854, 1012`

`grep 'F-A\b|F-B\b|F-C\b' design/ docs/` returns nothing, and the labels
collide with decision 0046's own "Finding A/B/C", which mean different things.
The self-exclusion canonicalization rule (F-C, `exclude_branch`) changes which
dest history a force-rebuild lands on and exists only in code comments.
AGENTS.md makes `design/decisions/` the source of truth.

**Remediation.** Add an addendum to 0046 defining the three, or rename the code
references to the addendum's letters.

## 5. Low and informational findings

### CODE-003 — Committer name and email accept line breaks

* **Severity:** LOW · **Confidence:** High · **Verification:** Executed
* **File:** `src/config.rs:89-94`

Only non-emptiness is checked; `git2::Signature::now("a\nb", …)` returns `Ok`.
A pinned config with a newline in `[committer]` writes a malformed commit
header that `receive.fsckObjects` rejects at push time, after local mutation.
Reject control characters as the URL fields already do.

### CODE-004 — `exclude_branch` can discard a rewritten branch's own still-valid anchor

* **Severity:** LOW · **Confidence:** High · **Verification:** Read-only
* **File:** `src/commands/sync/mapping_index.rs:226-235`

When the nearest mapped ancestor has the branch's own projection and a
sibling's incomparable one, the own projection is dropped and the force-rebuild
rewinds dest further than the amend required. Safe under decision 0038,
acknowledged in the comment, but unrecorded (see DOC-001); prefer the own
projection when it is comparable.

### CODE-005 — Setup rollback can leave HEAD on the scratch ref

* **Severity:** LOW · **Confidence:** Medium · **Verification:** Read-only
* **File:** `src/commands/setup.rs:626, 663-669`

HEAD is moved to `gitprism-setup-rollback-scratch` unconditionally but restored
only when the original target was readable. Detached HEAD is rejected earlier,
so the residual case is an unreadable HEAD file. Skip the scratch move when
there is nothing to restore.

### CODE-006 — dest→source divergence message blames the wrong side

* **Severity:** LOW · **Confidence:** High · **Verification:** Read-only
* **File:** `src/commands/sync/mod.rs:115-119`, called at `:1579`

Called with `"source"`, the message prints "dest branch … and source have
diverged". For dest→source the lost race is against source's own remote, not
dest. Parameterize the sentence, not only the target word.

### OPS-001 — Resolution worktree lives under the system temp directory across two invocations

* **Severity:** LOW · **Confidence:** Medium · **Verification:** Read-only
* **File:** `src/commands/resolve.rs:739-747`

Temp cleaners can delete a half-resolved worktree between start and
`--continue`; a same-host user can pre-create the predictable path so start
fails (`create_dir` prevents hijack, so DoS only). Place it under the common
directory or document the location and lifetime.

### PERF-002 — `filter_tree` re-filters identical subtrees for every pending commit

* **Severity:** LOW · **Confidence:** High · **Verification:** Read-only
* **File:** `src/commands/sync/filter.rs:28-108`

The full tree is walked and rewritten twice per pending commit with no
subtree-oid memo. A backfill of thousands of commits on a large monorepo
becomes O(commits × tree size). A per-run `HashMap<Oid, Oid>` keyed by input
subtree oid makes consecutive commits nearly free.

### PERF-003 — Under a tainted index, unmapped branches are re-walked to the horizon every scheduling round

* **Severity:** LOW · **Confidence:** High · **Verification:** Read-only
* **Files:** `src/commands/sync/mod.rs:130-143, 318-324`; `mapping_index.rs:370-406`

Only `None` distances are memoized; a taint-caused `Contradictory` after a
complete walk is stable for the run but recomputed per round. Memoize it, which
also needs the cause enum below.

### Informational

* **DOC-002.** Decision 0022 (interactive setup wizard) is listed in
  `design/decisions/index.md` as settled behaviour; the file is
  `status: draft` and `design/log.md` says "no code written". No wizard code
  exists.
* **CODE-007.** `MappingLookup::Contradictory(String)` conflates four causes
  (genuine contradiction, global taint, own-walk horizon, missing dest object);
  callers and tests distinguish by substring.
* **OPS-002.** `truncation_message` grows with every truncated head and is
  repeated in every halted branch's line; cap and count.
* **OPS-003.** `kill_and_reap` kills `git` but not its children (ssh,
  credential helpers); capture threads then block until those exit. Harmless
  for a short-lived CLI.
* **CODE-008.** Dead `.stdout(Stdio::null())` in `remote_ref_exists`
  (`git.rs:553`); `hex_encode`/`hex_decode` duplicated between `marker.rs` and
  `resolve.rs`; `resolve.rs` imports `setup::with_recovery_failures`, a generic
  helper living in a sibling command; `Resolve-Dest-Ref-Existed` is always
  `true`.
* **CODE-009.** Setup on a pre-existing branch strictly ahead of dest still
  writes a two-parent commit whose second parent is an ancestor; git itself
  would say "already up to date". Consistent with decision 0023, renders oddly
  in graph views.

## 6. Security assessment

**Attack surface.** Remote ref advertisements, fetched commits and trees,
commit messages, control files in source's tree, CLI arguments, environment
variables, and the local filesystem around the checkout.

**Trust boundaries.** `src/git.rs` is the single subprocess boundary:
arguments via `Command::arg` with `--` separators, validated URLs and ref
names, closed stdin, `GIT_TERMINAL_PROMPT=0`, `GIT_PROTOCOL_FROM_USER=0`,
bounded concurrent capture, a deadline, and `GITPRISM_STATE_KEY`/URL variables
removed from the child environment. Repository-controlled bytes reach the
terminal only through `escape_bytes`. Control files are digest-pinned before
parsing and re-materialized byte-exact after checkout with no-follow,
create-new writes. Marker state is HMAC-SHA256 over a length-prefixed payload
binding direction, branch, counterpart, parents, tree, both signatures and
body, verified constant-time. This is sound design and sound implementation.

**Major threats.** Local code execution via Git hooks and configuration
remains the accepted, documented boundary (decision 0029). Denial of service
from hostile repositories is bounded by static limits; the residual
availability risk is PERF-001's network cost, which a dest with many branches
triggers without malice.

**Dependencies.** 99 crates, all from crates.io, no git or path dependencies,
no `build.rs` in the project, `git2` with default features (which in 0.21
means no OpenSSL or libssh2, consistent with the hybrid design). `cargo audit`
and `cargo deny` were not installed and could not be run; CI runs both with
pinned actions. No vulnerability claim is made.

**Unsafe code.** None in production. Six `unsafe` blocks in tests:
`std::env::set_var/remove_var` under a shared mutex and one `libc::umask` in a
re-exec'd child process, each justified.

**Authentication, authorization, secrets, cryptography.** Delegated to Git's
own mechanisms by design. No hard-coded secrets; the state key is a private
newtype that never reaches logs or children. HMAC-SHA256 and SHA-256 via
RustCrypto crates; no custom cryptography.

## 7. Architecture assessment

**Strongest decisions.** Real graft instead of hand-rolled cross-repo ancestry
(0006). One merge primitive, `git merge-tree --write-tree`, for both directions
(0016) so sync and resolve cannot disagree about conflicts. Authenticated
markers instead of a state database (0025). Force semantics decided by branch
authority with an explicit `PushMode` at every call site and a
compare-and-swap lease (0038, 0040). The decision record itself: 47 numbered
decisions with context and consequences make the design reviewable in a way
most tools are not.

**Weakest points.** The two directions model "what has already crossed"
differently. Source→dest anchors on the decision-0046 mapping index;
dest→source still anchors on a Setup or branch-scoped DestToSource marker scan
that predates the index. CODE-001 is the direct consequence. Resolve
re-implements sync's replay (ARCH-001). Three modules exceed 2,000 lines; the
2026-08-28 split of `sync.rs` was the right move, and `resolve.rs` and
`setup.rs` are the next candidates once a concrete driver appears. Roughly half
of every large module is tests living in the same file, which inflates the
numbers but also slows navigation.

## 8. Testing gaps

* A mirror-only branch later added to `config.branches` (CODE-001); any
  dest→source test with two or more configured branches.
* Real policy pin and state key enforcement, per command (TEST-001).
* Resolve: "already in progress" in both directions, `--continue` with
  unresolved conflicts, "source branch moved", tampered state-ref MAC,
  worktree registered to another repository, mirror-only branch with no dest
  ref, clean-prefix push losing the race, dest→source finish push rejected,
  merge-commit pick with `-m 1`.
* Committer identity with control characters (CODE-003).
* Fetch-phase failure on branch N leaving branches 1..N-1 untouched in setup
  (only the commit-phase variant is tested).
* Dest-side mapping-entry truncation; `exclude_branch` interacting with alias
  canonicalization.
* Any test that runs the built binary as shipped.

## 9. Technical debt

**Immediate.** Duplicated replay loop (ARCH-001, CODE-002). Unrecorded
F-A/F-B/F-C behaviours (DOC-001). Test-only compile-time forks in security
checks (TEST-001).

**Medium-term.** Per-branch network model (PERF-001). Stringly-typed
`MappingLookup::Contradictory` (CODE-007). 2,000+ line `resolve.rs` and
`setup.rs`.

**Optional.** Dead `stdout(null)`, duplicated hex helpers, cross-command
helper import, vestigial `Resolve-Dest-Ref-Existed` field, index entry for
decision 0022.

## 10. Recommended action plan

| Priority | Action | Impact | Effort | Findings |
| --- | --- | --- | --- | --- |
| P0 | Decide and implement a dest→source boundary that uses the branch's own authenticated SourceToDest mapping; add the reproduction as a regression test; until shipped, document that `config.branches` must be complete at setup. | High | Medium | CODE-001 |
| P1 | Extract one shared source→dest replay function used by sync and resolve; switch resolve to `loop_prevented`. | High | Small | ARCH-001, CODE-002 |
| P1 | Fetch all dest heads in one subprocess during reconstruction; consider a run-wide deadline. | High | Medium | PERF-001 |
| P1 | Inject pin and key at the command entry; add per-command "wrong pin refuses before mutation" tests. | Medium | Medium | TEST-001 |
| P1 | Record F-A/F-B/F-C in a 0046 addendum, including the `exclude_branch` trade-off. | Medium | Small | DOC-001, CODE-004 |
| P2 | Add the missing resolve state-transition tests. | Medium | Medium | TEST gaps |
| P2 | Reject control characters in committer name and email. | Low | Small | CODE-003 |
| P2 | Memoize filtered subtrees per run in `filter_tree`. | Medium | Small | PERF-002 |
| P2 | Replace `Contradictory(String)` with a cause enum; memoize stable taint results. | Low | Medium | CODE-007, PERF-003 |
| P2 | Move the resolution worktree under the common directory, or document its lifetime. | Low | Small | OPS-001 |
| P3 | Fix the dest→source divergence wording; guard the setup rollback HEAD move; cap the truncation message; remove dead and duplicated helpers; mark decision 0022 as draft in the index. | Low | Small | CODE-005, CODE-006, CODE-008, OPS-002, DOC-002 |

## 11. Top recommendations

1. Fix the dest→source resume boundary for branches created after setup (P0, CODE-001).
2. Unify the replay loop between sync and resolve (P1, ARCH-001, CODE-002).
3. Fetch dest heads in one subprocess (P1, PERF-001).
4. Make the pin and key checks testable and tested (P1, TEST-001).
5. Record the F-A/F-B/F-C behaviours in the design bundle (P1, DOC-001).
6. Close the resolve state-machine test gaps (P2).
7. Memoize subtree filtering (P2, PERF-002).
8. Type the mapping-lookup causes (P2, CODE-007).
9. Validate committer identity (P2, CODE-003).
10. Keep the current subprocess, limit, lock and marker protections exactly as they are.

## 12. Not reviewed

* `cargo audit` and `cargo deny`: not installed in the review environment;
  dependency vulnerability scanning relies on CI.
* Windows and Linux behaviour: all execution was on macOS; the CI matrix
  covers the other two.
* Test bodies in `src/commands/sync/tests/source_to_dest.rs` and `anchor.rs`
  (6,300 lines) were sampled for fixture conventions, not read exhaustively;
  the named gaps come from grep-based coverage checks.
* The GitLab pipeline playbook and release playbook were not validated against
  a live pipeline.
* Third-party crate source was not audited.

## Appendix — CODE-001 reproduction

The test below was appended to `src/commands/sync/tests/dest_to_source.rs` in
an isolated copy of the repository and fails on this tree with the phantom
conflict quoted in section 3. It is reproduced here so it can be adopted as the
regression test once the fix lands.

```rust
#[test]
fn mirror_only_branch_later_added_to_config_branches_only_reflects_dest_native_commits() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    // Two source commits on main touching the same line.
    add_commit_with_message(&source_repo, "main", &[("shared.txt", "v2\n")], "s1");
    let s2 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v3\n")], "s2");

    let config_main_only = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config_main_only.path()).expect("first sync");

    // Cut a release branch from main AFTER setup; mirror it (mirror-only).
    source_repo
        .branch("release", &source_repo.find_commit(s2).unwrap(), false)
        .unwrap();
    run(source_dir.path(), config_main_only.path()).expect("second sync mirrors release");
    let dest_release_tip = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Customer merges a PR into dest's release.
    add_independent_dest_commit_on(
        &dest_repo,
        "release",
        dest_release_tip,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );

    // Seed source's remote with release, then promote release to round-tripped.
    git::push(
        source_repo.workdir().unwrap(),
        &source_remote.path().display().to_string(),
        s2,
        "release",
        PushMode::FastForwardOnly,
    )
    .unwrap();
    let config_both = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main", "release"],
    );
    run(source_dir.path(), config_both.path())
        .expect("promoting a mirrored branch must not replay main's own mirrored commits");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let release_tip = source_remote_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut revwalk = source_remote_repo.revwalk().unwrap();
    revwalk.push(release_tip.id()).unwrap();
    revwalk.hide(s2).unwrap();
    assert_eq!(
        revwalk.count(),
        1,
        "only the customer's commit should be reflected into source's release"
    );
}
```

## Errata (2026-09-02, after plan review)

Two findings were corrected while drafting the implementation plans in
`docs/plans/2026-09-02/`; the text above is left as written.

* **OPS-003 is withdrawn.** The capture threads' `JoinHandle`s are discarded
  (`src/git.rs:436-464`), and on deadline expiry the runner kills and reaps the
  direct child and returns immediately (`:417-423`). Nothing blocks on a
  surviving descendant. Decision 0031 already records that a descendant holding
  a pipe may outlive the direct child. There is no defect.
* **CODE-005 is defensive cleanup, not a reachable fault.** `original_head` is
  `None` only when `HEAD` is unreadable or not symbolic; `setup::run` refuses a
  detached HEAD before any mutation (`setup.rs:137-144`), and an unreadable
  `HEAD` fails that same check. The guard is still worth adding so
  `rollback_branches` handles every caller state, and its plan says so.
* **CODE-003, basis corrected.** The plan's first draft said git strips angle
  brackets from identities. libgit2 refuses them at commit construction
  (`git_signature_new`), so rejecting them at config parse moves an existing
  failure earlier rather than adding a new rule.
