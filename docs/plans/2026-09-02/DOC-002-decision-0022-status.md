# Plan DOC-002 — decision 0022 is listed as settled but is a draft

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational DOC-002 |
| Severity / priority | INFO / P3 |
| Effort | Small |
| Decision required | No — the decision's own `status: draft` is the fact being surfaced |
| Depends on | — |
| Status | Proposed |

## Problem

`design/decisions/index.md:24` describes 0022 (interactive setup wizard) as
current behaviour. The file's frontmatter says `status: draft`; `design/log.md`
records "no code written"; no wizard code exists in `src/commands/setup.rs`.

## Steps

### Step 1 — index entry

**Files.** `design/decisions/index.md:24`.

**Change.** Prefix the description with "Draft, not implemented." in the same
style 0043's entry uses "Superseded by decisions/0046." Keep the rest of the
sentence so the intent is still discoverable.

### Step 2 — header note in the decision

**Files.** `design/decisions/0022-setup-gains-an-interactive-first-run-wizard.md`.

**Change.** Below the frontmatter, one line: "Status: draft. No implementation
exists as of 2026-09-02; `gitprism setup` fails with a clear error when the
config path is missing." Link to the log entry that deferred it.

### Step 3 — log

One `design/log.md` line recording the bookkeeping fix.

## Verification

Read-through of `index.md`; nothing to build.
