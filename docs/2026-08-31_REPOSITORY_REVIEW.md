# Repository review — 2026-08-31

## Executive summary

The branch is well-designed for its purpose: a local, two-repository Git
synchronization CLI. Its safety model is explicit, Git history operations are
tested against real repositories, and the implementation centralizes production
Git subprocess execution. The release gates pass on this tree:

* `cargo fmt --all -- --check`
* `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
* `cargo test --workspace --all-features --locked` — 328 passing
* `cargo build --release --locked`

`cargo audit` and `cargo deny` were not installed in the review environment;
the CI workflow runs them using pinned actions.

| Area | Score | Rationale |
| --- | ---: | --- |
| Architecture | 8/10 | Clear command/domain/Git boundaries; sync orchestration is necessarily dense. |
| Rust code quality | 8/10 | Idiomatic error propagation and no production unsafe Rust. |
| Git integration | 8/10 | Centralized subprocess handling and strong Git-state coverage; two ref-byte edge cases remain. |
| Security | 6/10 | No shell injection path, but trusted Git configuration/hooks are an important documented boundary. |
| Reliability | 7/10 | Conservative failure model; malformed non-UTF-8 ref handling can cause resource use or a whole-run abort. |
| Testing | 9/10 | Extensive real-Git integration tests; targeted hostile-ref tests are missing. |
| Cross-platform robustness | 7/10 | CI covers three platforms; Windows lacks Unix-equivalent opened-file identity checks. |
| Maintainability | 8/10 | The 2026-08-28 domain-boundary extraction substantially improved sync cohesion; the remaining large modules have no current split mandate. |
| Production readiness | 6/10 | Suitable after the P1 items or with their risks accepted and documented. |

## Architecture overview

Execution follows this path:

```text
Clap CLI (main.rs)
  -> command handlers (commands/setup.rs, commands/sync, commands/resolve.rs)
  -> config/policy/marker/lock/limits domain modules
  -> git2 object/ref/tree operations + src/git.rs subprocess boundary
  -> repository refs, worktrees, and terminal output
```

The binary exposes `setup`, `sync`, `resolve`, and `policy-hash`. `git2` is used
for repository discovery and object manipulation. `src/git.rs` owns production
calls to the Git executable, including bounded concurrent stdout/stderr capture,
timeouts, and a scrubbed child environment. There is no server, database, or
application persistence outside the repositories and their Git metadata.

The design bundle is the primary architectural specification. In particular, the
sync design deliberately prefers a clear operator halt to guessing an unsafe Git
recovery.

## Threat model

Trusted inputs include the operator's executable environment, the selected Git
executable, and local Git configuration/hooks as documented in decision 0029.
CLI paths and options, remote advertisements, refs, commit data, configuration,
and repository filesystem contents must otherwise be treated as untrusted or
malformed.

The material boundaries are:

* Git subprocess arguments and inherited Git configuration.
* Source and destination repository object/ref databases.
* Filesystem paths used for repository discovery, locks, and controlled-file
  validation.
* Terminal output containing branch and repository-derived values.

The main realistic failure modes are malformed refs, changing refs during a run,
large/shallow history horizons, missing objects, local Git hook/config execution,
and repository-file races.

## Critical and high findings

### H-01 — Git hooks and Git configuration are a code-execution trust boundary

* **Category:** subprocess security
* **Severity:** High
* **Confidence:** High
* **Evidence:** `src/git.rs` constructs direct Git child processes; `resolve`
  invokes `git cherry-pick` and `git commit` through `git.rs`. Decision 0029
  explicitly preserves local/global Git configuration, credential helpers, URL
  rewrites, and hooks as trusted inputs.

This is not shell command injection: subprocess arguments are passed directly and
are carefully delimited. It is nevertheless arbitrary local code execution if an
attacker can influence the repository's `.git/config`, `core.hooksPath`, or the
operator's Git configuration and can induce `gitprism resolve` to run. Git itself
will execute configured hooks or helpers.

**Trigger:** an operator runs the tool in a repository obtained from an untrusted
or compromised local source that contains a malicious hook path/configuration.

**Impact:** execution under the invoking user's account.

**Recommendation:** no implementation action is currently warranted. The README
already documents this boundary, consistently with decision 0029. A hardened
subprocess mode would reopen that decision and needs an explicit project-owner
choice; it must not be added as an incidental remediation.

## Medium findings

### M-01 — Source branch limit can be bypassed by non-UTF-8 ref names

* **Category:** reliability / malicious repository handling
* **Severity:** Medium
* **Confidence:** High
* **Evidence:** `src/commands/sync/mod.rs`, source branch enumeration around
  `source_branches` and `skipped`.

The `MAX_SOURCE_BRANCHES` guard tests `source_branches.len()` before UTF-8
decoding. A non-UTF-8 ref is appended to `skipped` rather than `source_branches`,
so it does not consume the budget. The existing non-UTF-8 packed-ref test proves
this input form is accepted by the test fixture.

**Trigger:** a malformed or malicious repository contains many non-UTF-8 branch
names, especially packed refs.

**Impact:** unbounded enumeration, allocation, and warning output despite the
documented branch-discovery budget.

**Recommendation:** count every enumerated branch before attempting UTF-8
conversion, and add a test that exceeds the bound exclusively with non-UTF-8
names.

### M-02 — Destination remote ref listing uses lossy UTF-8 conversion

* **Category:** correctness / malicious repository handling
* **Severity:** Medium
* **Confidence:** High
* **Evidence:** `src/git.rs`, `remote_branch_names` converts `git ls-remote`
  stdout with `String::from_utf8_lossy`; its output feeds reconstruction in
  `src/commands/sync/anchor.rs`.

An advertised ref containing invalid UTF-8 is converted to U+FFFD. That resulting
string can still pass branch-name validation, be marked present, and later be
fetched under a different ref name. Refreshing repeats the same conversion; the
failed fetch then makes reconstruction incomplete and aborts the run.

**Trigger:** destination advertises a branch such as a valid byte sequence with
one non-UTF-8 byte in its name.

**Impact:** a malformed or hostile destination can prevent all synchronization,
despite the code comment suggesting invalid branch names are skipped.

**Recommendation:** parse `ls-remote` output as bytes and reject invalid ref-name
bytes without replacement. Whether an invalid advertised destination ref is
skipped or makes the listing incomplete is an architectural policy decision and
must be recorded before implementation. Either choice must not fabricate a
replacement-character name. Add an integration test with a byte-invalid advertised
ref.

## Low and informational findings

### L-01 — Windows lacks the Unix opened-file identity and hard-link checks

* **Category:** filesystem safety / cross-platform robustness
* **Severity:** Low
* **Confidence:** Medium
* **Evidence:** `src/limits.rs` compares Unix device/inode identity and link count
  for sensitive opened files; the Windows path can enforce file type but not the
  equivalent identity check.

Symlinks are rejected on supported platforms, so exploitation requires a local
race and is not shown to be remotely reachable. Windows file-ID verification
would align the protections, or the limitation should be documented.

### I-01 — Release checksums provide integrity, not publisher provenance

* **Category:** release security
* **Severity:** Info
* **Confidence:** High
* **Evidence:** the release workflow produces `SHA256SUMS`; the README correctly
  states checksums do not establish provenance.

Consider signed tags, signed release artifacts, or build provenance before broad
binary distribution.

### I-02 — Mapping-index horizon is an accepted availability tradeoff

* **Category:** availability
* **Severity:** Info
* **Confidence:** High

The documented mapping horizon can halt exact-anchor operations in sufficiently
large histories. This is a deliberate safety decision rather than a defect, but
operators should monitor it and retain recovery guidance.

## Git and subprocess security assessment

Production code does not invoke a shell. Git arguments are supplied through
`Command::arg`, so branch/ref/path values are not shell-interpolated. The wrapper
uses `--` where remote operands require it, validates branch names at its API
boundaries, bounds captured output, validates exit status, uses timeouts, and
does not inherit an interactive stdin.

The remaining concern is Git's own trusted configuration and hook model (H-01),
not an argument-injection defect. Git output needs byte-accurate handling at the
destination ref-listing boundary (M-02).

## Filesystem safety assessment

Repository mutation is intentionally narrow: Git object/ref operations and the
controlled-file checks. Path validation, symlink rejection, locking, and
repository-type checks are strong. No broad deletion or user-file overwrite path
was found. The platform-specific identity gap in `limits.rs` is a defense-in-depth
issue on Windows (L-01), not evidence of a current data-loss path.

## Testing assessment and gaps

The test suite is a major strength. It uses real temporary Git repositories and
covers shallow clones, force-with-lease behavior, mirroring, merges, deleted
branches, control-file policy, and run-level aggregation. CI covers Linux, macOS,
and Windows.

Highest-value additions:

1. Exceed `MAX_SOURCE_BRANCHES` using only non-UTF-8 packed refs.
2. Advertise a destination ref containing invalid UTF-8 and assert a conservative,
   intelligible outcome rather than a fetch of a replacement-character name.
3. Add Windows-focused coverage for opened-file replacement/race protections if a
   Windows file-ID solution is implemented.

## Architecture assessment

The module boundaries are appropriate for a CLI of this size. Configuration,
policy, markers, Git operations, and sync planning are meaningfully separated;
Git subprocess behavior is centralized and testable. Error contexts are generally
specific, and review found no production `unsafe` blocks.

The 2026-08-28 design-log entry records the completed, behaviour-preserving
domain-boundary extraction of the former 12,054-line `commands/sync.rs` into
`sync/mod.rs`, `marker_scan.rs`, `anchor.rs`, `local_advance.rs`,
`policy_check.rs`, and `filter.rs`, with tests and shared fixtures moved as well.
It also records why the three marker revwalks were intentionally not unified:
their distinct starting points and predicates made equivalence insufficiently
obvious. `sync/mod.rs` is therefore the intended post-extraction orchestration
residue, not evidence that the split remains to be done. `mapping_index.rs` and
`resolve.rs` are still large, but no concrete defect supports another extraction
or a marker-walk rewrite now.

## Recommended action plan

### P0 — Immediate

No confirmed arbitrary shell injection, memory-safety flaw, or destructive
filesystem vulnerability was found. H-01 is an already documented, explicit
trusted-local-Git boundary; it is not a P0 code change unless decision 0029 is
reopened.

### P1 — Before release

| Recommendation | Impact | Effort |
| --- | --- | --- |
| Count all source refs, including non-UTF-8 names, against the discovery budget (M-01). | Medium | Small |
| Decide and record whether invalid advertised destination refs are skipped or make reconstruction incomplete; then replace lossy decoding (M-02). | High | Medium |

### P2 — Near term

| Recommendation | Impact | Effort |
| --- | --- | --- |
| Add hostile non-UTF-8 ref integration tests. | Medium | Small |
| Evaluate a Windows file-ID/opened-file identity check in `limits.rs`. | Low | Medium |
| Keep mapping-horizon monitoring and recovery guidance visible in operations docs. | Medium | Small |

### P3 — Opportunistic

| Recommendation | Impact | Effort |
| --- | --- | --- |
| Add signed tags/artifacts or build provenance for public binary releases. | Medium | Medium |

## Top 10 recommendations

1. Fix source budget accounting for non-UTF-8 refs.
2. Decide and record the policy for invalid advertised destination refs.
3. Eliminate lossy UTF-8 handling of destination ref advertisements under that
   decision.
4. Add regression tests for both malformed-ref cases.
5. Keep decision 0029's trusted Git configuration/hooks boundary visible in
   operator documentation.
6. Keep all Git subprocess construction centralized in `src/git.rs`.
7. Preserve the current `--`, timeout, bounded-output, and noninteractive-stdin
   subprocess protections.
8. Investigate Windows file-identity parity for sensitive file reads.
9. Add signed provenance if distributing release binaries to third parties.
10. Preserve the completed sync domain-boundary split; do not reopen
    marker-walk unification or pursue a module split without a concrete new
    correctness or maintenance driver.
