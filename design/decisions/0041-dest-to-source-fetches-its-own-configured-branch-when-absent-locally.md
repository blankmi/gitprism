---
type: Decision
title: dest→source fetches its own configured branch when it's absent locally, instead of assuming a full checkout
description: sync_pair_from_dest_with_key's first attempt no longer assumes a config.branches entry already exists as a local branch — if it doesn't, gitprism fetches it from source's own remote and creates the local branch itself, the same fetch already used one branch-state later on a push-race retry.
tags: [architecture, ci, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-25T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-25T00:00:00Z }
---

# Context

Surfaced by a real GitLab deployment of [playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md):
source→dest's pipeline is push-triggered per branch, and GitLab CI's default git
strategy only fetches/checks out the single ref that triggered the pipeline — a push
to `feature-x` leaves that job's checkout with no local branch for `develop` at all,
even though `develop` is named in `config.branches`.

`sync_pair_from_dest_with_key` (`src/commands/sync.rs`) resolved a round-tripped
branch's source tip, on its first attempt, purely from the local checkout:

```rust
let source_tip = if attempt == 0 {
    repo.find_branch(branch, git2::BranchType::Local)?...
} else {
    // only a lost push-race retry fetches source's own branch
    ...
};
```

with the assumption stated directly in the removed comment: *"sync always runs
inside a real checkout of source... there's nothing to fetch for it normally."* That
holds for a full clone or a developer's long-lived local checkout, but not for a
single-ref CI clone — which is exactly the shape [playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md)
recommends ("ordinary push-triggered GitLab CI on source's own repo... same as any
other CI job," no special infrastructure called out).

This is not a trigger problem. Playbook 0001's push/manual/schedule triggers for
dest→source are unaffected and don't need a fourth "sync `develop` specifically"
trigger — every run is already supposed to process every `config.branches` entry
regardless of which branch triggered it (decisions/0005, decisions/0017). The gap is
narrower: the code silently assumed the ambient checkout already had every
round-tripped branch present locally, without that ever being a documented
precondition anywhere, and without checking it.

Note the asymmetry this exposed: source→dest doesn't have this problem, because
decisions/0017 already has it discover *whatever's locally checked out*
(`list_source_branches`, a local-only enumeration) — a single-branch CI clone
naturally limits a given run to the branch that was just pushed, which matches
"sync whatever just changed." dest→source's `config.branches` list is different: it's
meant to run for every configured branch on every invocation, not just the one that
triggered the pipeline, so it can't rely on the trigger branch happening to be the
one it needs.

# Decision

`sync_pair_from_dest_with_key`'s first attempt (`attempt == 0`) fetches `branch` from
source's own remote and creates the local branch from the fetched tip whenever
`refs/heads/<branch>` doesn't already exist — instead of only ever doing this on a
push-race retry. Concretely, on `git2::ErrorCode::NotFound` from `find_branch`:

1. Fetch `branch` from `config.source_url()` (same call already used by the retry
   path and by dest→source's own dest fetch).
2. Create `refs/heads/<branch>` at the fetched tip (`repo.reference(..., force:
   false)`), matching what `git fetch origin <branch>:<branch>` would leave behind.
   HEAD is left untouched — no working-tree checkout happens, same as a real `git
   fetch` with a local destination refspec.
3. Use that tip as `source_tip`, same as the pre-existing-local-branch case.

When the local branch already exists, behavior is unchanged (no extra fetch, no
extra network round trip) — this only fires for the branch that was actually
missing.

# Why

* **Git already has the primitive.** Fetching a named branch from an
  already-configured, already-trusted remote (source's own) is exactly what the
  retry path (`attempt > 0`) already does one branch-state later in the same
  function — this closes the gap by using that same call at the point it's actually
  needed, not by inventing new automation. Matches AGENTS.md's "automate only when
  the safe behavior is deterministic and established by Git."
* **No CI reconfiguration required, and none should be.** Which branches round-trip
  is already `config.branches`, versioned in source (decisions/0012). Requiring the
  CI job to separately pre-fetch that same list (option considered and rejected)
  would duplicate that list into `.gitlab-ci.yml`, silently rotting if one changes
  without the other. Making gitprism fetch what it actually needs keeps
  [playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md)'s claim —
  "gitprism itself is trigger-agnostic... it just needs to be invoked" — true for
  the checkout it runs inside too, not just for what triggers it.
* **Consistent with every downstream check, unmodified.** `preflight_local_source_branch`
  and `advance_local_source_branch` both read the local branch via
  `local_source_branch_tip` (`refs/heads/<branch>`), not `source_tip` directly.
  Creating the local ref immediately after the fetch — rather than only holding the
  fetched OID in memory, as the retry path does — means those checks see a real,
  current local branch and need no special-casing for "this branch didn't exist a
  moment ago."
* **No prior art needed beyond what decisions/0002/0013 already established**:
  push/fetch already goes through a real `git` subprocess against a URL resolved the
  same way regardless of caller (decisions/0002, decisions/0013); this decision only
  changes *when* that fetch happens, not how.

# Consequences

* `sync_pair_from_dest_with_key` needs `config` in scope where `source_tip` is
  computed for `attempt == 0` (it already has `config` as a parameter) to call
  `config.source_url()` — previously only the `attempt > 0` arm did.
* A config that omits both `[source].url` and `GITPRISM_SOURCE_URL` (decisions/0013,
  valid as long as a branch never needs to push to source) now also needs that URL
  resolvable the moment a configured branch is *missing locally*, not only when a
  push is actually about to happen. This is judged an acceptable tightening: a
  round-tripped branch absent from the local checkout has no other way to be
  synced at all without source's URL.
* Doesn't change source→dest's behavior or its reliance on whatever's locally
  checked out (decisions/0017) — that direction still only mirrors branches present
  in the ambient checkout on a given run, which remains correct for a single-ref CI
  clone (it mirrors whichever branch was just pushed).
* No change to `playbooks/0001`'s trigger recommendations (push/manual/schedule) —
  this fixes what happens *inside* a run, not what invokes one.
