# Plan PERF-001 — fetch dest's heads once per run

| | |
| --- | --- |
| Finding | `docs/2026-09-02_REPOSITORY_REVIEW.md`, section 4, PERF-001 |
| Severity / priority | MEDIUM / P1 |
| Effort | Medium |
| Decision required | Yes — new decision 0049 (amends 0041 and 0046 Addendum 2 Finding K) and an addendum to 0031 (runner variant that writes stdin) |
| Depends on | — (CODE-001 and this plan touch `sync/mod.rs` dest→source; land CODE-001 first to avoid a rebase of the P0 fix) |
| Status | Implemented 2026-09-04 (steps 1-7; step 8 out of scope) |

## Problem

Per run, gitprism opens one transport per dest branch in three places:

| Site | Subprocesses | Purpose |
| --- | --- | --- |
| `anchor.rs:432-446` reconstruction | 1 fetch per advertised dest head | mapping index |
| `mod.rs:1425, 1436` dest→source | 1 `ls-remote` + 1 fetch per configured branch | existence + tip |
| `mod.rs:536` source→dest | 1 fetch per source branch | decision 0040 lease (must stay) |

Each has its own 300 s deadline and there is no run-wide deadline. A dest with a
few hundred customer branches makes a no-op sync take minutes; at the 4,096
listing horizon roughly 8,000 sequential handshakes are possible. Decision 0046
says heads are "fetched once".

## Target shape

One `git fetch -q --stdin -- <dest_url>` at the start of `run`, before
dest→source, reading one refspec per line from stdin:

```text
+refs/heads/<name>:refs/gitprism/fetched/dest/<name>
```

The names are exactly the ones the existing `ls-remote` listing
(`remote_branch_names`) returned, so the fetch is bounded by
`MAX_SOURCE_BRANCHES` (decision 0032) the same way the per-branch loop it
replaces is today. Every name has already passed `validate_branch_name`, so no
line can contain a newline or start with `-`. Reconstruction and dest→source
then read tips from `refs/gitprism/fetched/dest/<name>` through `git2`. The
listing itself is kept: it defines the decision-0047 completeness signal and
the horizon. Source→dest's per-branch fetch is kept unchanged for the lease.

Before the fetch, every existing `refs/gitprism/fetched/dest/*` ref is deleted
through `git2` (`--prune` only prunes under wildcard refspecs, not under an
explicit list). Counting those refs against `MAX_SOURCE_BRANCHES` and failing
above it keeps the clean-up bounded; more refs than the horizon can only have
been put there by something other than gitprism.

The namespace is a transient cache, not durable state: nothing reads it across
runs, it is cleared at the start of each run, and no marker or mapping is
stored there. Decision 0046's "no dedicated `refs/gitprism/*` mapping refs"
constraint is about mapping state and still holds; the decision text must say
so explicitly.

## Steps

### Step 1 — measure

**Files.** `src/git.rs` (test-only counter), `src/commands/sync/tests/anchor.rs`.

**Test first.** Add a `#[cfg(test)]` `thread_local!` counter incremented in
`git_command()` with a helper to read/reset it. Thread-local, not a
process-global atomic: `cargo test` runs tests on parallel threads, and the
runner spawns every subprocess from the calling thread, so a thread-local count
is exact for one test without serialising the suite. Write a test: dest with
12 branches, source with 2 configured; assert the number of `git` subprocesses
a no-op `run` spawns. Record the current number in the test's failure message;
it becomes the baseline the later assertion tightens.

### Step 2 — runner variant that writes stdin (0031 addendum first)

**Files.** `design/decisions/0031-centralized-git-process-runner.md` (addendum),
`src/git.rs` runner, tests in the same file.

**Change.** Decision 0031 sets stdin to null so no `git` child can inherit an
interactive stdin. Add an addendum: a runner variant may supply a
caller-generated, bounded byte string on a piped stdin, written from a helper
thread that closes the pipe when done (the same pattern the capture threads
use), never an inherited handle. The input is bounded by construction
(`MAX_SOURCE_BRANCHES` lines); the variant refuses larger inputs.

**Test first.** Using the existing fake-runner child (`runner_child("stdin")`
already reports the byte count it read): the variant delivers the exact bytes;
the child sees EOF; an input over the bound is refused before spawning; the
existing timeout and output-limit behaviour is unchanged for the variant.

### Step 3 — decision 0049

**Files.** `design/decisions/0049-dest-heads-are-fetched-once-into-a-transient-namespace.md`,
`index.md`, `log.md`.

**Change.** Context (the table above), Decision (target shape; the namespace
name; clear-then-fetch; the refspec list comes from the bounded listing; what
stays per-branch and why: 0040 lease), Why (one handshake per run; `git fetch
--stdin` is the primitive, available since git 2.29 and so on every git that
already satisfies the `merge-tree --write-tree` requirement; the listing
already exists), Consequences and trade-offs to state plainly:

* the fetch transfers the objects of every listed head in one transport; the
  per-branch loop it replaces transferred the same objects over many
  transports, so the bound (0032, 4,096 heads) is unchanged in kind;
* the local clone gains up to one ref per listed dest branch; `git branch -a`
  in the operator's checkout does not show them (not under `refs/remotes`);
* staleness for dest→source is unchanged in kind (a tip read once per run),
  only moved earlier in the run;
* 0041 ("dest→source fetches its own configured branch when absent locally")
  is satisfied by the namespace ref instead of a per-branch fetch;
* 0046 Addendum 2 Finding K's "duplicate fetch is the price of lease freshness"
  now describes exactly one duplicate, the source→dest lease fetch.

Record the rejected alternatives: a wildcard `+refs/heads/*` refspec (not
bounded by the listing; a dest with more heads than the horizon would transfer
all of them, which decision 0032 forbids for repository-controlled work);
refspecs as command-line arguments (Windows 32 KiB argv limit at 4,096 names);
parsing `FETCH_HEAD` (branch names may contain `'`); fetching by object id
(server-dependent).

### Step 4 — `git::fetch_heads_into_namespace`

**Files.** `src/git.rs`, tests in the same file.

**Change.** New function next to `fetch`, using the step 2 runner variant and
`validate_remote`. Takes the listed names, builds one refspec line per name,
runs `fetch -q --stdin -- <url>`. Tests with two bare repos: every listed
branch lands under the namespace and an unlisted one does not; a branch
deleted on dest between listing and fetch fails the fetch with git's own
"couldn't find remote ref" message and no namespace ref is left behind (the
caller treats this as the Finding G non-recoverable case, see step 5); a
branch whose name is valid but unusual (`a/b.c-d`) round-trips; the namespace
clear removes stale refs from a previous run.

### Step 5 — reconstruction reads the namespace

**Files.** `src/commands/sync/anchor.rs:375-458`, `src/commands/sync/tests/anchor.rs`.

**Change.** Delete the per-branch loop and `fetch_dest_head_for_reconstruction`.
For each listed name, read `refs/gitprism/fetched/dest/<name>`. Because the
fetch is all-or-nothing over the list, a name listed but deleted before the
fetch surfaces as a fetch error before any mutation (Finding G's
non-recoverable case). Finding G's recoverable case ("deleted in between,
contributes nothing") therefore moves: it is handled by re-running the listing
once and fetching again if the first fetch failed on a missing ref, bounded to
one retry, or by leaving today's behaviour and accepting the abort. Pick the
retry only if the owner wants it; the plan's default is the abort, which is the
smaller change and still fail-closed.

Adapt the existing Finding G tests to the chosen shape.

### Step 6 — dest→source reads the namespace

**Files.** `src/commands/sync/mod.rs:1403-1443`.

**Change.** Move the bulk fetch and the listing to `run` before dest→source.
Replace `remote_ref_exists` + `fetch` with: existence from the listing (when it
`can_establish_absence()`; otherwise fall back to one `remote_ref_exists`), tip
from the namespace ref. Keep the "has no ref on dest anymore" message text.

The race-retry loop at `:1574-1580` refetches source, not dest; unchanged.

### Step 7 — tighten the measurement

Step 1's test asserts the new count: one listing, one bulk fetch, one lease
fetch per source branch, plus pushes. Add a second test with 300 dest branches
(cheap: empty commits) to show the count no longer scales with dest's branch
count.

### Step 8 — run-wide deadline (separate, optional)

Not part of this plan's code. If wanted, it needs its own decision: a
`limits::MAX_RUN_SECONDS` with an operator remedy (the message must say which
phase and branch was in progress). Without step 8 the run is bounded by
per-subprocess deadlines times the now-constant subprocess count.

## Verification

Release gates; step 1/7 counter tests; the runner-variant tests; the full
`sync/tests/anchor.rs` suite (reconstruction semantics unchanged: Findings F,
G, J assertions all keep passing in their adapted form).

## Revision history

* 2026-09-02, after plan review: the first draft used a wildcard refspec,
  which is not bounded by the listing and so conflicts with decision 0032. It
  also rejected `--stdin` because the runner closes stdin; a bounded
  stdin-writing runner variant under a 0031 addendum is the smaller change and
  keeps the bound. The test counter is now thread-local so parallel tests
  cannot disturb it.
