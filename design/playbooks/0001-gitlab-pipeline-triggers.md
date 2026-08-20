---
type: Playbook
title: GitLab CI trigger setup for gitprism
description: How to trigger gitprism's two sync directions from GitLab CI. Operational guidance for whoever deploys the tool — gitprism itself is trigger-agnostic; it just needs to be invoked.
tags: [operations, ci, gitlab]
status: draft
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
---

# Why this is a playbook, not a decision

gitprism's own design doesn't care what triggers it — every run is safe to invoke
redundantly, because resuming and no-op detection are already driven by the
trailer-based history scan
([decisions/0003](../decisions/0003-mapping-state-in-commit-trailers.md)), not by
anything trigger-specific. So "what triggers a run" is deployment configuration, not
an architectural fork in the tool itself. This page documents the recommended GitLab
CI setup; it isn't something gitprism's code needs to know about.

# Protected policy variable

Before any job invokes `setup`, `sync`, or `resolve`, check out the approved
source revision and set the protected CI variable `GITPRISM_POLICY_SHA256` to
the output of `gitprism policy-hash`. This variable is a deployment approval,
not a secret. Keep it protected and update it deliberately whenever the
versioned `.gitprism.toml` or root `.gitprismignore` changes. `policy-hash` is
read-only and does not require `GITPRISM_STATE_KEY`.

# source → dest

Ordinary push-triggered GitLab CI on source's own repo. No special infrastructure:
source's `.gitlab-ci.yml` runs a `gitprism sync` job on every push, same as any other
CI job. This is not cross-repo notification — it's the repo reacting to its own
pushes, which GitLab supports natively.

# dest → source

dest can't easily notify source's pipeline (different repo, possibly a different
host/org from source). Rather than treating this as one trigger to pick, combine
GitLab's `rules:` to fire the same job multiple ways:

1. **Piggyback on the push-triggered pipeline.** Every source→dest run also checks
   dest→source, at zero extra config cost — the job already runs, it just does both
   directions' resume-scans.
2. **Manual trigger** (`when: manual`) for "I know a PR just merged on dest, sync it
   now" without waiting for a source push.
3. **Scheduled pipeline** (e.g. hourly) as a staleness backstop: if source goes quiet
   for a while and nobody remembers to trigger manually, a merged dest PR still gets
   picked up within one schedule interval instead of sitting unsynced indefinitely. A
   scheduled run that finds nothing new is cheap and safe by construction (same
   resume-scan as any other run).

Combining these isn't redundant effort — each covers a gap the others leave: push
gives free coverage tied to source activity, manual gives immediacy on demand,
schedule guarantees an upper bound on staleness regardless of either.

# Open

Concrete `.gitlab-ci.yml` job definitions are an implementation detail for later, not
resolved here.
