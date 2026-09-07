# Plan CODE-010 — a stale source checkout must not authorize a mirror rewrite

| | |
| --- | --- |
| Finding | September 7 follow-up in `docs/2026-09-02_REPOSITORY_REVIEW.md`, CODE-010 (conversational CODE-001) |
| Severity / priority | HIGH / P0 |
| Impact / effort | High / Medium |
| Decision required | Accepted in decision 0050: 0039's rewrite detection gains a fifth condition, the local tip must equal the source remote's advertised tip |
| Depends on | No unfinished implementation task |
| Status | Implemented; 412 tests passing (408 unit + 4 `tests/cli.rs`) |

## Evidence and scope

At `f1794e5`, `mirror_only_rewrite_detected` in `sync/anchor.rs:492` returns
`!graph_descendant_of(source_tip, boundary)` once the boundary object exists.
`source_tip` is the clone's local branch. Fetching S2 into a stale S1 clone
without pulling makes an ordinary behind branch satisfy that predicate, and
`sync_pair_to_dest_with_key`'s `ForceMirrorOnly` arm (`sync/mod.rs:649-693`)
force-updates dest from S2's projection to S1's. The August 27 F-01 fix only
covers a boundary object this clone never fetched.

Decision 0050 keeps mirror-only branches source-authoritative (requirements
0001 step 3, decisions 0038/0039/0040) and closes the hole by asking the
source remote whether the local tip is its tip before any force. An earlier
halt-everything draft was withdrawn; see 0050's Context.

## 1. Regression first

In `sync/tests/anchor.rs`, extend the F-01 stale-clone fixture: mirror S1,
clone source, advance source to S2 and mirror it from the original clone,
`git fetch` S2 into the stale clone without moving its local branch, sync
from the stale clone. Assert: dest's ref and content are unchanged, the
branch reports `Outcome::Error` naming the local and source-remote tips, the
run exits nonzero, and no "source branch was rewritten" step is reported.
This test fails at `f1794e5`.

Add a `tests/cli.rs` fixture with real bare source and dest remotes for the
same shape, since the local and remote source tips must be distinct objects
the binary can only tell apart by asking the remote.

Keep every existing rewrite test green with its current expectation: a
genuine reset, an amended tip, a rebased tip and reset-to-graft still rebuild
and force under the lease when the local tip equals source's tip — which the
existing fixtures satisfy, because they rewrite the clone that pushes to
source. Keep the missing-boundary-object, round-tripped-refusal and
rejected-lease tests as they are.

## 2. `git` helper

Add a sibling of `git::remote_ref_exists` that returns the advertised OID
for `refs/heads/<branch>` on a URL: same `ls-remote --exit-code -- <url>
<ref>` shape, same `validate_remote`/`validate_branch_name`, same
`SMALL_OUTPUT` capture through the standard runner (decision 0031). Return
`Some(oid)` on exit 0 with a parseable 40/64-hex first field, `None` on exit
2, `Err` otherwise — including unparseable output. Unit-test all three
against a local bare remote and a malformed-output stub if the existing
runner test seam allows one; otherwise cover exit 0 and exit 2 for real and
the parse failure through the parser alone.

## 3. Condition 5 at the force site

In the `None if !round_tripped && mirror_only_rewrite_detected(...)` arm,
before building the rebuild base, read `config.source_url()` (decision 0013)
and the source remote's tip through the new helper:

* equal to `source_tip` → proceed exactly as today (rebuild, lease, push);
* different, `None`, or `Err` → `reporter.complete(Outcome::Error, ...)` and
  `return Ok(true)` (decision 0045's per-branch halt), with a message that
  names the branch, local tip, source-remote tip (or "no such branch" / the
  error), the prior boundary, and playbook 0003. Do not reuse
  `unsafe_to_build_on_message`; this halt has a different cause.

The query runs inside the retry loop so a `RejectedRefMoved` retry
re-evaluates it with the refetched dest tip. Do not cache it across
attempts or across branches. Leave the fast-forward path and every
round-tripped path untouched; condition 5 must be unreachable from them.

## 4. Coverage the decision requires

Through real remotes: stale-fetched-not-pulled clone halts; genuine reset and
genuine rebase with local == remote force under lease; local tip ahead of
source halts; branch deleted on source halts; unreachable source URL halts
with the error named; stale-checkout recovery — `git merge --ff-only` from
source, rerun syncs, second rerun is a no-op. Assert per-branch isolation:
another mirror-only branch in the same run still syncs.

## 5. Documentation

Update `mirror_only_rewrite_detected`'s and the force arm's doc comments to
name condition 5 and decision 0050. Record the test count in `design/log.md`.
Update this plan's Status row, the README status table and the review's
status section.

## Acceptance

A stale local branch with the newer source object present cannot replace a
newer dest projection: it halts, dest is unchanged, exit is nonzero. A local
tip that equals source's advertised tip and does not descend from the prior
boundary still rebuilds and force-updates dest under decision 0040's lease.
All release gates pass on stable and MSRV 1.89.
