---
type: Decision
title: Materialize control files byte-exact regardless of checkout filtering
description: .gitprism.toml and .gitprismignore are rewritten straight from their blob bytes after every checkout that might place them, bypassing text/CRLF filtering; every other file keeps its ordinary checkout attributes.
tags: [security, config, filtering, portability]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Context

[0026](0026-protected-versioned-policy.md) requires `.gitprism.toml` and
`.gitprismignore` to be authenticated as exact bytes: `GITPRISM_POLICY_SHA256`
pins a digest over the raw bytes every command reads back off disk. libgit2's
checkout applies ordinary text/CRLF filtering (`core.autocrlf`,
`.gitattributes`) when materializing a tree into the working directory, the
same as for any other tracked file, and gitprism never overrode that for
these two. On a machine where `core.autocrlf=true` — a common Git for
Windows default — checkout silently turns the committed LF blob into CRLF on
disk with no change to the blob itself, so the bytes a deployment pinned no
longer match what a later `setup`/`sync`/`resolve` run reads back, for
reasons unrelated to any real content change. This defeats 0026's premise of
an exact, portable byte anchor. Windows CI surfaced it concretely:
`setup`'s own "don't run again against your own prior graft" guard misfired
because its raw-byte comparison against the external `--config` file saw a
checkout-mangled copy instead of what was actually committed.

# Decision

Every checkout that might place `.gitprism.toml` or `.gitprismignore` in the
working tree — `setup`'s initial graft/merge checkout, and `sync`'s/
`resolve`'s checkout of what dest→source or a resolve replacement just
landed on the local source branch — is immediately followed by
`policy::restore_control_files_exact`, which re-writes both files straight
from the checked-out commit's tree blobs via a raw filesystem write. This
bypasses the working-tree filter pipeline entirely, but only for these two
filenames: every other file continues through git's normal checkout
attribute/config handling untouched, so a repository owner's own
`.gitattributes`/`core.autocrlf` choices for their own tracked content are
unaffected. The digest comparison itself (`policy::verify_expected_digest`)
and `setup`'s `ensure_external_config_is_not_overwritten` raw-byte comparison
are unchanged — this fixes what lands on disk, not what an already-correct
comparison does with it. Neither is loosened or made to normalize bytes
before comparing.

Rejected: disabling checkout filters for the whole tree via
`CheckoutBuilder::disable_filters`. That would also strip whatever filtering
a repository owner legitimately relies on for their own source/dest content,
which is unrelated to gitprism's own two control files.

# Why

0026's digest is only a meaningful trust boundary if the exact bytes it
authenticates are also the exact bytes every command reads back, on every
platform gitprism runs on. Scoping the fix to the two known control-file
names preserves that guarantee without imposing an opinion on how the rest
of the repository's content should be checked out.

# Consequences

`policy::restore_control_files_exact` must run after every real (non-dry-run)
checkout of a tree that may contain these two files; a future checkout call
site that forgets it reintroduces this bug. Tests set `core.autocrlf=true` on
the test repository itself (not the process's ambient git config) and prove
both directions: the two control files stay byte-identical and pass a
`policy::hash_files` check, while an ordinary tracked text file still
receives normal CRLF conversion.

The raw filesystem write bypasses git2's index, and both remaining steps
around it turned out to need the same care 0033 already established for
control-file recovery, not a shortcut:

- The write itself uses the same no-follow, create-new pattern `setup`'s own
  control-file recovery uses (remove whatever occupies the path, then
  `create_new`), not a plain overwrite — a plain write follows an existing
  symlink or writes through an existing hardlink instead of replacing it,
  which is exactly what 0033 already rejects for control-file recovery.
- Re-staging the index after the write must not go through
  `Index::add_path`: it hashes through the working-tree filter's *clean*
  side, so a control file whose own committed blob already contains CRLF
  (a real case — an externally-authored `--config` with CRLF line endings,
  committed byte-for-byte per 0026) would get re-normalized to LF before
  hashing, staging a blob that differs from the one `tree`/HEAD actually
  has. The index entry is instead pointed straight at the tree's own
  oid/mode, with no hashing involved. A test commits such a CRLF-bearing
  blob directly and asserts the index entry's id equals HEAD's, not just
  that the working-tree bytes look right.

Two more followed on Unix, both about the file's mode rather than its
content:

- `write_regular_file_no_follow` used to close the freshly created `File`
  and then chmod it by path. That reopens exactly the race the no-follow
  write itself exists to close: whatever occupies the path by the time the
  chmod runs — symlink or not — is what gets its permissions changed, not
  necessarily the file this call just created. The mode is now applied to
  the still-open `File` handle before it's dropped, so the write and the
  permission change are both pinned to the one inode this call created.
  `setup`'s own control-file recovery went through the same helper and so
  needed no separate fix.
- `restore_control_files_exact`'s raw write only carried over the file's
  *content*, leaving the recreated file at whatever default permissions
  `create_new` gives it. A control file tracked as `100755` would silently
  become `100644` on disk while the index (correctly) still pointed at the
  executable blob — dirty under `core.filemode=true` despite nothing about
  the content changing. The tree entry's mode is now passed into the same
  helper alongside the bytes. A Unix test commits a control file as
  `FileMode::BlobExecutable`, calls the restore directly, and asserts both
  the on-disk permission bits and `status_file`'s cleanliness.
