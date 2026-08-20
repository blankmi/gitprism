---
type: Decision
title: Protect versioned policy with an external digest
description: Keep .gitprism.toml and .gitprismignore versioned and reviewable, while requiring a deployment-protected digest before any mutating command trusts them.
tags: [security, config, filtering, operations]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

Decisions [0004](0004-exclude-list-versioned-in-source.md) and
[0012](0012-config-versioned-in-source.md) correctly make the policy
reviewable, but a repository author can still change a checked-out branch's
configuration, exclusions, branch list, or remotes. A local Git CLI must treat
that content as untrusted before it performs network or repository mutation.

Copybara's deployment-oriented workflow configuration and josh's committed
workspace file establish the useful split: policy can remain versioned and
reviewable, while deployment controls the trust boundary. gitprism has no
external service or database, so a protected digest is the smallest external
anchor that preserves both properties.

# Decision

`setup`, `sync`, and `resolve` read the exact raw bytes of the selected
`.gitprism.toml` and root `.gitprismignore`, then require
`GITPRISM_POLICY_SHA256` to equal the canonical lowercase SHA-256 of:

```text
gitprism-policy\0 || u64_be(len(config_bytes)) || config_bytes
                 || u64_be(len(ignore_bytes)) || ignore_bytes
```

The digest is domain-separated and length-prefixed. Missing `.gitprismignore`
is represented by a zero-length field. Verification happens before parsing,
Git subprocesses, ref updates, or working-tree mutation; parsing occurs only
after verification. Control files must be regular files, not symlinks.

One `sync` invocation uses one verified `ExcludeList` for every
source-to-dest branch. It does not load branch-tip versions of the ignore file.
`gitprism policy-hash` is read-only, discovers the repository and resolves
`--config` like other commands, and prints the digest without requiring either
`GITPRISM_STATE_KEY` or this expected digest.

URLs supplied through `GITPRISM_SOURCE_URL` and `GITPRISM_DEST_URL` remain
deployment input and are intentionally not part of the file digest.

# Consequences

Deployments must compute `gitprism policy-hash` after checking out the approved
policy and store that value as the protected `GITPRISM_POLICY_SHA256` variable.
Changing either versioned policy file requires an explicit digest update.
Repository-controlled policy can no longer redirect a mutating run merely by
changing a branch tip, and a failed verification leaves Git state untouched.
