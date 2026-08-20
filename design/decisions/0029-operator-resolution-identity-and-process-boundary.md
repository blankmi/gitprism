---
type: Decision
title: Operator resolution uses configured identity and keeps Git hooks in the trust boundary
description: Conflict resolution remains fail-fast and operator-driven; temporary Git commits use the configured committer identity while Git's executable, configuration, credential helpers, and hooks remain trusted process boundaries.
tags: [security, conflict-handling, git, identity]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

Decisions 0007, 0008, 0015, and 0027 establish that a real conflict must
stop the affected sync and let an operator edit and stage the result. The
temporary commit made by a real dest-to-source `git cherry-pick` was still
dependent on the machine's Git identity, even though gitprism's final
authenticated commit already uses the configured committer. Git subprocesses
also inherited the URL fallback environment variables after gitprism resolved
them, exposing credential-bearing values to repository-local hooks and helper
processes.

# Decision

`sync` always fails fast when a conflict is found. It never selects a side or
automatically resolves content. The operator uses `gitprism resolve`, edits the
ordinary conflict markers, stages the chosen result with `git add`, and
explicitly continues. The source-to-dest helper likewise only reproduces the
conflict in its isolated linked worktree; it never chooses a resolution.

Every dest-to-source Git command that can create the temporary cherry-pick
commit receives `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL` from the
verified `.gitprism.toml` committer section. Git retains the picked commit's
author. Source-to-dest `git cherry-pick --no-commit` remains identity
independent because gitprism creates the final commit itself through git2.

Every Git child has `GITPRISM_STATE_KEY`, `GITPRISM_SOURCE_URL`, and
`GITPRISM_DEST_URL` removed from its environment. Normal Git authentication
variables, including `GIT_ASKPASS`, remain inherited. gitprism does not attempt
to disable commit hooks or override the Git executable, global configuration,
credential helpers, or repository-local configuration/hooks: those are part
of the operator's trusted Git installation and process boundary. The
configured identity prevents identity drift but is not a sandbox.

Operator command snippets use literal placeholders such as `<branch>`.
Repository-controlled values are displayed separately with debug-safe
rendering, so a branch name containing shell metacharacters never becomes an
executable copy-paste fragment.

# Why

The per-command committer environment is consumed by Git's own commit machinery
and overrides system, global, and local `user.*` settings without changing the
author stamp. Removing only gitprism's resolved values preserves credential
helpers and SSH/askpass behavior needed for ordinary Git transport.
Disabling hooks portably would require changing Git's per-command configuration
and could alter repositories that intentionally depend on hooks or other
transport configuration; it is therefore outside gitprism's isolation model.
Literal snippets provide a small, reliable terminal-safety boundary without
damaging legitimate branch display.

# Consequences

The temporary commit visible during a dest-to-source resolve has the
configured gitprism committer, regardless of machine Git identity. A hook can
still observe and alter the temporary commit because the Git executable,
configuration, and hooks are trusted inputs; deployments that require stronger
isolation must provide a trusted checkout and Git installation or an external
sandbox. Remote URL fallback values are available to gitprism's in-process
configuration resolution but are not leaked to Git children or their hooks.
