---
type: Requirements
title: Workflow and scope for gitprism
description: The two-repository sync workflow gitprism must support, as described by the project owner, before any architecture was chosen.
tags: [scope, workflow]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Workflow

Two git repositories are involved, called **source** and **dest** below (names to be
finalized — see open questions).

1. A new **source** repo is set up.
2. Everything currently in **dest** is copied into **source**. After this, source is a
   superset of dest's content: same shared files, plus files/folders that only exist
   in source.
3. Ongoing: changes made on **source** are synced to **dest**, filtered so that
   source-only files/folders never appear in dest. This push to dest must be
   **fast-forward only** — never a force-push — for every branch whose dest-side
   changes flow back into source (step 4's round-tripped branches). Those are
   shared histories, and a non-fast-forward means they diverged: an operator
   reconciles it, gitprism does not. A branch that only ever flows source→dest
   is different: its dest history is a projection of source's, never imported
   back, so it may be force-updated when source's own branch was deliberately
   rewritten. Authority over the history decides, not the direction of the push.
4. Independently, one or more branches on **dest** receive updates (e.g. when PRs are
   merged directly against dest). Those updates must sync **back** into source's
   corresponding branch(es).
5. If filtering a source commit for dest removes all of its changes (because it only
   touched excluded paths), the resulting empty commit must **not** be pushed to dest
   at all.

# Explicit constraints

* No `git-filter-repo`-style workflow: rewriting history and force-pushing dest on
  every sync is unacceptable — dest is treated as shared history other clones depend
  on.
* Exactly two repositories, not an arbitrary N. (This matters for evaluating prior art
  built for a different N.)
* Sync is bidirectional, but asymmetric: source → dest is a filtered subset; dest →
  source is (so far) unfiltered — dest never has source-only content to filter out.
* Implementation language: Rust.

# Open questions (not yet decided)

* ~~What exactly is excluded on the source → dest path~~ — resolved, see
  [decisions/0004](../decisions/0004-exclude-list-versioned-in-source.md).
* ~~Is dest a single repo with one synced branch, or must the tool support several
  branch pairs at once?~~ — resolved, see
  [decisions/0005](../decisions/0005-branch-pairs-are-a-configured-list.md).
* ~~Trigger model~~ — resolved as operational guidance, not a tool decision: see
  [playbooks/0001](../playbooks/0001-gitlab-pipeline-triggers.md). gitprism itself is
  trigger-agnostic by construction (any run is safely re-runnable, per
  [decisions/0003](../decisions/0003-mapping-state-in-commit-trailers.md)); what
  actually invokes it is deployment configuration. The tool running inside a pipeline
  (not an interactive session) is still why [decisions/0007](../decisions/0007-conflict-policy-hard-stop.md)
  and [decisions/0008](../decisions/0008-ship-resolve-helper.md) look the way they do.
* ~~Where does the tool persist the mapping~~ — resolved, see
  [decisions/0003](../decisions/0003-mapping-state-in-commit-trailers.md).
* ~~Build vs. reuse~~ — resolved, see [decisions/0001](../decisions/0001-build-bespoke-rust-tool.md).
* ~~Which git library~~ — resolved, see [decisions/0002](../decisions/0002-hybrid-git-backend.md).
* ~~Author/committer identity on commits gitprism creates~~ — resolved, see
  [decisions/0010](../decisions/0010-preserve-author-stamp-committer.md).
* ~~Exact exclude-list filename/syntax~~ — resolved, see
  [decisions/0011](../decisions/0011-exclude-list-is-gitignore-syntax.md).

# Cited by

* [references/josh](../references/josh.md)
* [references/git-subtree](../references/git-subtree.md)
* [references/git-filter-repo](../references/git-filter-repo.md)
