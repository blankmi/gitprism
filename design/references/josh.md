---
type: Reference
title: josh — Just One Single History
description: A Rust git-virtualization proxy that does incremental, reversible, bidirectional history filtering without rewriting the underlying repos.
resource: https://github.com/josh-project/josh
tags: [prior-art, rust, git, monorepo, filtering]
sources:
  - { id: readme, resource: "https://github.com/josh-project/josh/blob/master/README.md", title: "josh README" }
  - { id: intro, resource: "https://josh-project.dev/docs/intro.html", title: "josh Introduction" }
  - { id: faq, resource: "https://josh-project.github.io/josh/faq.html", title: "josh FAQ" }
  - { id: deepwiki, resource: "https://deepwiki.com/josh-project/josh", title: "josh DeepWiki (generated architecture overview)" }
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
status: stable
---

# What it is

josh is a git virtualization proxy, written in Rust, that lets you treat a
subdirectory or an arbitrary composition of paths inside a monorepo as its own
independent-looking git repository, kept in sync with the monorepo in both
directions — without ever rewriting the monorepo's real history.[^readme]

# Why it's relevant here

It solves almost exactly the property the project owner ruled out `git-filter-repo`
for: filtering is **non-destructive and reversible**, so it never requires a
force-push to keep the filtered view current.[^intro] It's also the only prior art
found that is bidirectional by design rather than as an afterthought.

# How it works, as far as could be confirmed

* Filtering happens on the git object graph, not the working tree — josh builds an
  **alternate history with no reference to the skipped parts**, similar in spirit to
  `git filter-branch`/`git-filter-repo` but incremental instead of one-shot.[^faq]
* It keeps a **persistent cache of filtered-commit mappings**, stored via a local
  `sled` database plus **git notes** for sharing the cache across clones. This is the
  concrete mechanism behind "reversible": the mapping from a filtered commit back to
  its originating monorepo commit is looked up, not recomputed from scratch, and
  because it's content-derived the same input always maps to the same output.[^deepwiki]
* Because of the cache, re-filtering after new commits is proportional to what's new,
  not to the whole history — this is what makes it usable as an always-on proxy
  rather than a batch job.[^intro]
* Filters are restricted to a small DSL (not arbitrary scripts) specifically so they
  compose and stay reversible; a "workspace" is a named, versioned filter checked into
  the repo itself.[^intro]
* User-facing shape: `josh-proxy` is an HTTP service that intercepts ordinary git
  fetch/push and applies the filter transparently, so any git client can clone/push a
  "virtual" filtered repo as if it were real. `josh` / `josh-filter` are CLIs for
  local, one-shot filtering without running the proxy.[^readme]

# The reverse merge, confirmed from source

This was previously an open question here — the docs said only "filters are
reversible." Read directly from josh's source when
[decisions/0016](../decisions/0016-both-directions-merge-via-real-git-merge-tree.md)
came to depend on it:

* [`unapply_filter` in `josh-core/src/history.rs`](https://github.com/josh-project/josh/blob/2ac70eab2710ef9302fe999e92b6b7cf961f4e2e/josh-core/src/history.rs#L378-L655)
  is the reverse-apply. It is **not** patch application. Where it can't pick a single
  parent tree it performs two `git2::Repository::merge_commits` calls — one with
  `FileFavor::Ours`, one with `FileFavor::Theirs` — and proceeds only if both
  resulting trees agree, on the source's own stated reasoning that conflicts should
  only occur in paths present in the filtered commit.
* When that agreement check fails, josh **hard-errors** rather than guessing:
  `return Err(anyhow!("rejecting merge with {} parents..."))`, with a maintainer
  comment about considering "a manual override as last resort." Same operator-first
  policy as [decisions/0007](../decisions/0007-conflict-policy-hard-stop.md).
* Idempotency does not come from the merge alone: `josh-core/src/trailers.rs` extracts
  a stable `change-id` (native commit header, falling back to `Change:`/`Change-Id:`
  trailers) which, with the `sled`-backed mapping cache, is what stops an
  already-mapped commit being reprocessed — the same division of labour as
  [decisions/0003](../decisions/0003-mapping-state-in-commit-trailers.md).
* The merge path needs no working tree: `git2`'s `merge_commits`/`merge_trees` produce
  an in-memory index, and checkout is optional. This is structurally forced for josh,
  since `josh-proxy` is a headless async server speaking the git protocol against bare
  mirrors.
* Known failure reports in exactly this reconciliation path, worth designing around:
  [#998 "rejecting merge with 2 parents..."](https://github.com/josh-project/josh/issues/998)
  (hit by the rustc↔miri subtree sync; whether it was ever resolved could not be
  confirmed), [#1325 "Pushing to josh produces non-roundtrip commit"](https://github.com/josh-project/josh/issues/1325)
  (same tree, different history — the reporter stopped syncing rather than risk
  duplicating history), [#1583](https://github.com/josh-project/josh/issues/1583) and
  [#952 "Josh generates lots of redundant merge commits"](https://github.com/josh-project/josh/issues/952).

Still unconfirmed: a repo-wide search for `git` subprocess use in josh was not
possible (GitHub code search requires login), so "libgit2 only, never shells out"
remains inferred from the dependency set and from every merge call found going through
`git2`, rather than proven.

# Relevant difference from our problem

josh is built for **N virtual repos carved out of one monorepo** (arbitrary
composition, workspaces, a whole proxy service). Our problem is narrower: exactly two
repos, one exclude-list, no proxy required. That gap is the crux of the first open
decision — see [requirements/0001](../requirements/0001-workflow-and-scope.md).

# What it uses internally (git library choice)

josh's `Cargo.toml` depends on both `git2` (libgit2 bindings) *and* several low-level
`gix` crates (`gix-object`, `gix-hash`, `gix-actor`, `gix-config`, `gix-submodule`) —
it never fully adopted gix's high-level API. Public discussion from the maintainers
notes that fuller gix integration was "somewhat difficult," particularly in an async
context — relevant context, since `josh-proxy` is an async HTTP server implementing
the git smart protocol itself, a different shape of program than a batch/CLI sync
tool. See [decisions/0002](../decisions/0002-hybrid-git-backend.md).

[^readme]: [^intro]: [^faq]: [^deepwiki]: see `sources` above.
