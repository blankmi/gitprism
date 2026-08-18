---
type: Decision
title: source→dest mirrors every branch; dest→source stays an explicit list
description: The two directions no longer share one configured list. source→dest discovers and mirrors every branch on source under its own name, with no per-branch config entry; dest→source keeps an explicit configured list, simplified from {source_branch, dest_branch} pairs to plain branch names now that source→dest never renames. Supersedes decisions/0005's shared-pairs-list model for source→dest's scope.
tags: [architecture, scope, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-17T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

[decisions/0005](0005-branch-pairs-are-a-configured-list.md) has both directions
driven by the same configured list of `{source_branch, dest_branch}` pairs. That
matched the workflow as understood at the time, but doesn't match the actual
development workflow: developers branch off source's main, push the feature branch to
source, get it filtered and synced to dest, finish the work, merge it into dest's
main, and have that merge synced back to source; source's main also takes direct
commits independent of any feature branch.

Feature branches are created ad hoc, with unpredictable names, at a rate a static
config list can't track — every new branch would need a config entry added before it
would sync at all, which is exactly the busywork this decision exists to remove. Only
main (or whatever small set of long-lived branches actually needs it) ever needs
content ported *back*: feature branches are transient and disappear once merged into
dest's main, so mirroring "everything" dest→source as well would mean reflecting
doomed-to-be-discarded feature-branch commits back into source for no purpose — not
something asked for (rule: don't implement solutions for things we don't need).

# Decision

* **source→dest discovers branches at run time** — every branch that exists on
  source is mirrored to a same-named branch on dest, each one filtered
  (decisions/0004, 0011) and merged the same way any pending branch is today
  (decisions/0016). No config entry is needed for a branch to start syncing; no
  `dest_branch` remapping exists any more — the mirrored name is always identical to
  the source name.
* **dest→source keeps an explicit configured list** — but as plain branch names, not
  `{source_branch, dest_branch}` pairs, since there is nothing left to remap. Only
  branches named in this list get their independent dest content ported back to
  source.
* **A branch deleted on source is not deleted on dest.** Decided explicitly in this
  conversation, not inferred: gitprism never deletes branches on either side. A
  merged/abandoned feature branch's mirror on dest is left to sit until an operator
  cleans it up manually — deletion is exactly the destructive, easy-to-get-wrong
  automatic behavior the project's "don't auto-fix, fail loud" rule rules out, and
  nothing in the workflow as described requires it.

# Why

* KISS, applied to config shape: the source branch list isn't fixed, so config
  shouldn't pretend it is. A wildcard/discovery mechanism for source→dest replaces N
  manual config edits with zero.
* Asymmetric scope isn't an inconsistency to unify away — it reflects a real
  asymmetry in the workflow. Every source branch is legitimate dest content once
  filtered; only main (or a short explicit list) is a legitimate *return* path, since
  feature branches are meant to disappear on the dest side once merged.
* Lightweight prior-art check: plain git's own mirroring primitive
  (`refs/heads/*:refs/heads/*`) already treats "every branch, mirrored 1:1 by name" as
  the unremarkable default case for a repo mirror — gitprism doesn't need to invent a
  new mechanism for this, only to enumerate source's branches itself (unlike a raw
  mirror push, each branch's history still has to be filtered and merge-tree'd per
  decisions/0016 before it can be pushed, so a bespoke discovery loop is still
  required). **Not yet checked as rigorously as decisions/0016's merge-engine
  research**: worth confirming against josh's own ref-handling (its proxy applies the
  same filter regardless of which ref is requested, i.e. it doesn't special-case
  enumeration either) before this decision is marked stable.

# Consequences

* **Config schema changes.** `[[pairs]]` (`source_branch`/`dest_branch`) no longer
  describes source→dest at all; dest→source needs a new shape, e.g. a plain list of
  branch names. Exact config surface (key names, TOML shape) isn't settled here — code
  and tests haven't been touched yet.
* **source→dest's loop changes** from "for pair in config.pairs" to "for each branch
  ref discovered on source" — a new branch-enumeration step. Every other part of the
  per-branch logic (filtering, decisions/0016's merge, decisions/0007's hard-stop,
  decisions/0003's trailers) is unchanged; it just runs once per discovered branch
  instead of once per configured pair.
* **A brand-new branch needs no setup step to start mirroring** — the first sync run
  after it's pushed picks it up, since discovery is dynamic. This assumes new branches
  created from source's main share the same ancestry decisions/0006's graft commit
  established at setup time, so the existing revwalk/boundary logic finds a valid
  resume point without special-casing — plausible, but not yet verified against actual
  code.
* **Branch deletion is explicitly out of scope.** dest may accumulate stale mirrored
  branches for merged or abandoned feature branches; that's accepted, not overlooked,
  and only worth revisiting if it becomes a real operational problem.
* **Not decided here**: what happens to a branch deleted on source that still had
  dest→source content pending, or any other deletion-adjacent edge case. Explicitly
  deferred — not something the described workflow currently needs.
* Existing tests and any doc-comments built around symmetric configured pairs (decisions/0005)
  need reshaping for source→dest once implementation starts; dest→source's tests
  mostly still apply, since its per-branch logic doesn't change, only what supplies
  the branch name.
