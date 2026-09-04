---
type: Decision
title: Destination heads are fetched once per run into a transient namespace
description: PERF-001 (docs/2026-09-02_REPOSITORY_REVIEW.md, section 4) — reconstruction and dest→source each open one transport per dest branch, up to MAX_SOURCE_BRANCHES times. A single `git fetch -q --stdin` at the start of `run`, reading one refspec per line for every branch the existing bounded `ls-remote` listing already named, lands every listed dest head under a transient `refs/gitprism/fetched/dest/*` namespace that both call sites then read via git2 instead of fetching. The namespace is cleared before every fetch; nothing reads it across runs; no mapping or marker state lives there, so decisions/0046's "no dedicated refs/gitprism/* mapping refs" constraint is unaffected. Source→dest's own per-branch fetch is unchanged: decisions/0040's force-with-lease needs the dest tip as fetched by that exact push attempt.
tags: [architecture, ci, git, performance]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-09-04T00:00:00Z }
---

# Context

Per run, gitprism opens one transport per dest branch in three places:

| Site | Subprocesses | Purpose |
| --- | --- | --- |
| `anchor::reconstruct_mapping_index` (`src/commands/sync/anchor.rs`), via `fetch_dest_head_for_reconstruction` | 1 fetch per advertised dest head | mapping index (decisions/0046) |
| `sync_pair_from_dest_with_key` (`src/commands/sync/mod.rs`), dest→source | 1 `remote_ref_exists` (`ls-remote --exit-code`) + 1 fetch per configured branch | existence + tip |
| `sync_pair_to_dest_with_key` (`src/commands/sync/mod.rs`), source→dest | 1 fetch per source branch | decisions/0040 force-with-lease (must stay) |

Each fetch has its own 300-second deadline (decisions/0031) and there is no
run-wide deadline. A dest with a few hundred branches — plausible for a
customer- or team-per-branch deployment — makes even a no-op sync take
minutes; at the `MAX_SOURCE_BRANCHES` horizon (decisions/0032, 4,096) roughly
8,000 sequential handshakes are possible in the worst case. Decisions/0046
already describes dest heads as "fetched once"; the first two rows of the
table above do not actually do that.

# Decision

One `git fetch -q --stdin -- <dest_url>` runs at the start of `run`, before
dest→source, reading one refspec line per branch name from stdin:

```text
+refs/heads/<name>:refs/gitprism/fetched/dest/<name>
```

The names are exactly the ones the existing `git::remote_branch_names`
listing (`git ls-remote --heads`) returned, so the fetch is bounded by
`MAX_SOURCE_BRANCHES` (decisions/0032) the same way the per-branch loop it
replaces was. Every name has already passed `validate_branch_name`, so no
line can contain a newline or start with `-`.

Before the fetch, every existing `refs/gitprism/fetched/dest/*` ref is
deleted through git2, not `git fetch --prune` (which only prunes stale refs
that are still matched by a wildcard refspec, not one built from an explicit
name list). Those refs are counted first and the run fails if there are more
than `MAX_SOURCE_BRANCHES` of them — more than the horizon can only mean
something other than gitprism put them there, so cleaning them up
unconditionally would be silently absorbing unexplained state.

Reconstruction (`anchor::reconstruct_mapping_index`) and dest→source
(`sync_pair_from_dest_with_key`) then read each dest tip from
`refs/gitprism/fetched/dest/<name>` via git2 instead of fetching. The
listing itself is unchanged and is kept for two reasons that have nothing to
do with the fetch: it defines decisions/0047's completeness signal (whether
absence of a name can be trusted), and it is the refspec source for the bulk
fetch.

Source→dest's per-branch fetch (`sync_pair_to_dest_with_key`) is kept
exactly as it is: decisions/0040 requires a `ForceMirrorOnly` lease built
from the dest tip as actually fetched by *that* push attempt, including the
race-retry loop's refetch after a `RejectedRefMoved` rejection. A namespace
ref populated once at the start of the run cannot stand in for that —
lease freshness has to come from the fetch immediately preceding the push it
protects.

The namespace is a transient, per-run cache, not durable state: it is
cleared at the start of every run, nothing reads it across runs, and no
marker, mapping, or other durable record is ever stored under it.
Decisions/0046's "no database, Git notes, dedicated `refs/gitprism/*`
mapping refs, developer-side metadata, or persistent CI cache" constraint is
about durable mapping *state* specifically — it says the authenticated
markers already in Git history remain the sole source of truth for mapping.
A same-run fetch cache that is deleted and rebuilt every invocation, and
that nothing ever reads to resume or authenticate anything, does not touch
that constraint; this decision states so explicitly rather than leaving it
to be inferred.

# Why

* **One handshake per run for a fact that's read many times.** Every dest
  head reconstruction and dest→source need is already knowable from one
  `ls-remote` listing; there is no reason each name should separately renegotiate
  a fetch.
* **Git already has the primitive.** `git fetch --stdin` has read one refspec
  per line from stdin since Git 2.29 (October 2020) — well below
  `MIN_GIT_VERSION` (2.45), which gitprism already requires for
  `git merge-tree --write-tree` (decisions/0016). No new Git version
  requirement is introduced.
* **The listing already exists and is already bounded.** `remote_branch_names`
  already enforces `MAX_SOURCE_BRANCHES` (decisions/0032) and already reports
  incompleteness (decisions/0047); this decision reuses both instead of adding
  a second bound or a second completeness signal for the same data.
* **Matches AGENTS.md's "prefer operator intervention over novel
  automation."** Nothing here guesses; it replaces N handshakes for the same,
  already-bounded set of objects with one handshake for the same set.

# Consequences

* The fetch transfers the objects of every listed dest head over one
  transport; the per-branch loop it replaces transferred the same objects
  over many transports. The `MAX_SOURCE_BRANCHES` bound (decisions/0032,
  4,096 heads) is unchanged in kind — this decision changes how many
  transports carry that bounded set, not how large the set may be.
* The local clone gains up to one ref per listed dest branch under
  `refs/gitprism/fetched/dest/*`. `git branch -a` in the operator's own
  checkout does not show them (not under `refs/remotes`), and they are
  deleted at the very start of the next run.
* Dest→source's staleness is unchanged in kind: a dest tip is still read
  exactly once per run and used for the rest of that run, only earlier in
  the run now (right after the listing, rather than immediately before each
  branch's own dest→source turn).
* `sync_pair_from_dest_with_key`'s push-race retry loop no longer refreshes
  the dest tip on each attempt: it used to call `git::fetch` against dest
  again every time around the loop, and now re-reads the same run-start
  namespace ref instead. Only the loop's source refetch (a genuine live
  fetch, needed to recompute against source's actual current tip after a
  lost race) is unaffected by this decision.
* Decisions/0041 ("dest→source fetches its own configured branch when
  absent locally") is now satisfied by reading the namespace ref instead of
  a per-branch fetch; its own behavior (create the local branch from the
  fetched tip when missing) is unaffected — it concerns source's own local
  branch, not dest's.
* Decisions/0046 Addendum 2, Finding K's "the duplicate fetch is the price
  of lease freshness" now names exactly one duplicate: the source→dest lease
  fetch in `sync_pair_to_dest_with_key`. Reconstruction's and dest→source's
  own fetches are no longer duplicates of each other or of anything else —
  both now read the one bulk fetch's namespace.
* `fetch_dest_head_for_reconstruction` and its own list/fetch race handling
  (decisions/0046 Addendum 2, Finding G) are removed: because the bulk fetch
  is all-or-nothing over the whole listed set, a branch listed but deleted
  before the fetch now surfaces as a whole-fetch error before any mutation,
  rather than being individually recovered. This trades away Finding G's
  narrow per-branch recovery for a dest ref genuinely deleted in that window,
  in exchange for one bulk operation instead of `MAX_SOURCE_BRANCHES`
  sequential ones; a future decision may reintroduce a bounded
  refresh-and-retry if that trade proves wrong operationally, but this
  decision does not add one speculatively.

## Rejected alternatives

* **A wildcard `+refs/heads/*:refs/gitprism/fetched/dest/*` refspec.** Not
  bounded by the listing: a dest with more heads than `MAX_SOURCE_BRANCHES`
  would transfer all of them, which decisions/0032 forbids for
  repository-controlled work. The explicit per-name list is what keeps the
  bound in force.
* **Refspecs as command-line arguments instead of stdin.** Windows' argv
  limit (32 KiB) is reachable well below 4,096 branch names, silently
  reintroducing a lower, undocumented bound on a platform gitprism doesn't
  otherwise treat specially.
* **Parsing `FETCH_HEAD` instead of naming a namespace ref per branch.**
  Branch names may contain `'`, and `FETCH_HEAD`'s own format has no bytes-safe
  per-line parse the rest of gitprism could reuse (decisions/0030).
* **Fetching by object id instead of by ref name.** Server-dependent: not
  every Git server advertises or allows fetching arbitrary object ids
  (`uploadpack.allowReachableSHA1InWant`/`allowAnySHA1InWant` are off by
  default), so this would not work against an ordinary dest remote.
