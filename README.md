# gitprism

gitprism keeps two git repositories in sync where one — **source** — is a
superset of the other — **dest**. Source holds everything dest has, plus
files or folders that must never leave source (internal tooling, secrets-adjacent
config, whatever the split exists to protect). gitprism moves changes both ways:

* **source → dest**: filtered, so source-only paths never appear in dest, and
  **fast-forward only** — gitprism never force-pushes dest, because dest is
  treated as shared history other clones depend on.
* **dest → source**: for the branches you list, brings dest's independent
  changes (e.g. a PR merged straight against dest) back into source,
  unfiltered.

If a source commit only touched excluded paths, filtering it for dest leaves
nothing to push — gitprism skips it instead of pushing an empty commit.

No history rewriting, no force-pushing. See
[`design/references/git-filter-repo.md`](design/references/git-filter-repo.md)
for why that's a hard constraint this tool is built around, not just a
preference.

Mutating commands serialize through a non-blocking lock in Git's common
directory, so linked worktrees share the same operation boundary. Dest-to-source
local branch advancement is checked against a safe checkout (rejecting local
working-tree conflicts while preserving unrelated untracked files) and uses a
compare-and-swap ref update; gitprism never overwrites a concurrent ref move.

## Status

Early and under active design. The two sync directions and the conflict
helper described below are implemented and covered by `cargo test` (178 tests
passing as of this writing), but the tool hasn't run against a real
production pair of repos yet. Read [`design/index.md`](design/index.md)
before assuming behavior beyond what's written here.

## How it works, briefly

* **Setup is a real git graft, not a snapshot.** `gitprism setup` fetches
  dest's tip and makes source's first commit a genuine child of it, so every
  future merge/cherry-pick between the two has a real, native git
  merge-base — no hand-rolled cross-repo ancestry tracking.
* **Sync state lives in the commits themselves.** Every commit gitprism
  creates carries a trailer recording the commit it came from on the other
  side. There's no separate database or state file — a `sync` run scans
  history for what's already been carried across and picks up from there.
  That also means any run is safe to invoke redundantly: a run that finds
  nothing new is a cheap no-op.
* **source → dest mirrors every branch automatically.** No per-branch config
  needed on this side — a new branch on source starts syncing to dest the
  moment it exists.
* **dest → source is an explicit, configured list of branches** — the
  branches on dest whose independent changes should flow back into source.
* **Real conflicts hard-stop.** If dest and source independently changed the
  same content, gitprism doesn't guess a winner or silently drop one side —
  it aborts that branch's sync, pushes nothing, and prints the exact commands
  to reproduce and resolve the conflict by hand (`gitprism resolve`, below).
* **gitprism never deletes branches** on either side, on either sync
  direction.

The full reasoning behind each of these — including alternatives considered
and rejected — is recorded as individual decisions in
[`design/decisions/`](design/decisions/index.md).

## Installing / building

Requires Rust 1.89 or newer (2024 edition) and a `git` binary on `PATH` — gitprism calls
out to real `git` for push/fetch/cherry-pick rather than reimplementing
network or working-tree operations.

Git subprocesses are noninteractive (`stdin` is closed and
`GIT_TERMINAL_PROMPT=0`) and have a 300-second deadline. Operators running
slow, trusted Git transports may set `GITPRISM_GIT_TIMEOUT_SECONDS` to an
integer from 1 through 3600; this is process configuration, not repository
policy. Output is captured concurrently with hard limits: parse-capable output
has a 64 MiB limit, small commands have a 64 KiB stdout limit, and diagnostics
have a 1 MiB capture limit before their terminal-safe 8 KiB presentation
frame. Exceeding a limit or deadline fails the operation rather than parsing
truncated Git data.

```sh
cargo build --release
# binary at target/release/gitprism
```

### Byte and platform compatibility

Git paths are handled as bytes for comparisons and diagnostics where the
platform APIs allow it. Malformed bytes, ASCII controls, and backslashes are
rendered with deterministic escapes rather than lossy replacement or terminal
control sequences. Unix preserves non-UTF-8 path bytes in native paths;
Windows and other platforms reject paths that cannot be represented safely.
Non-UTF-8 commit messages, tree names, and branch names are rejected before a
commit or ref is created or advanced, so gitprism does not promise to sync
those objects across platforms.

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
  credential-bearing or per-environment URLs you don't want committed into
  source's history.

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
repository hooks and helpers cannot read it. Resolved `GITPRISM_SOURCE_URL` and
`GITPRISM_DEST_URL` fallback values are scrubbed from those children too;
normal Git authentication variables such as `GIT_ASKPASS` remain available.
Git's executable, global/local configuration, credential helpers, and
repository hooks are still a trusted boundary. User-supplied `Gitprism-*` lines
are stripped from generated messages to prevent a second trusted marker; use
ordinary prose for literal documentation of those names.

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
nothing new is a no-op. On a real conflict, it stops that branch's sync,
leaves everything else unaffected, and prints how to resolve it.

See [`design/playbooks/0001-gitlab-pipeline-triggers.md`](design/playbooks/0001-gitlab-pipeline-triggers.md)
for a suggested GitLab CI trigger setup (gitprism itself doesn't care what
triggers it).

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
