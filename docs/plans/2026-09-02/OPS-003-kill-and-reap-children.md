# Plan OPS-003 — timed-out `git` children (withdrawn)

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 5, informational OPS-003 |
| Severity / priority | INFO / P3 |
| Effort | — |
| Decision required | No |
| Depends on | — |
| Status | **Withdrawn 2026-09-02** — the finding rests on a wrong model of the runner |

## What the review claimed

`kill_and_reap` (`src/git.rs:473`) kills the `git` process but not its
descendants, and the capture threads block on the inherited pipes until every
writer exits, so a hung `ssh` could hold the runner past the deadline.

## What the code does

* `spawn_capture` (`src/git.rs:436-464`) discards the `JoinHandle`s. Nothing
  ever joins the capture threads.
* On deadline expiry (`src/git.rs:417-423`) the runner kills and reaps the
  direct child and returns the timeout error immediately. A descendant that
  still holds a pipe keeps a detached thread alive until the process exits,
  but it does not block the runner or the caller.
* Decision 0031 already records this: "a descendant that retains an output
  pipe may outlive direct-child termination" and "no unsafe process-group
  operations are used".

So the deadline does bound the runner, the behaviour is documented, and the
"bounded wait after kill" step in the first draft would have added joins that
do not exist today. There is nothing to fix and nothing left to record.

## Disposition

No code change, no decision addendum. The review document carries an erratum
for this finding. Reopen only if an executed reproduction shows the runner
blocking past its deadline.
