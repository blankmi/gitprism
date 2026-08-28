---
type: Decision
title: source→dest reports a mirror-only branch with no shared dest history as a warning and moves on, not a hard stop
description: A local branch discovered by source→dest (decisions/0017) that has no ref on dest yet and no Gitprism-Dest-Commit trailer anywhere in its first-parent history — genuinely unrelated, pre-existing history that predates gitprism setup — is reported as a magenta Outcome::Warning status line (a new variant distinct from Outcome::Skipped, both in label and in color, extending decisions/0020's green/yellow/red/cyan palette) and the sync run continues with the next branch, rather than aborting every other branch — including every properly round-tripped one — over one branch nobody asked gitprism to manage. Supersedes this decision's own earlier drafts: the first treated it as a hard stop matching decisions/0023's setup-time precedent; the second reused Outcome::Skipped, conflating it with decisions/0018's genuinely benign "already merged, cleaned up" skip. config.branches' own hard-fail on a missing trailer is unchanged.
tags: [architecture, branches, error-handling]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

[decisions/0017](0017-source-to-dest-mirrors-every-branch.md)'s Consequences flagged an
unverified assumption: a branch discovered on source, with no `config.branches` entry,
"shares the same ancestry decisions/0006's graft commit established at setup time...
plausible, but not yet verified against actual code." It doesn't hold for a genuinely
pre-existing, untouched local branch (decisions/0021's own `ai-setup` example) — its
history shares no ancestor with anything `gitprism setup` ever grafted, so
`sync_pair_to_dest`'s no-dest-ref fallback (`newest_dest_marker`, walking source's
first-parent history for a `Gitprism-Dest-Commit` trailer per decisions/0019) finds
nothing.

This decision's first draft treated that as a permanent hard stop, on the strength of
decisions/0023's identically-shaped precedent during `setup` ("no merge-base at all" is
an unconditional hard-fail there) — reported through the same `Reporter` machinery
decisions/0020 built for a real conflict: `reporter.complete(Outcome::Error, ...)`,
`reporter.finish()`, then `anyhow::bail!`.

Real usage surfaced two problems with that:

1. **Doubled, badly-wrapped output.** `run()` wraps every `sync_pair_to_dest` call in
   `.with_context(|| format!("syncing {branch:?} source -> dest"))?` (`sync/mod.rs`). When
   the hard-fail's `anyhow::bail!` propagates through that, `main`'s default `Result`
   printing shows the *exact same detail text* twice: once as the `Reporter`'s own
   colored, scannable line (the entire reason decisions/0020 exists), and again as a
   bare `Error: syncing "ai-setup" source -> dest` / `Caused by:` chain whose wrapped
   continuation lines lose their leading indent in a real terminal. Nothing about the
   second block adds information the first didn't already give more readably.
2. **A stray branch nobody asked gitprism to manage stopped every other branch too.**
   `ai-setup` isn't in `config.branches` — gitprism was never asked to round-trip it;
   only source→dest's decisions/0017 blanket discovery even noticed it exists. Aborting
   the *entire* `sync` invocation over it — including every properly round-tripped
   `config.branches` entry, and every other healthy mirror-only branch — is
   disproportionate to what actually went wrong: nothing is corrupted, no content
   disagrees with anything, there's simply one out-of-scope branch gitprism can't do
   anything useful with yet.

Revisited against decisions/0018's own precedent instead of decisions/0023's: Case 2
there (a mirror-only branch's dest ref goes missing) is *also* "gitprism found something
unexpected about a branch it doesn't own the round trip for," and decisions/0018 already
settled that as a skip-with-note, not a hard stop — the same shape of problem this one
is. decisions/0023's hard-fail precedent is about `setup` reconciling a branch
`config.branches` itself names — an operator directly asked gitprism to manage that
specific branch, a materially different situation from one decisions/0017 merely
stumbled across.

This decision's second draft implemented that by reusing the existing `Outcome::Skipped`
verbatim, reasoning that a fourth `Outcome` variant wasn't yet justified. A real build of
that draft printed the literal word "skipped" for this case — and against decisions/0018's
own "already merged, cleaned up" skip sitting right next to it in the same run, the two
read as the same thing. They aren't: decisions/0018's skip is a genuinely unremarkable
no-op (the branch already exists on dest by another route; nothing for an operator to do).
This case is the opposite — a branch gitprism is refusing to touch, that will keep
reappearing every run until someone acts on it. Reusing one word for both buries the one
that actually calls for attention inside the one that doesn't. This is the real case
`Skipped`'s deferral clause was waiting for.

# Decision

`sync_pair_to_dest`'s `!dest_ref_exists` branch, on finding no `Gitprism-Dest-Commit`
marker anywhere in the branch's first-parent history, is handled the same way
`already_merged_into_a_landing_branch`'s existing skip is structured (`sync/mod.rs` ~line
240-254) — build a note stating the branch has no shared history with anything
`gitprism setup` or a prior sync ever produced, and that combining unrelated histories
is a manual `git merge --allow-unrelated-histories` job if it's ever wanted — but report
it through a new `Outcome::Warning` variant instead of `Outcome::Skipped`:
`reporter.complete(Outcome::Warning, branch, Direction::SourceToDest, round_tripped,
Some(&note))`, then `return Ok(());`. No `anyhow::bail!`, no propagated `Err` — `run()`'s
loop moves on to the next branch exactly as it does for any other completed line.

`Outcome::Warning` renders magenta — a fourth color, distinct from `Outcome::Skipped`'s
yellow — with its own label, `"warning"`, so decisions/0018's genuinely benign skip and
this genuinely-needs-attention case no longer share either a color or a word.
`progress.rs`'s `Outcome::label`/`color` match arms gain a third arm; no other `Outcome`
behavior changes.

The two `config.branches`-only call sites (`dest_tip_is_accounted_for`,
`pending_dest_commits`, via the bailing `newest_dest_marker`) are unchanged: an
operator-named branch missing the trailer really does mean "has `gitprism setup` been
run for this pair," and that hard stop is unaffected by this decision.

# Why

* **Matches decisions/0018's own precedent for a mirror-only branch surprise**, not
  decisions/0023's: a branch gitprism only discovered, never named, deserves the same
  "skip with a visible reason, keep going" treatment Case 2 already established, not a
  full-run hard stop reserved for something an operator explicitly asked gitprism to
  reconcile.
* **decisions/0007's hard-stop-don't-guess policy still applies to the content
  question** (gitprism still refuses to auto-merge unrelated histories, exactly as
  before) **but not to whether the whole run continues** — those are different
  questions; skipping the branch and reporting it loudly every run is not "guessing,"
  it's refusing to guess while still making progress on everything else.
* **Eliminates the doubled/misindented output directly**: no `Err` from this path means
  nothing for `run()`'s `.with_context` wrapping or `main`'s default `Result` printing
  to duplicate — the `Reporter`'s own line is the only place this ever gets said.
* **A silent-forever skip was already rejected in this decision's first draft, and that
  reasoning still holds**: the note prints every run, in the same visible scrollback as
  everything else, so an operator still learns and can still act (rename/remove the
  branch, or deliberately `git merge --allow-unrelated-histories` it) — repeatable
  visibility, not silence, is what decisions/0007's rationale actually requires here.
* **A dedicated `Outcome::Warning` earns its keep now that a real build showed the
  alternative's cost**: this decision's second draft deferred a fourth variant until "a
  real case shows `Skipped`'s existing meaning is actually inadequate" — a build of that
  draft immediately did, printing "skipped" for a branch that will keep recurring every
  run right next to decisions/0018's genuinely one-time, benign "skipped" for an
  already-merged branch. One word covering both hides the one that needs a decision
  inside the one that doesn't.

# Consequences

* **`sync_pair_to_dest` never hard-stops for this specific case any more** — the two
  `config.branches` invariant call sites still do, unchanged.
* **A stray branch is reported every single run** as a magenta warning-with-note line
  until an operator resolves it (merge, rename, or delete it locally) — never silent,
  but never blocking either.
* **`progress.rs` gains a third `Outcome` variant, `Warning`** — its own color, magenta,
  distinct from `Skipped`'s yellow (decisions/0020's green/yellow/red/cyan palette gains
  a fourth entry), label `"warning"`. Every other existing `Skipped` call site
  (decisions/0018's benign case, and any other pre-existing skip) is unchanged and keeps
  printing `"skipped"` in yellow.
* **The `newest_dest_marker`/`newest_dest_marker_opt` split from this decision's first
  draft is unaffected** — `sync_pair_to_dest` still calls the `Option`-returning
  variant; only what happens on `None` changes (skip-and-continue instead of
  error-and-bail).
* **Test coverage**: a `sync_pair_to_dest`-level test asserts `Ok(())` and that nothing
  is pushed to dest for the unrelated branch; a `run()`-level test confirms a
  properly-grafted `config.branches` branch and a genuinely unrelated mirror-only branch
  coexist in one run — the whole run still succeeds (`Ok(())`), the grafted branch's own
  sync proceeds normally, and the unrelated branch is never created on dest. `progress.rs`
  gets its own unit coverage for `Outcome::Warning`'s label and color, matching the
  existing per-variant tests `Done`/`Skipped`/`Error` already have.
* **The general double-print risk this decision found — `run()`'s `.with_context`
  wrapping duplicating a `Reporter`-already-explained hard stop — is not fixed here for
  the paths that still legitimately hard-stop** (a real content conflict; the two
  `config.branches` invariant violations). Those still bail through the same
  `Reporter`-then-`anyhow::bail!` pattern this decision's first draft introduced, and
  will still double-print in a real terminal. Flagged, not fixed, here — it predates
  this decision and affects paths this decision doesn't touch.
