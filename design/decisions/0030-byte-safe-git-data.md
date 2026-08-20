---
type: Decision
title: Preserve Git bytes and reject unsupported text before mutation
description: Git paths remain byte-safe for comparisons and diagnostics where APIs permit; unsupported non-UTF-8 commit messages, tree names, and refs are rejected before any commit or ref mutation.
tags: [security, git, portability, terminal-safety]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

Git stores paths and other repository data as bytes, while Rust strings and
some platform APIs require UTF-8. Replacing malformed bytes with U+FFFD,
dropping them, or turning them into an empty message can make two different
repositories appear equivalent and can make a later mutation unsafe. Git's
NUL-delimited output also cannot be parsed correctly through a lossy string
conversion.

# Decision

gitprism preserves Git path bytes for comparisons and diagnostics whenever the
API exposes them. Raw byte paths are compared component-wise, so `foo` does
not collide with `foobar`, while `foo` and `foo/bar` do. Conflict and Git
diagnostic output is rendered deterministically: malformed bytes use `\xNN`,
ASCII controls and backslashes are escaped, and valid Unicode remains readable.
Credential-bearing remote values are redacted as raw bytes before rendering.

When an operation must construct a commit, ref, or tree through a UTF-8-only
interface, gitprism validates the original bytes and fails with the offending
object or ref identified. It never silently drops, replaces, or bypasses a
safety check. On Unix, Git path bytes are converted to native paths without
UTF-8 conversion. On Windows and other platforms whose native path API cannot
represent the bytes safely, gitprism rejects the path explicitly.

Non-UTF-8 commit messages, tree entry names, and branch names are therefore
not promised to sync across platforms; they fail before commit or ref
advancement. This policy does not treat repository-controlled data as trusted.

# Why

Raw bytes are the only representation that preserves Git's identity for
malformed filenames and NUL-delimited records. Explicit rejection is safer
than a lossy best effort when the next operation would create a different
commit or move a ref. Deterministic escaping keeps hostile repository content
from producing terminal controls without making ordinary Unicode unreadable.

# Consequences

Callers must use `message_bytes`, `name_bytes`, and `path_bytes` at Git
boundaries and include context when rejecting unsupported text. Tests cover
invalid Unix index paths, merge-tree conflict paths, valid Unicode messages,
malformed messages that leave refs unchanged, byte-safe collision logic, and
terminal-safe formatting. Full support for arbitrary non-UTF-8 branch and
tree names remains intentionally out of scope until every mutation API has a
safe byte-preserving implementation on the target platform.
