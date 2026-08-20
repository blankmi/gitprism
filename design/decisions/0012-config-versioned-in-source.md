---
type: Decision
title: Config is versioned in source, as .gitprism.toml, self-excluding like .gitprismignore
description: Repo locations, branch pairs, and committer identity live in a committed TOML file in source, not an external/deployment file, following the same self-exclusion convention as .gitprismignore.
tags: [architecture, config]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-14T00:00:00Z }
  - { by: "human:michael.blank@evia.de", at: 2026-08-14T01:00:00Z }
---

# Context

gitprism needs somewhere to hold dest's location, the `{source_branch, dest_branch}`
pair list ([decisions/0005](0005-branch-pairs-are-a-configured-list.md)), and
gitprism's own committer identity
([decisions/0010](0010-preserve-author-stamp-committer.md)). This is structurally the
same question [decisions/0004](0004-exclude-list-versioned-in-source.md) already
answered for the exclude-list: versioned inside source, or an external/deployment
file (the shape [playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md)
deliberately uses for CI trigger config, since that really is deployment-specific and
outside gitprism's own architecture).

The project owner's answer: config lives in source git, same as the CI pipeline
definition does — both are ordinary files in source's tree. The difference between
them is that gitprism *knows about its own config* and must keep it from ever
reaching dest by default, the same way
[decisions/0011](0011-exclude-list-is-gitignore-syntax.md) made `.gitprismignore`
self-excluding. gitprism has no comparable knowledge of a CI file's name or shape
(consistent with playbooks/0001 — gitprism is trigger-agnostic by construction), so
the CI file is not auto-excluded; the user adds it to `.gitprismignore` themselves,
same as any other source-only file.

This does surface one wrinkle `.gitprismignore` didn't have:
[decisions/0006](0006-setup-uses-real-shared-history.md) makes `setup` the operation
that creates source's *first* commit. There is no committed history for a versioned
config to live in yet at the moment `setup` needs to read it — but, same as `git`
itself never auto-`init`s a repository for any other command, that's a gap in
*history*, not in the repository's existence: the user runs `git init` (or already
has) exactly once, same precondition any other git command has, and `setup` reads
`.gitprism.toml` off disk into that empty repo, not into thin air.

# Decision

* **Location**: a committed file at source's root, `.gitprism.toml` — same
  `.gitprism*` namespace as `.gitprismignore`, both read as gitprism control files at
  a glance.
* **Format**: TOML, following Rust/Cargo ecosystem convention (`Cargo.toml`,
  `rustfmt.toml`, `clippy.toml`) rather than introducing a format with no precedent in
  this project's own toolchain. `serde` + `toml` also model the pairs list
  (`[[pairs]]`) cleanly.
* **Self-exclusion is automatic**: `.gitprism.toml` is always excluded from
  source→dest filtering, without needing to be listed in `.gitprismignore` — same
  convention as `.gitprismignore` itself
  ([decisions/0011](0011-exclude-list-is-gitignore-syntax.md)'s "Consequences"
  flagged this exact case: future gitprism control files should follow suit).
* **Bootstrap sequencing for `setup`**: the user has already run `git init` in
  source's directory — gitprism itself never creates a git repository, matching
  `git`'s own convention of erroring rather than silently initializing one outside a
  repo — and writes `.gitprism.toml` locally, uncommitted, since there's no committed
  history yet to have put it in. `gitprism setup --config <path>` reads it straight
  off disk, fetches dest, and creates source's first commit with dest's tree plus
  `.gitprism.toml` and `.gitprismignore`, parented on dest's tip. Every subsequent
  command reads the config from source's committed tree like any other versioned
  file — no separate "first run" config path to maintain past that one command.
  Like `git` itself, gitprism takes no separate source-location config or flag:
  `setup` discovers source's repo by walking upward from the current directory,
  exactly as `git` would, and a relative `--config` path resolves against that
  discovered root, not against whatever subdirectory it was invoked from.
* **Schema** (fields only; exact TOML shape is an implementation detail, not a
  design fork):
  * committer identity: name, email (decisions/0010)
  * dest's location (path or remote URL)
  * the branch-pair list (decisions/0005)

# Why

* Consistent with this project's existing precedent
  ([decisions/0004](0004-exclude-list-versioned-in-source.md)): reviewable commits,
  no extra location to check, no drift between "what's configured" and "what history
  says happened."
* The project owner's own framing settles the location question directly: the CI
  pipeline file already lives in source as an ordinary file; config should too. The
  only special handling gitprism needs to add is excluding *its own* files from the
  dest-ward filter — not treating config as architecturally different from any other
  source file.
* TOML avoids introducing a second config-format convention into a project that
  already has one implicit precedent (Cargo's own TOML) and no reason to diverge from
  it.

# Consequences

* `gitprism setup` takes a `--config` flag pointing at a local, not-yet-committed
  file — the one command whose config source is "the filesystem" rather than "the
  currently checked-out tree." Every other command's `--config` default
  (`.gitprism.toml`) resolves against the checked-out source tree.
* If dest's location or the committer identity ever needs to change, that's an
  ordinary commit to `.gitprism.toml` in source — reviewable like any other config
  change, not a redeploy of something external.
* `.gitprism.toml` never needs listing inside `.gitprismignore` itself, same
  guarantee `.gitprismignore` already gives itself.
* `gitprism setup` requires an existing, completely empty repository (no commits, no
  branches) at or above where it's invoked — it errors loudly like any other git
  command run outside a repository, rather than auto-`init`-ing one, and it refuses to
  run against a repo that already has history rather than grafting on top of or
  checking out over whatever's already there.

## Security addendum (decision 0026)

Versioning the configuration does not make it trusted input. Before `setup`,
`sync`, or `resolve` parses or acts on the file, gitprism authenticates its
exact bytes together with the root `.gitprismignore` bytes using the externally
protected `GITPRISM_POLICY_SHA256` digest. A missing, malformed, or mismatched
digest stops the command before Git subprocesses, ref updates, or working-tree
changes. Symlinked or non-regular control files are rejected. Environment URL
fallbacks remain deployment input and are deliberately outside this digest.
