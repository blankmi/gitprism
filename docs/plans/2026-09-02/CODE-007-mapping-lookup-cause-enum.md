# Plan CODE-007 — type the causes behind `MappingLookup::Contradictory`

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational CODE-007 |
| Severity / priority | INFO / P2 |
| Effort | Medium |
| Decision required | No — internal representation; operator-facing text unchanged |
| Depends on | — |
| Unblocks | PERF-003 (needs to tell a stable cause from an unstable one), OPS-002 (message shaping in one `Display`) |
| Status | Proposed |

## Problem

`MappingLookup::Contradictory(String)` (`src/commands/sync/mapping_index.rs:24-28`)
and `DestAnchor::Contradictory(String)` (`anchor.rs:285`) carry four different
situations as prose:

| Cause | Produced at | Stable within a run? |
| --- | --- | --- |
| Incomparable canonical destinations for one source commit | `resolve_for_anchor` | yes |
| Index incomplete (truncated scan, capped listing, undecodable ref) | `:203-205`, `:399-404` | yes — `truncated_scans` only grows during reconstruction |
| This branch's own walk hit `scan_limit` | `:382-390` | no — a push this run can add a mapping inside the horizon |
| Canonical dest object missing locally (F-B) | `:240-249` | yes |

Callers and tests distinguish them by substring.

## Steps

### Step 1 — enum with a `Display` that reproduces today's strings

**Files.** `src/commands/sync/mapping_index.rs`.

**Change.**

```rust
pub(crate) enum ContradictionCause {
    IncomparableDestinations { source: Oid, destinations: Vec<(Oid, Vec<MappingProvenance>)> },
    IndexIncomplete { scans: Vec<String> },
    OwnScanHorizon { limit: usize },
    MissingDestObject { source: Oid, dest: Oid },
}
```

`MappingLookup::Contradictory(ContradictionCause)`; `impl Display` emits exactly
the current messages so every existing substring assertion keeps passing in this
step. Add `is_stable_for_run(&self) -> bool` returning `false` only for
`OwnScanHorizon`.

### Step 2 — propagate

**Files.** `anchor.rs:285` and its match arms, `mod.rs` sites that format the
halt message.

**Change.** `DestAnchor::Contradictory(ContradictionCause)`; format with
`Display` at the single reporting site.

### Step 3 — migrate tests to variants

**Files.** tests in `mapping_index.rs`, `sync/tests/anchor.rs`, `sync/tests/scheduling.rs`.

**Change.** Replace `.contains("exceeds the")`-style checks with
`matches!(lookup, MappingLookup::Contradictory(ContradictionCause::OwnScanHorizon { .. }))`.
Keep one test per cause that asserts the `Display` text, so operator wording is
still pinned.

### Step 4 — hand off

PERF-003 consumes `is_stable_for_run`; OPS-002 edits `IndexIncomplete`'s
`Display`.

## Verification

Full suite unchanged in count after step 1; fmt and clippy clean.
