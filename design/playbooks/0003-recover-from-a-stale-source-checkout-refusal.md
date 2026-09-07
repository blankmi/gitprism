---
type: Playbook
title: Recover from a stale-source-checkout refusal
description: Operator steps when sync halts a mirror-only branch because the local tip is not the source remote's tip (decision 0050, CODE-010).
tags: [operations, recovery, branches]
status: stable
---

# Scope

[Decision 0050](../decisions/0050-mirror-only-force-requires-the-local-tip-to-match-the-source-remote.md)
halts a mirror-only branch, before any rebuild or push, when the clone's
local branch tip differs from what the source remote advertises for that
branch. The diagnostic names the branch, the local tip, the source remote's
tip and the prior source boundary. CODE-010 implements this refusal.

A deliberately rewritten source branch is *not* this case. Once the local
checkout is source's tip, gitprism projects the rewrite to dest itself under
decisions 0038/0039/0040. Nothing in this playbook rewrites history.

Pause sync jobs for the affected pair while investigating. Do not push the
raw source branch to dest, forge marker trailers, rerun `setup` to reset
synchronization state, or delete dest's ref to bypass the refusal.

# Local branch is behind source

The common cause: a workstation or reused CI checkout fetched newer source
objects without moving its local branch. Set `branch` to the affected
branch and `source_remote` to the remote name or URL matching the approved
source config. Run these together so nothing else replaces `FETCH_HEAD`
in between.

```sh
git fetch -- "$source_remote" "refs/heads/$branch"
git log --oneline --graph --left-right "refs/heads/$branch...FETCH_HEAD"
git switch -- "$branch"
git merge --ff-only FETCH_HEAD
```

A successful fast-forward moves the local branch to source's tip without
discarding anything. Rerun sync with the approved policy and key; the
branch fast-forwards dest, or, if source was genuinely rewritten, rebuilds
and force-updates it under the lease. A second run must be a no-op.

If `merge --ff-only` refuses, the local branch has commits source does not
— see the next section. Do not replace the merge with a hard reset.

# Local branch is ahead of, or diverged from, source

The checkout carries commits not on the source remote. gitprism will not
project them: they are not source's history yet. Either push them to source
first (an ordinary push; if source rejects it as non-fast-forward, reconcile
with source using normal Git and push again), or move the local branch off
them if they were not meant to exist. Then rerun sync.

A fresh clone of source is always an acceptable alternative when only this
checkout is wrong.

# Source remote has no such branch, or could not be queried

If the branch was deleted or renamed on source, there is nothing for this
clone to project on source's behalf; the halt is correct. Decide what dest's
copy should become as a separate operator action.

If the query itself failed (network, credentials, URL), fix that and rerun.
The failure is reported verbatim; it is never read as "no rewrite".

# Basis and verification

Git's [`merge --ff-only`](https://git-scm.com/docs/git-merge) refuses
anything that is not a fast-forward, so the recovery cannot discard local
commits silently. Decision 0050's condition 5 is a
[`git ls-remote`](https://git-scm.com/docs/git-ls-remote) read of the source
remote's advertised tip. The commands above were executed against a local
bare remote on 2026-09-07; CODE-010's `tests/cli.rs` suite exercises the
stale checkout, the fast-forward recovery and the no-op rerun end to end
through the compiled binary and real bare remotes. This playbook has not
been validated against a hosted production pair.
