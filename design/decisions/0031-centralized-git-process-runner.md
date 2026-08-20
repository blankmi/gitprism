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
