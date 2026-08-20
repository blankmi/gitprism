---
type: Decision
title: Source and dest URLs are optional in .gitprism.toml, falling back to CI env vars
description: Both [source].url and [dest].url may be omitted from the committed config; when omitted, gitprism reads GITPRISM_SOURCE_URL / GITPRISM_DEST_URL from the environment instead. An explicit TOML value still wins when present.
tags: [architecture, config, credentials]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
---

# Context

Implementing dest→source surfaced a gap [decisions/0012](0012-config-versioned-in-source.md)
didn't cover: that direction pushes its result straight to source's own remote
(consistent with [decisions/0008](0008-ship-resolve-helper.md), which has `gitprism
resolve` "perform the ff-only push itself" once a human resolves a conflict — the
normal, non-conflict path must do the same). But `.gitprism.toml`'s schema only ever
named `dest`'s location, never source's — and `.gitprism.toml` is versioned *inside*
source ([decisions/0012](0012-config-versioned-in-source.md)), so anything written
into it is a committed, reviewable, world-readable-to-anyone-with-clone-access value.

That's fine for a plain `https://gitlab.example.com/group/dest.git`-shaped URL, but
real CI remotes are frequently credential-bearing (`https://gitlab-ci-token:$TOKEN@...`)
or simply differ per environment (a staging pipeline pointing at a fork, a
developer's local run pointing at their own remotes). Committing either straight into
source's history is wrong for the same reason secrets don't belong in a git repo at
all, and env vars are exactly what GitLab CI (and CI generally) already provides for
injecting that kind of per-run value.

# Decision

Both `[source].url` and `[dest].url` become optional fields. Resolution order, for
each independently:

1. If the field is present in the committed `.gitprism.toml`, use it as-is.
2. Otherwise, read it from the environment: `GITPRISM_SOURCE_URL` / `GITPRISM_DEST_URL`.
3. If neither is set, fail loudly naming both the missing field and the env var that
   could have supplied it.

This applies uniformly to both repos, not just the newly-discovered `source` case —
`dest`'s existing `[dest].url` field gets the identical treatment, so there's one rule
to remember instead of two.

# Why

* Keeps credential-bearing or environment-specific URLs out of source's committed
  history entirely, while still letting a simple public-repo setup just write the
  literal URL in `.gitprism.toml` if that's all it needs — nothing forces every user
  onto env vars if they don't need them.
* One resolution rule for both fields is simpler to document and implement than
  special-casing `source` (env-only, because it lives in the repo being configured)
  differently from `dest` (TOML-only, because it doesn't) — and the project owner's
  own framing ("same for dest btw") asked for exactly that uniformity.
* Matches ordinary CI practice already in place for basically every other secret
  (deploy keys, tokens): the pipeline injects it as an environment variable, the
  committed config never needs to know the actual value.

# Consequences

* `Config` needs a small resolution helper (TOML value, else env var, else error) used
  identically for both fields, rather than plain `String` fields read directly.
* A config with neither field nor the matching env var set fails at the point that
  URL is actually needed (i.e. per-pair, when a sync direction using it runs) with a
  message naming both the field and the env var — not at parse time, since a config
  missing `dest`'s URL but supplying it via env is completely valid.
* Local/manual runs outside CI need the env var(s) exported by hand if the committed
  config omits the URL(s) — an ergonomics cost accepted deliberately in exchange for
  not committing secrets.

## Implementation-hardening addendum (2026-08-20)

Resolved remote values are never included in gitprism-authored errors. Git
diagnostics redact the exact configured remote before bounded terminal-safe
framing, and network subprocesses set `GIT_TERMINAL_PROMPT=0`; this preserves
credential-helper behavior without hanging unattended runs or echoing URL
credentials.

The committed configuration rejects unknown fields, empty committer identity,
and empty, option-like, or control-bearing explicit URL values. Environment
URLs remain lazy because they are intentionally resolved only when a direction
needs its remote.
