# Plan CODE-003 — validate committer identity at config parse

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, CODE-003 |
| Severity / priority | LOW / P2 |
| Effort | Small |
| Decision required | No for step 2a (control characters, the reported defect). Step 2b (angle brackets) moves an existing libgit2 refusal earlier and is recorded in `log.md`; it is not "the same validation the URL fields apply" |
| Depends on | — |
| Status | Proposed |

## Problem

`Config::parse` (`src/config.rs:89-94`) checks only that `[committer].name` and
`.email` are non-empty. `git2::Signature::now("a\nb", …)` returns `Ok`, so a
pinned config with a line break writes a malformed commit header. A remote with
`receive.fsckObjects` rejects it at push time, after local refs were already
moved.

Angle brackets are a different case. libgit2's `git_signature_new`
(`libgit2/src/libgit2/signature.c:79-83`, vendored by `libgit2-sys 0.18.7`)
returns an error for a name or email containing `<` or `>`. A config with them
parses today but every commit construction fails, after the fetch. Refusing
at parse time turns a late, confusing failure into an early one; it does not
reject any configuration that can produce a commit today.

## Steps

### Step 1 — failing tests

**Files.** `src/config.rs` tests.

**Test first.** `Config::parse` rejects, with an error naming the field:
`name = "a\nb"`, `email = "a\rb"`, `name = "a\u{0}b"`. It still accepts a name
with non-ASCII letters and an email with `+`. For step 2b: a test that
`git2::Signature::now("a<b>c", "x@y", …)` is `Err` today (documents the basis)
and that `Config::parse` rejects `email = "a<b>c"` naming the field.

### Step 2a — reject control characters

**Files.** `src/config.rs:89-94`.

**Change.** For each of the two fields: reject empty (already) and any
`char::is_control`. Error: `config at {path} contains an invalid
[committer].{field}`. This is the control-character check the URL fields
already apply at `:99-103`.

### Step 2b — reject angle brackets

**Files.** `src/config.rs:89-94`, `design/log.md`.

**Change.** Also reject `<` and `>`. One `log.md` line: "committer identity
validation now refuses angle brackets at parse time; libgit2 already refused
them at commit time, so no working configuration changes meaning."

### Step 3 — confirm the push-time symptom is gone

Add one end-to-end test in `src/commands/sync/tests/run_entrypoint.rs`: a
config with a newline in the name fails at `policy::load`, before any fetch
(assert no `FETCH_HEAD`).

## Verification

`cargo test config`; fmt and clippy clean.

## Revision history

* 2026-09-02, after plan review: the first draft said git "strips" angle
  brackets and bundled their rejection with the control-character fix as "the
  same validation". libgit2 refuses them outright; step 2b now states that
  basis and is recorded separately.
