# Plan DOC-001 — record the behaviours cited as F-A / F-B / F-C

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 4, DOC-001 |
| Severity / priority | MEDIUM / P1 |
| Effort | Small |
| Decision required | Yes — Addendum 3 to decision 0046 (documentation of existing behaviour, no code change) |
| Depends on | — |
| Related | CODE-004 (its trade-off is recorded here) |
| Status | Implemented 2026-09-04 (steps 1-3, including follow-up corrections) |

## Problem

Code comments cite `decisions/0046, F-A`, `F-B`, `F-C` at
`mapping_index.rs:215, 240, 342, 410, 590, 689` and in tests, `anchor.rs:396, 460`,
`mod.rs:833, 854, 1012`, and `git.rs:575`. `grep 'F-A\b|F-B\b|F-C\b' design/`
finds nothing. Decision 0046's addenda name their findings A–P, so "F-A" reads
as "Finding A", which is a different rule. AGENTS.md makes `design/decisions/`
the source of truth; three behaviours currently exist only in comments.

## The three behaviours, as implemented

* **F-A — provenance survives source-branch deletion.** Reconstruction lists
  dest's branches directly (`remote_branch_names`) so a mirror-only branch's
  dest ref still contributes `SourceToDest` mappings after its source branch is
  deleted (0018 Case 2 cleanup). Verification is against the marker's own
  recorded branch (`verify_self`), never the scanning head.
* **F-B — a mapping whose canonical dest object is absent locally is a
  per-branch refusal.** Existence is checked with `find_commit` before any
  `graph_descendant_of` comparison, and the walk does not continue past it to an
  older mapping (that could skip dest-native content a `DestToSource` marker
  represents). Already partially described in Addendum 1's last paragraph.
* **F-C — self-exclusion during canonicalization.** When resolving an anchor for
  `exclude_branch`'s own rebuild, destinations whose *only* provenance is
  `exclude_branch` are dropped if any other provenance exists for the same source
  commit; if every destination is the branch's own, they are kept. Mappings the
  run itself just built for `branch` are trusted by construction and recorded
  without re-verification.

## Steps

### Step 1 — write Addendum 3

**Files.** `design/decisions/0046-dest-anchors-come-from-exact-authenticated-mappings.md`.

**Change.** "Addendum 3 (2026-09-xx): three implementation rules recorded". One
subsection per rule, continuing the addenda's letter sequence so the labels do
not collide: **Finding Q** (F-A), **Finding R** (F-B), **Finding S** (F-C).
Each: what the code does, why, and the test(s) that pin it (names from
`mapping_index.rs` tests at `:1373, 1576, 1647, 1676, 1705, 1784`).

Under Finding S, record CODE-004's trade-off verbatim: when the nearest mapped
ancestor has both the branch's own projection and a sibling's, the own
projection is dropped even when it is comparable, so a force-rebuild may rewind
dest further than the amend required. State that this is safe under 0038 and
accepted for now; CODE-004's plan holds the alternative.

### Step 2 — rename the code references

**Files.** the fourteen sites listed above.

**Change.** `F-A` → `Addendum 3, Finding Q`, `F-B` → `Finding R`, `F-C` →
`Finding S`. Comment-only; no behaviour change. Run `grep -rn 'F-[ABC]\b' src design`
and expect zero hits.

### Step 3 — index and log

**Files.** `design/decisions/index.md` (0046 entry gains "Addendum 3 records
Findings Q–S"), `design/log.md`.

## Verification

`cargo test` unchanged; grep from step 2 empty; the addendum's test names exist
(`cargo test <name> -- --list`).
