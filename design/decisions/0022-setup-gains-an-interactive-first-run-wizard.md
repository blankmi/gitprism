---
type: Decision
title: setup gains an interactive first-run wizard when no config exists
description: When gitprism setup's resolved config path doesn't exist and stdin/stdout is a terminal, setup prompts interactively for dest's URL, branches, source's URL, and committer identity, writes .gitprism.toml from the answers, and then runs exactly as it does today. Non-interactive contexts keep today's hard failure unchanged.
tags: [architecture, setup, config, onboarding]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

Today, `gitprism setup` reads `.gitprism.toml` straight off disk
(`fs::read_to_string` in `src/commands/setup.rs`) and hard-fails with "reading
config at ..." if it's missing — [decisions/0012](0012-config-versioned-in-source.md)
deliberately makes the user hand-author that file, uncommitted, before `setup`
ever runs. That's a real gap for anyone bootstrapping gitprism for the first
time: nothing guides them through what the file needs (`committer`, `branches`,
`[source]`/`[dest]` `url`, per decisions/0012/0013/0005/0017), and the most
natural bootstrap sequence — `git clone <dest-url> source && cd source`, which
[decisions/0021](0021-setup-accepts-a-clean-clone-of-dest.md) already recognizes
as a safe starting state — leaves dest's own URL already sitting right there in
the freshly-cloned repo's `origin` remote, unused.

Raised by the project owner: when no config exists yet, `setup` should ask for
it interactively instead of just failing. Discussed and settled in conversation
(not batch-decided), covering: whether committer identity belongs in this
wizard too (the schema requires it, but the original ask only named
branches/source/dest); how to pick branches out of a dest that can have far
more of them than fit on one screen; whether to prefill anything from the
clone-first repo state decisions/0021 already recognizes; and what to do about
`origin` now pointing at dest when this checkout's real long-term identity, once
source's own URL is known, is source.

**Prior art check**: `dialoguer` (already the same console-rs vendor family as
`console`/`indicatif`, both already dependencies) has a `MultiSelect`, but no
built-in filtering — a dest with a large number of branches would just be a
long, unfiltered scroll. `inquire` (a separate, actively-maintained crate) does
support type-to-filter plus a configurable page size on its `MultiSelect`,
directly solving "dest has more branches than fit on screen" rather than
requiring a hand-rolled cap that would silently drop branches from view. That
concrete feature gap — not general preference — is why this decision reaches
for a new crate family instead of extending the one already partly present.

# Decision

1. **Trigger**: `setup` checks whether the resolved config path exists before
   attempting to read it (the same `io::ErrorKind::NotFound` check already used
   for a missing `.gitprismignore`). Missing → run the wizard below. Present →
   no behavior change at all; every existing precondition, error, and rollback
   in decisions/0006/0012/0021 is untouched.
2. **Non-interactive stays a hard failure**: guarded by `console::user_attended()`
   (already a dependency) — a missing config with no attached terminal still
   fails exactly as today, with the same message. gitprism's CI-safe,
   trigger-agnostic posture (requirements/0001, playbooks/0001) isn't something
   this decision touches; a missing config in a pipeline was already fatal
   before this, and remains so.
3. **Library**: [`inquire`](https://crates.io/crates/inquire), used uniformly for
   every prompt this wizard needs (`Text`, `Confirm`, `MultiSelect`) — not mixed
   with `dialoguer` for some prompts and `inquire` for others. `console`/
   `indicatif` are unaffected; they back `sync`'s progress display
   ([decisions/0020](0020-sync-status-output-is-a-pinned-progress-display.md)),
   a separate concern.
4. **Prompt sequence** (ordered by data dependency, not by the order the ask
   was phrased in):
   1. **Dest URL** (`Text`) — prefilled as an editable default from the
      source-root repo's existing `origin` remote URL, if one exists (the
      decisions/0021 clone-first case); blank default otherwise.
   2. **Branches** (`MultiSelect`) — populated via `git ls-remote --heads` on
      the URL just entered. Sorted `main`/`master` first, then `develop`, then
      anything matching `release*`, then everything else alphabetically —
      nothing excluded, just reordered so the small set decisions/0017 actually
      wants here (dest→source's explicit list) surfaces first. Always followed
      by a free-text prompt ("any other branches, comma-separated, blank to
      skip") for anything discovery didn't surface. If `ls-remote` itself fails
      (bad URL, auth not configured yet, network down), skip straight to that
      free-text prompt as the sole input rather than blocking the wizard on a
      failure this step doesn't need to be fatal for.
   3. **Source URL** (`Text`) — blank-by-default, skippable. Leaving it blank
      omits `[source].url` from the written TOML entirely, matching
      [decisions/0013](0013-repo-urls-optional-fall-back-to-env-vars.md)'s
      `GITPRISM_SOURCE_URL` fallback exactly — the wizard never forces a
      literal committed URL where the env var was always meant to be enough.
   4. **Committer name/email** (two `Text` prompts) — prefilled as editable
      defaults from `git config user.name`/`user.email` (read from the same
      repo setup is running in) if set, blank otherwise.
5. **Origin-remote follow-up**: if step 4.3 actually collected a non-blank
   source URL, ask a `Confirm` ("point this repo's `origin` at source instead
   of dest? this checkout is source going forward", default yes). Accepting
   repoints `origin` to source's URL if `origin` already exists, or adds it if
   it doesn't — the same end state either way, regardless of which bootstrap
   path (clone-first vs. from-scratch `git init`) got here. Declining, or
   leaving source's URL blank, leaves whatever remote configuration already
   existed untouched. This step is local git-repo housekeeping for the human's
   own convenience only: gitprism itself never reads a named remote for
   anything — `git.rs`'s `fetch`/`push` always take a raw URL straight out of
   resolved config — so skipping it or answering "no" has zero effect on
   `setup`'s or `sync`'s own behavior.
6. Once every prompt completes, the wizard assembles `.gitprism.toml`'s
   contents (decisions/0012's schema: `committer`, optional `[source]`/`[dest]`
   `url`, `branches`), writes it to the resolved config path on disk exactly as
   if hand-authored, and then `setup`'s existing logic runs completely
   unchanged, reading that same freshly-written file back through the same
   `Config::parse` path used today.

# Why

* Closes the actual gap between "clone dest" (decisions/0021's recognized
  starting state) and "hand-write a TOML file with no guidance," using state
  (`origin`'s URL, local git identity) that's already sitting right there
  instead of asking the user to retype it.
* `committer` is a required field in the existing schema that's easy to forget
  and, until now, only surfaces as a parse failure later — prompting for it
  directly, with a sensible editable prefill, catches it at the point it's
  actually needed instead of downstream.
* `inquire`'s filtering solves the concrete "dest can have far more branches
  than fit on screen" problem directly, rather than a hand-rolled cap that
  would silently drop branches from view — no branch is ever excluded from
  what discovery finds, only reordered and made searchable.
* The origin-remote follow-up defaults toward the generally more useful
  post-setup state (bare `git push`/`git pull` acts on source, the repo you're
  actually iterating in, not dest) without ever touching gitprism's own
  mechanics, and it's confirm-gated rather than silent — consistent with this
  project's general hard-stop/ask-before-mutating stance
  ([decisions/0007](0007-conflict-policy-hard-stop.md)), extended here from
  "never guess through a conflict" to "never silently mutate repo state outside
  gitprism's own object graph either."
* Every other invocation — config already present, or the wizard's own
  non-interactive fallback — is untouched: this is additive to `setup`'s
  existing decision tree, not a replacement of any of it.

# Consequences

* New dependency: `inquire`, alongside (not replacing) `console`/`indicatif`.
* `setup.rs` gains one pre-step — gated on the resolved config path not
  existing and `console::user_attended()` — that runs before today's
  `fs::read_to_string`; everything from config-loaded-into-memory onward is
  unchanged.
* `git.rs` needs two new small pieces of plumbing it doesn't have today: a
  full branch-listing call (today it only has `remote_ref_exists`, a
  single-branch check) for the `ls-remote --heads` discovery step, and reading/
  setting/adding a named local remote (today `git.rs` never touches named
  remotes at all — every fetch/push takes a raw URL directly).
* Per CLAUDE.md's TDD convention, the wizard's pure logic (branch sort order,
  assembling TOML text from collected answers, repoint-vs-add branching for
  `origin`) needs to be extracted into small functions testable without
  exercising real interactive prompts — `inquire`'s own prompt calls stay a
  thin, directly-untested IO shell, the same shape `git.rs` already uses for
  real `git` subprocess calls.
* **Not decided here**: exact prompt wording/copy; the precise TOML formatting
  the wizard emits (key order, comments) beyond "parses back via `Config::parse`
  unchanged"; and whether a comparable wizard step for `.gitprismignore` is ever
  warranted — out of scope, this decision is specifically about
  `.gitprism.toml`'s fields.
