# Implementation plans for the 2026-09-02 review

One plan per finding in `docs/2026-09-02_REPOSITORY_REVIEW.md`, written against
`main` at `7c5fa19` (0.1.8). Every plan follows the project's rules: a failing
test before the change, a decision file before any behaviour change, operator
intervention over automation. Line numbers refer to that tree and will drift as
plans land. September 7 follow-ups CODE-010/011 are written against `f1794e5`.

## Current status — 2026-09-07

Checked against `HEAD` at `f1794e5`, the implementation, regression tests, and
recorded decisions. All five top recommendations in the original review have
landed. The original review and plan steps remain historical descriptions;
this table and each plan's Status row track completion.

| Plan | Status | Evidence / remaining scope |
| --- | --- | --- |
| [CODE-010](CODE-010-stale-source-rewrite-authorization.md) | Implemented | Decision 0050: force only when the local tip is the source remote's tip; playbook 0003 for stale checkouts |
| [CODE-011](CODE-011-generated-marker-message-limit.md) | Scope accepted; implementation pending — P1 | Decision 0032 addendum: prevent new invalid output; document existing-state limitation |
| CODE-001 | Implemented | Decision 0048; represented-prefix boundary and promoted-branch regression tests |
| CODE-002 / ARCH-001 | Implemented | Shared `build_pending_dest_tip` used by sync and resolve; inherited-marker and clean-prefix parity tests |
| DOC-001 | Implemented | Decision 0046 Addendum 3 Findings Q–S; code references renamed |
| TEST-001 | Implemented | `SecretSource`, production-env refusal tests, and three binary smoke tests |
| PERF-001 | Implemented | Decision 0049; bulk dest fetch; source→dest lease fetches retained. Step 8's run-wide deadline remains optional and out of scope |
| CODE-004 | Documentation complete | Step 1 recorded under Finding S; current behavior accepted. Step 2 requires an owner decision |
| TEST-GAPS | Partially implemented | Promoted-branch resolve and binary tests complete; remaining state-transition coverage, step D coverage, and test-module split proposed |
| CODE-003, CODE-005–008 | Proposed | Committer validation, defensive rollback guard, divergence wording, cause enum, helper cleanup |
| CODE-009 | Proposed | Recommendation is documentation only; single-parent graft requires a separate decision |
| DOC-002 | Proposed | Mark the wizard decision as draft in its index entry |
| OPS-001–002 | Proposed | Resolution worktree location and bounded truncation diagnostics |
| PERF-002–003 | Proposed | Path-aware filter memo and stable taint-result memo |
| OPS-003 | Withdrawn | Runner already returns at its deadline; surviving descendants are documented |

Validation on this tree: `cargo test --workspace --all-features` passed
**400 tests (397 unit + 3 binary integration), zero failures**. The subsequent
production-readiness review also passed frozen all-target/all-feature check and
tests on Rust 1.98 and MSRV 1.89, plus formatting and Clippy on 1.98. Audit/deny
were unavailable; release build and live deployment were not rerun.

The new findings use CODE-010/011 to avoid reusing completed CODE-001/002.
See the [dated review follow-up](../../2026-09-02_REPOSITORY_REVIEW.md#production-readiness-follow-up--2026-09-07).
Existing proposed work below is retained, not re-reported as new defects.

## Remaining-work queue

| Priority | Task | Status / dependency | Impact | Effort |
| --- | --- | --- | --- | --- |
| P1 | [CODE-011 — generated message limit](CODE-011-generated-marker-message-limit.md) | Prevention and documentation accepted; implementation pending | Medium | Medium |
| P2 | [TEST-GAPS](TEST-GAPS-resolve-and-sync-coverage.md) | Partial; CODE-001/TEST-001 done; path-specific tests coordinate with OPS-001 | Medium | Medium |
| P2 | [CODE-003 — committer validation](CODE-003-committer-identity-validation.md) | Proposed; independent | Low | Small |
| P2 | [PERF-002 — path-aware filter memo](PERF-002-filter-tree-memo.md) | Proposed; shared replay dependency completed | Medium | Small |
| P2 | [CODE-007 — mapping cause enum](CODE-007-mapping-lookup-cause-enum.md) | Proposed; prerequisite for PERF-003 and OPS-002 | Low | Medium |
| P2 | [PERF-003 — stable lookup memo](PERF-003-memoize-tainted-lookups.md) | Proposed; after CODE-007 | Low | Small |
| P2 | [OPS-001 — worktree lifetime](OPS-001-resolution-worktree-location.md) | Proposed; decision 0033 addendum required | Low | Small |
| Optional P2 | [CODE-004 — own anchor](CODE-004-exclude-branch-own-anchor.md) | Documentation done; behavior accepted; owner decision for optional change; CODE-010 (its dependency) is implemented | Low | Medium |
| P3 | [CODE-005 — rollback guard](CODE-005-setup-rollback-head.md) | Proposed; defensive cleanup, no demonstrated reachable defect | Low | Small |
| P3 | [CODE-006 — divergence wording](CODE-006-divergence-message.md) | Proposed | Low | Small |
| P3 | [CODE-008 — helper cleanup](CODE-008-dead-and-duplicated-helpers.md) | Proposed | Low | Small |
| P3 | [CODE-009 — setup graph documentation](CODE-009-setup-ahead-of-dest.md) | Proposed; changing graft shape is a separate owner decision | Low | Small |
| P3 | [DOC-002 — wizard status](DOC-002-decision-0022-status.md) | Proposed | Low | Small |
| P3 | [OPS-002 — diagnostic cap](OPS-002-cap-truncation-message.md) | Proposed; after CODE-007 | Low | Small |

OPS-003 remains withdrawn. PERF-001's run-wide deadline remains optional and
outside its completed scope. CODE-010 is implemented; CODE-011's correctness
regression stays independent of the test-module split or general TEST-GAPS
completion.

## Original suggested order

Dependencies, not severity, drive the order. Each row is one PR-sized unit
unless the plan says otherwise.

| Order | Plan | Priority | Decision | Depends on |
| --- | --- | --- | --- | --- |
| 1 | [CODE-001 — dest→source resumes from the branch's own boundary](CODE-001-dest-to-source-boundary.md) | P0 | 0048 | — |
| 2 | [CODE-002 — resolve uses the same loop prevention as sync](CODE-002-resolve-loop-prevention.md) | P1 | — | — |
| 3 | [ARCH-001 — one source→dest replay loop](ARCH-001-single-replay-loop.md) | P1 | — | CODE-002 |
| 4 | [DOC-001 — record F-A / F-B / F-C](DOC-001-record-f-a-f-b-f-c.md) | P1 | 0046 Addendum 3 | — |
| 5 | [TEST-001 — pin and key enforced under test](TEST-001-pin-and-key-enforcement.md) | P1 | 0026 addendum | — |
| 6 | [PERF-001 — fetch dest's heads once per run](PERF-001-fetch-dest-heads-once.md) | P1 | 0049, 0031 addendum | CODE-001 (ordering only) |
| 7 | [CODE-007 — cause enum for mapping lookups](CODE-007-mapping-lookup-cause-enum.md) | P2 | — | — |
| 8 | [PERF-003 — memoize tainted lookups](PERF-003-memoize-tainted-lookups.md) | P2 | — | CODE-007 |
| 9 | [OPS-002 — cap the truncation message](OPS-002-cap-truncation-message.md) | P3 | — | CODE-007 |
| 10 | [PERF-002 — memoize filtered subtrees](PERF-002-filter-tree-memo.md) | P2 | 0032 list gains one limit | ARCH-001 |
| 11 | [CODE-003 — committer identity validation](CODE-003-committer-identity-validation.md) | P2 | — | — |
| 12 | [OPS-001 — resolution worktree location](OPS-001-resolution-worktree-location.md) | P2 | 0033 addendum | — |
| 13 | [TEST-GAPS — resolve and sync coverage](TEST-GAPS-resolve-and-sync-coverage.md) | P2 | — | CODE-001, TEST-001, OPS-001 |
| 14 | [CODE-004 — rewritten branch's own anchor](CODE-004-exclude-branch-own-anchor.md) | P2 | 0046 addendum (step 2 only) | DOC-001 |
| 15 | [CODE-005 — setup rollback HEAD (defensive)](CODE-005-setup-rollback-head.md) | P3 | — | — |
| 16 | [CODE-006 — divergence message](CODE-006-divergence-message.md) | P3 | — | — |
| 17 | [CODE-008 — dead and duplicated helpers](CODE-008-dead-and-duplicated-helpers.md) | P3 | — | — |
| 18 | [DOC-002 — decision 0022 status](DOC-002-decision-0022-status.md) | P3 | — | — |
| 19 | [OPS-003 — timed-out git children](OPS-003-kill-and-reap-children.md) | withdrawn | — | — |
| 20 | [CODE-009 — setup ahead of dest](CODE-009-setup-ahead-of-dest.md) | P3 | 0023 addendum | — |

Rows 11, 15, 16, 17, 18 are independent and can be done in any gap.

## Decision records referenced by the plans

Rows for unfinished plans describe proposed documentation, not settled
decisions, except 0050, which is implemented, and the 0032 CODE-011
addendum, which is accepted with implementation pending.

| Number | Plan | Subject |
| --- | --- | --- |
| 0048 | CODE-001 | dest→source resumes from the newest authenticated boundary on either side |
| 0049 | PERF-001 | dest heads are fetched once into a transient ref namespace |
| 0050 | CODE-010 | mirror-only force requires the local tip to match the source remote's tip (0039's fifth condition); playbook 0003 for the stale-checkout halt |
| 0032 CODE-011 addendum | CODE-011 | the 1 MiB limit applies to the complete generated message; existing-state repair out of scope |
| 0046 Addendum 3 | DOC-001, CODE-004 | Findings Q, R, S (the behaviours cited as F-A, F-B, F-C) |
| 0026 addendum | TEST-001 | secrets are supplied through a `SecretSource`; the environment is one source, read at today's points |
| 0033 addendum | OPS-001 | the resolution worktree root is the repository's common dir |
| 0031 addendum | PERF-001 | a runner variant may write a bounded, caller-generated stdin (`git fetch --stdin`) |
| 0032 list | PERF-002 | `MAX_FILTER_MEMO_ENTRIES` joins the static limits |
| 0023 addendum | CODE-009 | graft shape on a branch already ahead of dest |

CODE-004's documentation step is complete; CODE-009 recommends recording the
behavior only. Both leave their optional code change to the project owner.
OPS-003 is withdrawn: the runner already returns at its deadline, and decision
0031 already documents surviving descendants.

## Revision 2026-09-02 (plan review)

Ten review points were checked against the code; all held, and the plans were
corrected in place. Each corrected plan ends with a "Revision history" entry.
In brief: CODE-001 keeps today's B1 ancestry precondition (the draft could
accept a force-rewound dest tip); PERF-001 uses a bounded `--stdin` refspec
list instead of a wildcard (decision 0032); OPS-003 withdrawn; CODE-004 step 2
split into options A/B because rule 3 only picks the own anchor when it is the
ancestor; TEST-001 uses an on-demand `SecretSource` to preserve command-level
repository/key ordering (the later 0026 addendum records the narrower
pin/control-file precedence change), and a known-answer digest constant for the binary test; PERF-002's
memo gets a static cap; CODE-005 reclassified as defensive; TEST-GAPS' setup
test reframed as preflight atomicity; CODE-003 states libgit2's angle-bracket
refusal as its basis; PERF-001's test counter is thread-local. The review
document carries an errata section for OPS-003, CODE-005 and CODE-003.

## Gates for every plan

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --release --locked
```

Record the resulting test count in `design/log.md` when a plan lands.
