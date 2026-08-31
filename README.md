# gitprism

gitprism synchronizes a **private source-of-truth repository** with a filtered
customer-facing Git repository while allowing the customer repository to run
its own normal branch, pull-request, and merge workflow.

The **source** repository is authoritative and is a superset of **dest**.
Source contains everything dest contains, plus files or directories that must
never leave source: internal tooling, secrets-adjacent configuration, CI
infrastructure, or anything else the repository split exists to protect.

The important distinction from a conventional one-way mirror is that dest is
not read-only. It may be a real customer collaboration repository — for
example Azure DevOps — where tickets are handled, branches are reviewed, and
pull requests are merged. Once a change has been merged into an explicitly
authorized destination branch, gitprism can promote that committed change back
into the private source of truth.

The synchronization policy is deliberately asymmetric:

* **source → dest**: every source branch is mirrored automatically, with
  source-only paths filtered out. Round-tripped branches — the ones listed
  in `.gitprism.toml` — are updated **fast-forward only**, because dest is
  published history that customer clones, branches, and pull requests
  depend on there. A mirror-only branch (not in that list) is a pure
  projection of source with nothing imported back; gitprism still updates it
  fast-forward whenever that's possible, and only force-updates dest's
  projection when a fast-forward can no longer work because source's own
  history there was deliberately rewritten.
* **dest → source**: only explicitly configured branches are watched. If one
  of those branches advances independently — for example because a customer
  PR was merged directly into a release branch — gitprism brings those
  committed changes back into source, unfiltered.

A typical workflow looks like this:

```text
private source                         customer dest
──────────────                         ─────────────

main ────────────────────────────────► main
release ─────────────────────────────► release
feature/foo ─────────────────────────► feature/foo
                                           │
                                      customer work
                                           │
                                      pull request
                                           │
                                         merge
                                           │
release ◄──────── gitprism ─────────── release
   │
   └─────────────────────────────────────► ...
```

The private repository remains the durable source of truth. The destination is
an **authorized producer of changes on selected branches**: customer work does
not need to be imported while a PR is still under review, and gitprism does
not need to understand or own the customer's ticket or PR system. It operates
on Git history after the destination branch has actually advanced.

If a source commit only touched excluded paths, filtering it for dest leaves
nothing to push — gitprism skips it instead of pushing an empty commit.

No history rewriting, and force-pushing is never the steady-state sync
mechanism: it's reserved for the rare, positively-detected case of a
mirror-only branch whose source history was deliberately rewritten, per
[decisions/0038](design/decisions/0038-branch-authority-determines-whether-history-may-be-rewritten.md)
and [decisions/0039](design/decisions/0039-mirror-only-source-rewrites-rebuild-the-projection.md).
Round-tripped branches never accept a forced update, in either direction.
See [`design/references/git-filter-repo.md`](design/references/git-filter-repo.md)
for why avoiding force-push as the routine mechanism is a hard constraint
this tool is built around, not just a preference.

Mutating commands serialize through a non-blocking lock in Git's common
directory, so linked worktrees share the same operation boundary. Dest-to-source
local branch advancement is checked against a safe checkout (rejecting local
working-tree conflicts while preserving unrelated untracked files) and uses a
compare-and-swap ref update; gitprism never overwrites a concurrent ref move.

## Why gitprism?

This combination of constraints shows up when, for example:

* an organization keeps its complete repository private;
* all internal branches must be available in a customer's Git service;
* selected internal paths must never be exposed to that service;
* the customer manages tickets, reviews, and pull requests entirely in their
  own Git platform;
* customer PRs are merged there using the customer's normal process;
* only designated branches, commonly release branches, are allowed to feed
  those completed changes back into the private source of truth; and
* neither side's already-published history may be rewritten to make the
  synchronization work.

Tools such as Copybara are relevant prior art and share useful concepts with
gitprism, including filtered transformations, per-commit migration, and
commit-embedded synchronization state. But Copybara's natural model is an
authoritative source plus changes imported from the other side *before* they
become authoritative destination history — not a destination branch that
has already advanced independently, which is what gitprism reconciles.

In short, gitprism is designed for:

> **filtered projection outward, customer-owned merges, selective promotion
> back into the source of truth, and immutable published history.**

## Status

Early and under active design. The two sync directions and the conflict
helper described below are implemented and covered by the locked test suite,
but the tool has not yet reached a stable release. Read
[`design/index.md`](design/index.md) before assuming behavior beyond what's
written here.

The repository CI workflow validates formatting, Clippy, locked tests and a
locked release build with Rust 1.89, and runs the test suite on Linux, macOS
and Windows. Dependency checks use `cargo audit` and `cargo deny`. This is
engineering validation, not a claim that a release has already shipped: no
tagged release has been published yet. The pinned release workflow builds,
tests and smoke-checks the approved archives and publishes their checksums;
installer and package-manager integrations are optional.

## How it works, briefly

* **Setup is a real git graft, not a snapshot.** `gitprism setup` fetches
  dest's tip and makes source's first commit a genuine child of it, so every
  future merge/cherry-pick between the two has a real, native git
  merge-base — no hand-rolled cross-repo ancestry tracking.
* **Sync state lives durably in authenticated commit markers.** Every commit
  gitprism creates carries a human-readable trailer and authenticated state
  recording the commit it came from on the other side. There's no separate
  database or state file — each `sync` run reconstructs a bounded, in-memory
  index from the fetched histories and picks up from there. That also means
  any run is safe to invoke redundantly: a run that finds nothing new is a
  cheap no-op.
* **New and rewritten mirror-only branches use exact anchors.** gitprism
  follows the branch's first-parent history to the nearest authenticated
  source-to-dest mapping, rather than inferring an anchor from sibling
  branches. Mapped branches are processed by increasing first-parent distance
  (with branch name as the deterministic tie-break), so a parent projected in
  the current run is available to its child. Only incomparable exact
  destination mappings recorded for the same source commit halt an affected
  branch for operator review.
* **source → dest mirrors every branch automatically.** No per-branch config
  needed on this side — a new branch on source starts syncing to dest the
  moment it exists.
* **dest → source is an explicit, configured list of branches** — the
  branches on dest whose independent changes should flow back into source.
* **Real conflicts hard-stop the whole run.** If dest and source
  independently changed the same content, gitprism doesn't guess a winner or
  silently drop one side — it aborts the entire run, pushes nothing further
  for any branch, and prints the exact commands to reproduce and resolve the
  conflict by hand (`gitprism resolve`, below). Branches already pushed
  earlier in the same run are unaffected; branches not yet reached are not
  processed until the conflict is resolved.
* **gitprism never deletes branches** on either side, on either sync
  direction.

The full reasoning behind each of these — including alternatives considered
and rejected — is recorded as individual decisions in
[`design/decisions/`](design/decisions/index.md).

## Installing / building

Requires Rust 1.89 or newer (2024 edition) and Git 2.45 or newer on `PATH` —
gitprism calls out to real `git` for push/fetch/cherry-pick rather than
reimplementing network or working-tree operations. Git 2.45 is required for
the raw-tree `git merge-tree` interface used by synchronization.

Git subprocesses are noninteractive (`stdin` is closed and
`GIT_TERMINAL_PROMPT=0`) and have a 300-second deadline. Operators running
slow, trusted Git transports may set `GITPRISM_GIT_TIMEOUT_SECONDS` to an
integer from 1 through 3600; this is process configuration, not repository
policy. Output is captured concurrently with hard limits: parse-capable output
has a 64 MiB limit, small commands have a 64 KiB stdout limit, and diagnostics
have a 1 MiB capture limit before their terminal-safe 8 KiB presentation
frame. Exceeding a limit or deadline fails the operation rather than parsing
truncated Git data.

Every git subprocess also carries `-c credential.interactive=true`,
overriding a CI checkout's own `credential.interactive=false`/`never` (GitLab
Runner sets this on its own clones to avoid hangs), which would otherwise make
Git refuse to invoke `GIT_ASKPASS` at all before gitprism's own credential
helper gets a chance to run — command-line `-c` wins over that ambient
config. It also sets `GIT_PROTOCOL_FROM_USER=0` with `-c
protocol.file.allow=always`, so an `ext::`/`fd::`-style transport stays
refused regardless of an operator's own `protocol.allow` config, independent
of Git's own already-refusing default, while a local-path source/dest URL
keeps working.

```sh
cargo build --release
# binary at target/release/gitprism
```

gitprism is distributed from source and is not published to crates.io;
clone the canonical repository at
[`https://github.com/blankmi/gitprism`](https://github.com/blankmi/gitprism)
and build it locally. It is available under the [MIT License](LICENSE).

### Release policy

Release tags are exactly `v<package-version>`. Each release must provide
archives for Linux x86_64, macOS arm64, macOS x86_64 and Windows x86_64, plus
a `SHA256SUMS` file covering every archive. Artifacts are
intentionally unsigned: the checksums detect corruption, but do not
authenticate release provenance. Push the exact tag to run the pinned release
workflow; it collects the archives, generates `SHA256SUMS`, creates one draft
release with all assets, and then publishes it. The release automation is
documented in
[`design/playbooks/0002-release-distribution.md`](design/playbooks/0002-release-distribution.md).

### Byte and platform compatibility

Git paths are handled as bytes for comparisons and diagnostics where the
platform APIs allow it. Malformed bytes, ASCII controls, and backslashes are
rendered with deterministic escapes rather than lossy replacement or terminal
control sequences. Unix preserves non-UTF-8 path bytes in native paths;
Windows and other platforms reject paths that cannot be represented safely.
Non-UTF-8 commit messages, tree names, and branch names are rejected before a
commit or ref is created or advanced, so gitprism does not promise to sync
those objects across platforms.

Repository-controlled input also has static safety budgets: control files are
limited to 1 MiB, small Git state files to 64 KiB, commit messages to 1 MiB,
configured branches to 1,024, source-branch discovery to 4,096, pending
commits to 10,000, and marker scans to 100,000 first-parent commits. Recursive
tree work is limited to 1,000,000 entries and depth 256; conflict reporting is
limited to 100,000 records and 8 MiB of raw path bytes. Resolution worktrees
are signed into their operation state and must remain registered to the same
Git common directory; tampered or legacy state fails closed. Exceeding a
budget fails the operation; gitprism does not truncate data, skip conflicts,
or choose a conflict resolution.

## Configuration: `.gitprism.toml`

gitprism is configured by `.gitprism.toml`, committed inside **source**
itself (it's automatically excluded from what gets synced to dest, so you
never need to list it in your own exclude file). Every command except
`setup` reads this file from source's checked-out tree; `setup` reads it
straight off disk since source has no history yet at that point.

```toml
branches = ["main", "release-2.0"]

[committer]
name = "gitprism"
email = "gitprism@example.com"

[source]
url = "git@example.com:group/source.git"

[dest]
url = "git@example.com:group/dest.git"
```

* **`branches`** — the branches `dest → source` watches for independent
  changes to bring back into source. (`source → dest` doesn't read this list;
  it discovers and mirrors every branch on source automatically.)
* **`[committer]`** *(required)* — the identity gitprism stamps as committer
  on every commit it creates. The original author is preserved untouched;
  gitprism only ever stamps itself as committer, the same way `git`'s own
  rewriting commands (rebase, cherry-pick) do.
* **`[source].url`** / **`[dest].url`** *(optional)* — where gitprism pushes
  each side. Omit either (or both) to fall back to the `GITPRISM_SOURCE_URL`
  / `GITPRISM_DEST_URL` environment variables instead — useful for
  per-environment URLs (staging vs. a developer's own fork) you don't want
  committed into source's history. Neither form is a place for embedded
  credentials; see "Mapping state key" below.

### Protected policy digest

The versioned `.gitprism.toml` and root `.gitprismignore` are treated as
untrusted repository input until their exact bytes match the protected
`GITPRISM_POLICY_SHA256` deployment variable. Compute the value after checking
out the approved policy:

```sh
gitprism policy-hash
```

Set that 64-character lowercase SHA-256 value in CI before `setup`, `sync`, or
`resolve`. A policy change requires an intentional protected-variable update.
The URL fallback variables remain deployment input and are not included in the
policy digest.

### Mapping state key

Set `GITPRISM_STATE_KEY` for every `setup`, `sync`, and `resolve` invocation.
It must be a unique key for this source/dest pair, encoded as exactly 64
hexadecimal characters (32 bytes); generate one with `openssl rand -hex 32`
and store it in your CI secret manager. Gitprism keeps the
normal `Gitprism-Source-Commit` / `Gitprism-Dest-Commit` trailers readable, but
trusts them only when the commit also has a final authenticated state block.
The key is never committed and is removed from Git subprocess environments so
repository hooks and helpers cannot read it. Resolved `GITPRISM_SOURCE_URL`
and `GITPRISM_DEST_URL` fallback values are scrubbed from those children's
environment the same way, so a hook or helper cannot read either straight out
of the environment. That scrubbing is not URL secrecy, though: gitprism still
passes the resolved URL to Git as a command-line argument, where it is
visible to any other process running as the same user (`ps`, or
`/proc/<pid>/cmdline` on Linux) and can end up captured in CI logs or crash
reports. Do not put credentials in a repository URL. Gitprism scrubs only its
own variables, leaving Git's ordinary authentication mechanisms untouched — use
an SSH key/agent, a Git credential helper, `GIT_ASKPASS`, or the CI platform's
native Git authentication instead. Git's executable, global/local
configuration, credential helpers, and repository hooks are still a trusted
boundary. User-supplied `Gitprism-*` lines are stripped from generated messages
to prevent a second trusted marker; use ordinary prose for literal
documentation of those names.

`dest → source` imports dest-authored content unfiltered: `.gitprismignore`
only ever excludes source's own commits from reaching dest, it is never
applied against dest's independent commits on the way back. An add/add
conflict protects an *existing* excluded file — dest editing it in a way that
would collide is a real conflict, not a silent overwrite — but dest can still
introduce a brand-new file under a path source excludes entirely (a CI config
directory, say). That file lands in source exactly as dest committed it and
may run in source's own CI. Treat dest as capable of adding, not just
reflecting, content in source.

## Excluding paths: `.gitprismignore`

Files and folders that must stay in source and never reach dest go in
`.gitprismignore`, committed at source's repo root, using exactly
`.gitignore`'s own syntax (globs, `#` comments, `!` negation, trailing `/`
for directories — no new syntax to learn). Like `.gitprism.toml`, this file
excludes itself automatically.

## Commands

### `gitprism setup`

One-time step. Run inside a real, already-`git init`'d repo that will become
source. Fetches dest's current tip for every branch in `.gitprism.toml`'s
`branches` list and, for each one, either grafts a new same-named branch
onto it (if source has nothing there yet, or exactly matches dest's tip
already — e.g. a plain `git clone <dest-url> source && cd source`) or
reconciles source's own existing history with dest's tip into a real merge
commit, whenever the two genuinely share history — carrying over
`.gitprism.toml` and `.gitprismignore` either way. A branch whose history has
*nothing* in common with dest hard-fails outright: gitprism never merges
unrelated histories on its own, so that has to be done by hand with real git
first. A local branch not listed in `.gitprism.toml` is left completely
alone, whatever it contains. Refuses to run against its own prior output —
this is a one-time step per branch, not something to re-run once it has
succeeded; `gitprism sync` is how dest's later changes come in after that.

### `gitprism sync`

The recurring job — run this from CI (or manually) on every source push, on
a schedule, or on demand. Does both directions in one run: `dest → source`
for every configured branch, then `source → dest` for every branch that
currently exists on source. Safe to run redundantly; a run that finds
nothing new is a no-op. On a real conflict, it aborts the run: no further
branches are processed until the operator resolves it with `gitprism
resolve`. Branches already pushed earlier in the same run are unaffected.

See [`design/playbooks/0001-gitlab-pipeline-triggers.md`](design/playbooks/0001-gitlab-pipeline-triggers.md)
for a suggested GitLab CI trigger setup (gitprism itself doesn't care what
triggers it) and known GitLab Runner environment problems.

### `gitprism resolve <branch>`

Run this after `sync` reports a conflict on `<branch>`. It reproduces the
conflict as an ordinary `git cherry-pick` with normal conflict markers in
your working tree — resolve it exactly like you would any other git
conflict, then run:

```sh
gitprism resolve <branch> --continue
```

which finishes the cherry-pick, writes the trailer gitprism needs to track
that the conflict is resolved, and pushes the result. No trailer to hand-type,
no gitprism-specific merge UI — resolution is 100% standard git. The temporary
cherry-pick commit uses the configured `[committer]` identity and preserves the
picked commit's author; it does not depend on the operator's global Git
identity.

For a source-to-dest conflict, use the explicit direction:

```sh
gitprism resolve <branch> --direction source-to-dest
```

Gitprism creates a unique linked worktree containing destination-space content
and leaves the source checkout unchanged. Resolve and stage the conflict there,
then run:

```sh
gitprism resolve <branch> --direction source-to-dest --continue
```

The operation is authenticated and resumable through Git metadata. Edits to
excluded paths are rejected, and destination-owned paths matching an exclusion
remain intact. If the destination moves or the final push loses its
fast-forward race, copy or save the staged resolution, remove the reported
linked worktree, rerun the source-to-dest resolve command against the new
destination, and reapply the resolution there. Do not retry `--continue` after
the destination has moved.

## Design documentation

This repo's `design/` directory is the source of truth for *why* gitprism
works the way it does — start at [`design/index.md`](design/index.md):

* [`design/requirements/`](design/requirements/index.md) — the workflow this
  tool exists to support, in the project owner's own words.
* [`design/references/`](design/references/index.md) — prior art evaluated
  before building this (josh, Copybara, git-subtree, git-filter-repo,
  jujutsu), and what each ruled in or out.
* [`design/decisions/`](design/decisions/index.md) — numbered architecture
  decisions, each with context, the decision, why, and consequences. This is
  more authoritative than this README for anything about *how* gitprism
  works internally.
* [`design/playbooks/`](design/playbooks/index.md) — operational guidance
  (e.g. CI triggers) that's deliberately kept separate from the tool's own
  architecture, since gitprism is trigger-agnostic by design.

## Development

```sh
cargo test      # unit + integration tests; TDD is how this project is built
cargo build
```

New behavior here is expected to start with a failing test, then the
decision that motivates it recorded in `design/decisions/` before the
implementation is treated as settled — see [`AGENTS.md`](AGENTS.md) for the
full working-style notes.
