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

# What could not be confirmed from the docs

The exact mechanics of the **reverse merge** — when a change lands in the filtered
(source-like) repo and gets pushed back, how josh reconstructs the corresponding
monorepo (dest-like) commit if the monorepo side has moved on independently in the
meantime — were not spelled out in the fetched docs beyond "filters are reversible."
This is the specific mechanic our own dest→source direction needs, so it's worth
reading the josh source (`josh-core`) directly rather than trusting docs summaries if
we end up depending on it.

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
