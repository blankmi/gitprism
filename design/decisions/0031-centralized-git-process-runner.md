---
type: Decision
title: Centralize bounded noninteractive Git execution
description: Every production Git subprocess uses one standard runner with null stdin, disabled terminal prompting, concurrent bounded capture, and a deadline.
tags: [security, reliability, git, subprocesses]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

gitprism delegates fetch, push, merge, worktree, and interactive resolution
operations to the Git executable. A direct `Command::output` call can block
forever, deadlock when one pipe fills, inherit an interactive stdin, or retain
unbounded output in memory. Parse-critical output must not be truncated and
then treated as valid Git data.

# Decision

All production Git children are started through one standard-library runner.
The runner sets `stdin` to null, sets `GIT_TERMINAL_PROMPT=0`, and preserves
normal authentication variables such as `GIT_ASKPASS` while retaining the
existing gitprism-specific environment scrubbing. It reads stdout and stderr
concurrently through bounded channels.

The default Git deadline is 300 seconds, suitable for a slow CI fetch or push
without allowing a stuck helper to hang a job indefinitely. An operator may
override it with `GITPRISM_GIT_TIMEOUT_SECONDS`, an integer from 1 through
3600. This is process configuration, not repository policy, and is never read
from repository-controlled files.

Parse-capable stdout is limited to 64 MiB per stream and diagnostics stderr to
1 MiB; small commands use a 64 KiB stdout limit. Each invocation also enforces
the corresponding combined stdout/stderr total. Exceeding any cap kills and
reaps the direct Git child and returns an error. The runner never truncates
parse-critical output and continues. Human-facing diagnostics are separately
escaped, credential-redacted, and framed to 8 KiB.

The runner polls `try_wait`. On timeout or overflow it kills and reaps the
direct child. No unsafe process-group operations are used. Git hooks, helpers,
SSH agents, and other descendants remain part of the trusted Git boundary; a
descendant that retains an output pipe may outlive direct-child termination.
If a direct child exits but its pipes do not close within a bounded two-second
drain grace, the runner fails rather than returning incomplete output.

# Why

Concurrent bounded capture prevents stdout/stderr pipe deadlocks and makes
resource exhaustion explicit. Null stdin and disabled terminal prompting make
CI behavior deterministic. A process-level deadline and direct-child reap
cover the failure modes that standard `output()` leaves to the operating
system. Keeping the timeout outside repository policy avoids letting a
malicious repository extend or shorten the operator's execution boundary.

# Consequences

Git commands must declare whether they need small diagnostic output or the
larger parse limit. Tests use an explicit-duration internal runner rather than
mutating the process-global timeout environment, and exercise normal dual
stream capture, timeout, stdout/stderr overflow, and prompt suppression.
Stray trusted descendants are not forcibly placed in a process group or
terminated by gitprism.

# Addendum 2026-08-28 — the child environment and forced `-c` settings

Every Git child built by `git_command()` (`src/git.rs`) receives, in addition
to the operator's inherited environment:

| Setting | Value | Why |
|---|---|---|
| `GITPRISM_STATE_KEY`, `GITPRISM_SOURCE_URL`, `GITPRISM_DEST_URL` | removed | Repository-controlled hooks and helpers must not see the marker secret or credential-bearing URLs. |
| `GIT_TERMINAL_PROMPT` | `0` | No interactive prompting; `GIT_ASKPASS` and credential helpers still work (this decision). |
| `GIT_PROTOCOL_FROM_USER` | `0` | Transports whose `protocol.<name>.allow` is unset fall to Git's `user` policy, which this disables. Closes the `ext::`/`fd::` vector independent of the operator's global/system config (review 2026-08-27, F-20). An explicit `protocol.ext.allow=always` in ambient config still wins, as it would for any Git invocation. |
| `-c protocol.file.allow=always` | argv | `file` shares the `user` default, so the line above would also refuse local-path remotes. Local paths are a supported remote shape (and the test suite's fixture shape). gitprism never recurses submodules, so the CVE-2022-39253 concern behind Git's `file` default does not apply. |
| `-c credential.interactive=true` | argv | A CI checkout's `.git/config` (GitLab Runner) may set `credential.interactive=false`, which makes Git refuse to call `GIT_ASKPASS` at all. Command-line `-c` beats file-level config. |
| `GIT_EDITOR` | `true` | Only on `cherry-pick`/`commit`-shaped calls: never open an editor. |
| `GIT_COMMITTER_NAME`, `GIT_COMMITTER_EMAIL` | `[committer]` from `.gitprism.toml` | Only on commit-creating calls; the committer is policy, not host identity. |

Tests additionally set `GIT_CONFIG_GLOBAL` to an empty file and
`GIT_CONFIG_NOSYSTEM=1` (`#[cfg(test)]` only) so the suite never reads the
host's `~/.gitconfig` or `/etc/gitconfig`. Production never touches these two
variables: the operator's git config remains the trusted boundary it always
was.

Adding a new override here requires updating this table and README's
"Installing / building" paragraph together.

# Addendum 2026-09-04 — a runner variant that writes bounded stdin

PERF-001 (`docs/plans/2026-09-02/PERF-001-fetch-dest-heads-once.md`) needs
`git fetch --stdin`, which reads one refspec per line from stdin instead of
argv — the only way to fetch a bounded but potentially large (up to
`MAX_SOURCE_BRANCHES`, decisions/0032) list of branches over one transport
without a command-line length limit (Windows' 32 KiB argv caps refspec
arguments well below 4,096 names). This decision's stdin-null rule exists so
no child can inherit an interactive stdin; it says nothing about a caller
supplying its own, already-bounded data on a pipe.

## Decision

`run_git_stdin_output` is a new sibling of the runner every production
caller already goes through: it applies the same `GIT_TERMINAL_PROMPT=0`,
output caps, and deadline, but opens stdin as a pipe instead of
`Stdio::null()`. The caller-supplied byte string is written to that pipe
from its own helper thread — the same pattern the existing stdout/stderr
capture threads already use, just in the opposite direction — and the
thread drops its end of the pipe once the write finishes, so the child sees
a normal EOF rather than hanging on stdin. No inherited handle (the
operator's own stdin, or any other file descriptor) is ever connected to
the child. Both variants now share one spawn/capture core; only how stdin is
set up before that core runs differs.

The input is bounded twice: every call site builds it from a listing already
capped at `MAX_SOURCE_BRANCHES` (decisions/0032), and the variant itself
refuses an input above a fixed byte ceiling before spawning anything, rather
than trusting a caller's bound alone.

## Why

Git already provides the primitive (`--stdin`) for the command PERF-001
needs it for; the alternative — refspecs as command-line arguments — has a
real, cross-platform argv limit at exactly the branch counts this bound is
meant to allow (decisions/0032). Writing from a helper thread mirrors the
concurrent capture threads this decision already established, rather than
inventing a second, different pattern for the opposite direction of a pipe.

## Consequences

* This variant is for a caller with fully caller-generated, already-bounded
  input only. It does not reopen interactive stdin, and no existing
  production caller changes — every one of them keeps going through the
  null-stdin variant.
* Tests exercise it directly (`src/git.rs`): exact-byte delivery, the child
  observing EOF, an input over the byte ceiling refused before any process
  is spawned, and that timeout/output-cap behavior is otherwise unchanged
  from the null-stdin variant.
