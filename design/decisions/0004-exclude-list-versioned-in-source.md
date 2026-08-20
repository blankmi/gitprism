---
type: Decision
title: Exclude-list is a versioned file committed inside source
description: What must never reach dest is declared in a committed file in source, using gitignore-style patterns, not an external config.
tags: [architecture, filtering, config]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Context

Two shapes were considered: a filter definition committed inside source (josh's
`workspace.josh` precedent — see [references/josh](../references/josh.md)), or an
external config decoupled from both repos (Copybara's workflow-config model — see
[references/copybara](../references/copybara.md)).

# Decision

A committed file inside source (exact filename/format TBD, gitignore-style path
patterns) is the source of truth for what must not reach dest.

# Why

* This project is explicitly scoped to exactly two repositories
  ([requirements/0001](../requirements/0001-workflow-and-scope.md)); an external
  config decoupled from source solves a problem (governing many source/dest pairs
  centrally) this project doesn't have.
* Filtering rules become ordinary, reviewable commits — normal diff/blame/PR history
  applies to "why is this excluded," with no extra location to check.
* Direct precedent in josh's committed workspace file.

# Consequences

* The exclude-list file itself is source-only metadata, so it must be covered by its
  own rule (a versioned filter file that filters out mention of itself) — a detail for
  the concrete filtering-algorithm decision, not resolved here.
* When the exclude-list changes over time, the simple default is: apply the
  *current* list at processing time to whatever commit is being processed, not a
  historical reconstruction of what the list looked like when that commit was made.
  This is simple and consistent with git-notes-free stateless resume
  ([decisions/0003](0003-mapping-state-in-commit-trailers.md)); revisit only if a
  concrete case demands otherwise.

## Security addendum (decision 0026)

The repository-controlled file remains the source of truth, but it is not
trusted merely because it is versioned. Every mutating command authenticates
the exact raw bytes of `.gitprismignore` together with `.gitprism.toml` against
the externally protected `GITPRISM_POLICY_SHA256` value before parsing or
performing Git work. `sync` loads that verified exclude list once per run and
uses it for every source-to-dest branch; it never adopts a branch-tip ignore
file. `gitprism policy-hash` prints the canonical digest for deployment
configuration.
