---
type: Reference
title: Copybara
description: Google's tool for moving/transforming code between repositories (e.g. keeping an open-source mirror in sync with an internal monorepo). Relevant for its workflow model and its stateless commit-trailer approach to sync bookkeeping.
resource: https://github.com/google/copybara
tags: [prior-art, google, migration, workflow-model]
sources:
  - { id: repo, resource: "https://github.com/google/copybara", title: "Copybara README" }
  - { id: concepts, resource: "https://copybara.hallucinatedocs.com/getting-started/concepts/", title: "Copybara core concepts" }
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
status: stable
---

# What it is

Google's tool for transforming and moving code between repositories: keeping a public
mirror in sync with an internal codebase, importing upstream changes into a fork, or
one-time code moves with path rewrites and author mapping. Each **workflow** names an
origin, a destination, path filters, and a list of transformations.[^repo]

# Why it's relevant here

Two things transfer directly, independent of Copybara's implementation (Java/JGit,
not something we'd depend on):

1. **A vocabulary for sync direction/shape** that matches our situation almost
   exactly: `SQUASH` (many origin commits → one destination commit — not what we
   want, we want to preserve commits), `ITERATIVE` (each origin commit synced
   individually — this is our source→dest direction), `CHANGE_REQUEST` (import one
   pending change from the *other* side — this is close to our dest→source
   direction, importing merged-PR commits).[^concepts]
2. **Where sync state lives.** Copybara is explicitly *stateless as a service*: it
   stamps every destination commit with a **`GitOrigin-RevId: <origin-sha>`** label in
   the commit message (plus a `--last-rev` override for manual resume), instead of an
   external database. The next run finds the most recent commit carrying that label
   and resumes from the origin commit it names. That means any clone can resume the
   sync correctly just from git history, and multiple people/machines running the same
   workflow can't desync each other.[^concepts]

That second point is a real alternative to josh's approach
([references/josh](josh.md)'s git-notes + local cache) worth weighing directly
against it when we decide where gitprism's own commit-mapping state lives: notes/cache
(josh-style) vs. commit-message trailers (Copybara-style).

# Relevant difference from our problem

Copybara workflows are normally run **on demand / in CI**, producing one commit per
invocation (or per origin commit in ITERATIVE mode) rather than acting as an always-on
sync daemon — closer to the trigger model we're likely to want than josh's proxy is.
It also doesn't claim fast-forward-only as a hard guarantee the way we need; that's
still ours to design.
