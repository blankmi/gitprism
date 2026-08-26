---
type: Decision
title: .gitprism.toml drops out of the per-commit policy-mismatch check
description: Amends decisions/0037 — only .gitprismignore is compared per pending commit going forward; .gitprism.toml keeps decisions/0026's single global verification and is never re-checked per branch or per commit.
tags: [security, filtering, config, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-26T00:00:00Z }
---

# Context

A real deployment hit this: an operator updated `.gitprism.toml` on a branch and
re-pinned `GITPRISM_POLICY_SHA256` via `gitprism policy-hash`, then `sync` still
halted — an earlier, still-pending commit on the same branch carried a previous
`.gitprism.toml` version. [decisions/0037](0037-branch-policy-mismatch-fails-closed.md)
checks every commit `pending_commits(boundary, source_tip)` would replay, not just
the branch tip, so any not-yet-synced commit that ever touched the config file
differently from what's now pinned halts the branch — including commits made purely
while iterating on the config before the first successful sync
([decisions/0022](0022-setup-gains-an-interactive-first-run-wizard.md)'s wizard, or
manual first-run setup). Reconciling that via a history rewrite (rebase/squash) for
every such commit is disproportionate for a file with no content-disclosure vector.

[decisions/0036](0036-branch-additive-exclusions.md)'s Context — which 0037 was
written to close — is specifically about `.gitprismignore`: a commit can add
sensitive content to source's tree before a later commit adds the exclusion for it;
source→dest replay filters *that specific commit's* snapshot with the one pinned
exclude list ([decisions/0026](0026-protected-versioned-policy.md)), so if the
earlier commit's own (differing, more permissive) `.gitprismignore` were trusted or
silently ignored, the content leaks the moment that commit is replayed — not caught
by checking the branch tip alone. `.gitprism.toml` has no equivalent path: it's
self-excluded from dest ([decisions/0012](0012-config-versioned-in-source.md)) — its
own bytes never reach dest regardless of version — and it doesn't gate what content
gets filtered out of any commit (that's `.gitprismignore`'s job); it holds dest's
location, the branch-pairs list, and committer identity. An earlier pending commit's
stale `.gitprism.toml` can't leak anything by being replayed.

# Decision

`find_control_file_policy_mismatch` (`src/commands/sync.rs`) now compares only
`.gitprismignore` per pending commit. `.gitprism.toml` is removed from that
per-commit loop entirely. It keeps exactly the check it already had —
decisions/0026's single, global, digest-verified read before either sync phase
runs, once per `sync` invocation, never per branch or per commit.

# Why

* The disclosure argument 0037 relies on (0036's Context) is about content gated by
  `.gitprismignore` specifically; it doesn't transfer to `.gitprism.toml`, which is
  self-excluded and doesn't filter anything.
* decisions/0026 already states the intended model directly: "one verified
  `ExcludeList` for every source-to-dest branch... does not load branch-tip
  versions." 0037's per-commit `.gitprism.toml` check was, in effect, a second,
  redundant policy source layered on top of that for a file with no per-commit
  relevance at all.
* Matches `AGENTS.md`'s own rule: automation (here, an added safety check) is
  justified only when it answers a real, deterministic risk. A check with no
  matching threat is exactly the kind of thing to remove, not carry forward out of
  caution — the friction it causes (forced history rewrites during ordinary config
  iteration) is a real cost with nothing on the other side of the ledger.

# Consequences

* An operator iterating on `.gitprism.toml` before the first successful sync — the
  sanctioned setup workflow (decisions/0022) — no longer needs to rebase away
  intermediate config versions; only the working tree's current bytes need to match
  `GITPRISM_POLICY_SHA256` when `sync` runs.
* `.gitprismignore` is unaffected: still checked per pending commit, unchanged from
  decisions/0037.
* `find_control_file_policy_mismatch` no longer takes a `config_raw` parameter;
  `sync_pair_to_dest_with_key` and its test-only wrapper `sync_pair_to_dest` drop it
  too. `VerifiedPolicy.config_raw` (`src/policy.rs`) is untouched — decisions/0026's
  digest computation still needs it.
* `PolicyMismatch`/`PolicyMismatchReason`/`policy_mismatch_message` are unchanged in
  shape; they just never get constructed with `crate::config::FILENAME` at runtime
  anymore.
* Test `sync_pair_to_dest_halts_a_branch_whose_replayed_commit_has_a_differing_gitprism_toml`
  replaced by `sync_pair_to_dest_replays_a_commit_with_a_differing_gitprism_toml_normally`,
  asserting the branch does not halt and ordinary content alongside the differing
  config still reaches dest.

# Prior art

None re-checked beyond 0036/0037's own findings — this narrows an existing
decision's scope rather than introducing new filtering behavior.
