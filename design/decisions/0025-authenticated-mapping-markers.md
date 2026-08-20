---
type: Decision
title: Mapping markers are authenticated with a repository-external key
description: Commit trailers remain human-readable mapping state, but only a canonical final HMAC-SHA256 block can advance resume or loop-prevention state.
tags: [architecture, state, mapping, security]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Authenticated mapping markers

## Context

Decision 0003 keeps source/dest mapping in commit trailers, following
Copybara's `GitOrigin-RevId` precedent. A repository author can also write
arbitrary commit messages, so an unauthenticated `Gitprism-*` trailer cannot
be trusted to advance a resume boundary or suppress loop prevention.

## Decision

Keep the human-readable `Gitprism-Source-Commit` and `Gitprism-Dest-Commit`
trailers, but trust mapping state only when the commit ends with one canonical
`Gitprism-State` block. The block contains version, direction, branch,
counterpart OID, and a 64-character hexadecimal HMAC-SHA256. The MAC uses
the external `GITPRISM_STATE_KEY` (exactly 32 bytes, represented by exactly
64 hexadecimal characters) and binds direction, branch, counterpart, parents,
tree, author and committer identities/timestamps, and original message body.
Verification uses constant-time HMAC comparison.

Directional markers are accepted only for the branch named in their signed
block. Setup markers are the deliberate exception: a feature branch cut from
an authenticated setup graft inherits that graft as its shared source/dest
boundary, matching decision 0017's automatic branch mirroring.

Missing or malformed keys fail before repository network or mutation work.
The key is removed from every Git subprocess environment, including hooks and
helpers. Generated messages preserve ordinary prose but strip all reserved
`Gitprism-*` lines before appending the mapping and canonical state block.
Malformed, duplicate, or non-final blocks are treated as ordinary user text.

## Why

Copybara demonstrates that commit trailers are a practical stateless mapping
record, but its trailers are not a secret-authenticated trust boundary for a
hostile repository author. An external secret lets gitprism retain readable,
portable history while making marker copying, reshaping, and forged trailers
fail verification.

## Consequences

Existing unsigned mapping history is not trusted by this implementation and
must be recreated through setup or a signed gitprism run. The key must be
unique to the source/dest pair and available to every sync worker, but never
be committed or exposed to Git hooks. Reusing one key across pairs would let
an authenticated commit object copied wholesale between those pairs retain a
valid MAC. Stripping reserved lines means a user's literal `Gitprism-*`
trailer must be rewritten as ordinary prose if it is intended to remain
explanatory.
