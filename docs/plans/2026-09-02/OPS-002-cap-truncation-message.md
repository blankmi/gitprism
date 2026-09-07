# Plan OPS-002 — cap the truncation message

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational OPS-002 |
| Severity / priority | INFO / P3 |
| Effort | Small |
| Decision required | No |
| Depends on | CODE-007 (edit one `Display` arm) — or do standalone in `truncation_message` |
| Status | Proposed |

## Problem

`MappingIndex::truncation_message` (`src/commands/sync/mapping_index.rs:162-168`)
joins every entry of `truncated_scans`. One entry is added per truncated head
and per listing problem, so with thousands of dest branches beyond the horizon
the message is thousands of clauses long, and it is repeated on every halted
branch's completion line.

## Steps

### Step 1 — failing test

**Files.** `mapping_index.rs` tests.

**Test first.** Index with 12 distinct truncation notes and 3 duplicates. Assert
the message names at most 5 distinct notes, ends with "and 7 more", and contains
no duplicate.

### Step 2 — shape the text

**Files.** `mapping_index.rs:162-168` (or `ContradictionCause::IndexIncomplete`'s
`Display` after CODE-007).

**Change.** Deduplicate, keep insertion order, print the first 5, append
`; and {n} more` when longer. Prefix with the total count: "the mapping index
could not be fully reconstructed this run ({total} incomplete scans), so this
lookup cannot be trusted: …".

### Step 3 — print the detail once

**Files.** `src/commands/sync/mod.rs` reporting sites, `src/progress.rs`.

**Change.** Emit the full deduplicated list once as a note line after
reconstruction (the reporter already has a note-line shape, decision 0020); each
halted branch's line carries only the one-sentence summary.

## Verification

`cargo test mapping_index`, `cargo test progress`; a manual look at the output of
a tainted run in a terminal.
