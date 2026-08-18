---
type: Decision
title: Setup can adopt an already-existing repo with real independent history, via an explicit --adopt flag
description: A third accepted starting state for `gitprism setup`, alongside decisions/0021's "empty" and "clean clone of dest": a repo whose configured branch already has real, independent history unrelated to dest (a live, already-in-development repo being brought into gitprism for the first time). Requires the operator to pass `--adopt` explicitly — mirroring git's own `--allow-unrelated-histories` gate — and produces a real two-parent merge commit (existing tip first-parent, dest's tip second-parent) via the same `git merge-tree` primitive decisions/0016 already uses elsewhere, hard-stopping on any real content conflict rather than guessing a resolution. Also amends decisions/0021's precondition scan: setup now only inspects branches actually named in `config.branches`; any other local branch is left completely alone rather than blocking the run.
tags: [architecture, setup, bootstrap, merge]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

Running `gitprism setup` against `~/work/catenax-connector` — the project owner's real
GitLab repo, already under active development — surfaced a real gap neither
[decisions/0006](0006-setup-uses-real-shared-history.md) nor
[decisions/0021](0021-setup-accepts-a-clean-clone-of-dest.md) covers. Diagnosis:

* `origin` is source's own GitLab URL, not dest's — this was never a clone of dest.
* The repo has three local branches: `main` (the one configured branch), plus
  `ai-setup` and `backup` — neither in `config.branches`.
* `main`'s tip already carries real, independently-authored commits (production
  pipeline config, CA certificate imports) that predate any involvement with dest.

Under decisions/0021 as implemented, this hard-fails twice over: `ai-setup`/`backup`
aren't in `config.branches` (unconditional hard-fail, independent of `main`'s state
at all), and even if they weren't present, `main`'s tip wouldn't match a fresh fetch
of dest's tip either.

This is a materially different situation from either state setup already recognizes.
Decisions/0006's whole model is that a branch's history on source *begins* as a graft
onto dest's tip — dest's tip as the commit's sole parent. That has no way to express
"this history already existed independently before dest was ever involved" — there is
no single parent that is simultaneously "whatever came before" and "dest's tip."
Confirmed directly with the project owner: this repo's GitLab remote is itself empty
(no risk to already-published history), `main` is meant to become gitprism's actual
source going forward — not discarded in favor of a fresh graft — and `ai-setup`/
`backup` should be left alone entirely, never required to appear in `config.branches`.

This is exactly `git subtree add`'s primary use case
([references/git-subtree](../references/git-subtree.md)): merging an external
history into an *already-existing* repo, as a real merge commit with two parents,
not decisions/0006's single-parent graft into emptiness. It's also exactly the
situation `git merge` itself refuses without an explicit flag: two histories with no
common ancestor. `git merge` requires `--allow-unrelated-histories` before it will
even attempt this — real, directly-applicable prior art for gating this behind an
explicit opt-in rather than auto-detecting "this looks like adoption."

The project owner's own answers, obtained in conversation before drafting this:

* Conflict handling: **hard-stop on any real conflict** — matching
  [decisions/0007](0007-conflict-policy-hard-stop.md)'s existing policy everywhere
  else in this tool, not `-X ours`/`-X theirs` auto-resolution.
* Other local branches (`ai-setup`, `backup`): **leave them alone entirely** — they
  never need to appear in `config.branches`, and their mere existence must not block
  `setup` from running against `main`.

# Decision

**1. Amend decisions/0021's precondition scan.** Setup no longer enumerates *every*
local branch and requires each to be in `config.branches`. It now looks up only the
branches named in `config.branches`, one at a time — exactly the set setup already
needs to fetch dest's tip for. Any other local branch in the repository is invisible
to setup: never inspected, never a reason to fail. (The "detached HEAD" check from
decisions/0021 is dropped along with it — it existed only to backstop the same
whole-repo sweep this amendment removes; the real protection now lives entirely in
each configured branch's own per-branch check below.)

**2. A configured branch whose local tip doesn't match dest's freshly-fetched tip is
still a hard-fail by default** — decisions/0021's existing behavior is unchanged for
the common case. It only proceeds as an *adopt* if the operator passes a new
`--adopt` flag on the `gitprism setup` invocation (global to the run, not per-branch
— matching how `config` itself is a whole-run flag, not a per-branch one).

**3. With `--adopt`, a diverging configured branch is merged with dest via the same
`git merge-tree --write-tree` subprocess primitive decisions/0016 already established
for both sync directions** — one merge engine in this codebase, not two:
   * `base` = git's canonical empty tree (there is, by construction, no common
     ancestor between two histories that were independently authored).
   * `ours` = the branch's own current tree (its real, pre-existing content).
   * `theirs` = dest's freshly-fetched tip's tree.
   * **Clean** → the resulting merged tree gets `.gitprism.toml`/`.gitprismignore`
     inserted into it exactly the way `graft_branch` already does today, then
     committed with **two parents**: the branch's own previous tip first (ordinary
     git convention for "merge X into the branch you're on" — the branch keeps its
     own identity as mainline), dest's tip second. The same `Gitprism-Dest-Commit`
     trailer is stamped, naming dest's tip — `sync`'s dest→source resume-scan needs
     nothing else to recognize this as reflecting dest's state at adoption time.
   * **Conflict** → hard-stop, per decisions/0007, naming the conflicted paths — no
     `-X ours`/`-X theirs` fallback, matching the project owner's own answer above.

**4. Adopt only ever operates on the branch currently checked out.** `--adopt`
requires source's `HEAD` to already be attached to the exact branch being adopted;
any other configured branch that also needs adopting is a separate, later
`git checkout <branch> && gitprism setup --adopt` invocation. Setup never switches
`HEAD` or touches the working tree of a branch other than the one already checked
out — this is a live, already-in-use repo, not an empty one setup is free to
materialize onto.

**5. No forced checkout, and `HEAD` is never moved, in the adopt path.** Once the
merge commit lands, setup does a plain, non-forced `checkout_head` refresh (same
"never overwrite a real conflicting untracked file" rule as every other checkout in
this file) to bring any dest-only files into the working directory — nothing more.

# Why

* **Matches real prior art for exactly this shape twice over**: `git subtree add`'s
  actual primary use case is merging external history into an already-existing repo,
  and `git merge --allow-unrelated-histories` is git's own precedent for gating a
  no-common-ancestor merge behind an explicit flag rather than inferring intent.
  Neither is invented for this decision — both were already checked into this
  project's own prior-art references before recommending this shape.
* **One merge engine, not two.** Reusing `git::merge_tree` (decisions/0016) rather
  than building bespoke adopt-specific merge logic means adopt's conflict behavior,
  git-version floor, and object-database-only operation (no working-tree requirement
  for the merge computation itself) all come from a mechanism this codebase already
  tests and relies on elsewhere.
* **Explicit opt-in over auto-detection** is the safer default for an operation this
  consequential: silently deciding "this looks like adoption, let's merge" on an
  unexpected repo state (e.g. genuinely the wrong directory) is a worse failure mode
  than requiring the operator to say so. This mirrors decisions/0007's own
  hard-stop-over-guessing philosophy one level up — applied to *whether to attempt
  the operation at all*, not just to how a resulting conflict is handled.
* **Restricting the amended precondition scan to only `config.branches`, and adopt to
  only the checked-out branch**, keeps this decision's blast radius to exactly what
  the project owner asked for: `ai-setup`/`backup` genuinely never need to exist in
  gitprism's world, and a live working directory's other branches are never at risk
  of an unexpected `HEAD` move or checkout.
* **First-parent = the branch's own previous tip** matches ordinary `git merge`
  convention (merging something *into* the branch you're on keeps that branch as
  mainline) and costs nothing for decisions/0019's first-parent-only marker scans:
  they only need to reach the adopt commit itself via first-parent from source's
  future tip, which they always will, since the adopt commit becomes that branch's
  real new tip going forward — dest's own historical commits (reachable only via the
  second parent) were never something those scans needed to see.

# Consequences

* **New CLI flag**: `gitprism setup --adopt` (boolean, defaults to off). Passing it
  when every configured branch is either absent locally or already matches dest's
  tip is a no-op difference from not passing it at all — the flag only changes
  behavior for a branch that's genuinely diverging.
* **`setup.rs`'s precondition scan shrinks**: no more whole-repo branch enumeration
  or detached-HEAD check (decisions/0021's additions to it); each configured branch
  is looked up individually, exactly where its dest-tip fetch already happens.
* **A new merge-commit code path alongside the existing single-parent graft path** —
  `graft_branch` gains a sibling (or an internal branch) that takes an `ours` tree,
  runs it through `merge_tree`, and commits with two parents on `Clean`, propagating
  a `Conflict` as a hard failure naming the paths.
* **`ensure_merge_tree_supported`'s git-version floor (decisions/0016) now also
  applies to `setup`**, not just `sync` — `--adopt` needs the same `git merge-tree
  --write-tree` support. Worth checking at the top of `run()` when `--adopt` is
  passed, the same way `sync` already does, rather than surfacing as a confusing
  parse failure partway through.
* **Not decided here — deferred as a real open question**: what an operator does
  after `--adopt` hard-stops on a conflict. There is no `gitprism resolve`-equivalent
  for this one-time bootstrap case today (decisions/0008/0015 built that machinery
  specifically for sync's *recurring* conflict, which justifies its `--continue`
  mechanism in a way a once-ever adoption may not). For now, the hard-stop message
  needs to name the conflicted paths and point at manually completing a real
  `git merge --allow-unrelated-histories <dest-tip>`, resolving conflicts by hand,
  and committing with a `Gitprism-Dest-Commit: <dest-tip-oid>` trailer themselves —
  every other command only depends on that trailer's presence and value, not on how
  the commit was produced. Whether that manual path is good enough in practice, or
  whether adopt needs its own resolve/continue helper, is left for real usage to
  surface rather than built speculatively now.
* **`main`'s own pre-existing history (everything before the adopt commit) has no
  ancestral relationship to dest's pre-adoption history** — expected and unavoidable
  given the two were genuinely developed independently; decisions/0006's "real
  shared ancestry" property holds from the adopt commit forward, not retroactively.
