---
type: Decision
title: A replayed commit's differing control file halts that branch instead of being unioned into policy
description: Supersedes decisions/0036's additive-exclusion union. When a commit a source→dest branch replays carries a .gitprismignore or .gitprism.toml whose bytes differ from the digest-pinned, already-verified working-tree policy, that branch's sync stops with an operator-facing error and the overall sync exit status is non-zero; other branches still sync. Absence is not a mismatch. dest→source and decisions/0026's single verified ExcludeList are unaffected.
tags: [security, filtering, config, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

# Context

[decisions/0036](0036-branch-additive-exclusions.md)'s Context diagnoses a real
disclosure path and is not repeated here: [decisions/0026](0026-protected-versioned-policy.md)
loads one verified `ExcludeList` from the working tree per `sync` run, never a
branch-tip version (`src/commands/sync.rs::run`, ~lines 101-103); a feature
branch that adds sensitive content and correctly adds its own
`.gitprismignore` entry excluding it has that instruction discarded, and per
`design/log.md`'s still-open "a file that already reached dest and is later
added to `.gitprismignore` stays on dest forever" item, that disclosure is
irreversible. That diagnosis stands.

What changed is `AGENTS.md` (commit `e213f72`), which added a working-style
rule after 0036 was written:

> Prefer operator intervention over novel automation. Automate only when the
> safe behavior is deterministic and established by Git or credible prior
> art. If resolution requires guessing intent or weakening a Git safety
> invariant, fail clearly and let the operator resolve it. When evaluating a
> proposed automatic recovery, first ask whether Git already provides a safe
> primitive for it. If not, the default decision is to stop and involve the
> operator, unless the project owner explicitly decides the added automation
> is necessary.

0036's own Prior art section already found no precedent — among josh,
Copybara, git-subtree, git-filter-repo, or jujutsu — for per-branch or
per-commit additive filtering. Under the new rule, an unprecedented union of
an untrusted per-branch matcher into policy is novel automation, and the
default is to stop and involve the operator instead. The project owner has
explicitly chosen fail-closed over the union.

# Decision

In short: a source branch may omit `.gitprismignore`, but if present it must
exactly match the authenticated exclusion policy. A mismatch halts
synchronization for that branch. Other branches continue, but the `sync`
command exits non-zero.

Source→dest replay compares, per pending commit, the exact bytes of that
commit's tree entry for `.gitprismignore` and `.gitprism.toml` against the
corresponding bytes of `VerifiedPolicy.ignore_raw`/`config_raw` — the same
digest-pinned bytes decisions/0026 already verifies and loads once in
`sync::run` (~lines 101-103) before either sync phase starts. `run` currently
only binds `verified_policy.config`/`.exclude_list` out of that struct;
`config_raw`/`ignore_raw` remain on `verified_policy` unconsumed and must also
be threaded into `sync_pair_to_dest_with_key`.

**Absence is not a mismatch.** A replayed commit whose tree has no entry for
one or both filenames is not compared and does not stop the branch — the
digest-verified trusted policy already applies to that commit regardless of
what its own tree contains, so absence cannot weaken anything. Only a file
that is *present and different* is ambiguous: gitprism cannot tell an
approved-but-not-yet-repinned policy change from an attacker's edit, and that
ambiguity is exactly what the operator must resolve. Practical consequence:
`setup`'s graft commit and any history predating either control file replay
normally, since neither ever carries a differing control file.

**A present-and-byte-identical control file also does not stop the branch.**
Only a present-and-*different* file does.

**Per-branch stop, non-zero run status.** A mismatch stops replay for that
branch only. [decisions/0024](0024-sync-warns-and-continues-past-a-mirror-only-branch-with-no-shared-history.md)
already establishes the precedent for not letting one branch's problem block
every other branch: a mirror-only branch merely discovered by
[decisions/0017](0017-source-to-dest-mirrors-every-branch.md), with no shared
history, is reported as a per-branch `Outcome::Warning` and `sync` continues.
A policy mismatch is reported the same way structurally — `reporter.complete`
for that branch, then move on to the next — but with `Outcome::Error`, not
`Outcome::Warning`, because this is [decisions/0007](0007-conflict-policy-hard-stop.md)'s
hard-stop shape (a real conflict gitprism refuses to guess through), not
0024's benign "out of scope" shape. `run()` must not let that branch's
completed error line be the end of the story: the overall `sync` invocation's
exit status must be non-zero, so CI cannot mistake a halted branch for a
clean run. A security-relevant halt that exits `0` would be worse than the
bug it replaces — silent failure defeats the entire point of failing closed.

**The two categories must stay semantically distinct.** They are not two
shades of the same thing, and the difference must be encoded so it cannot
erode: a *warning* means the branch wasn't synced for a benign, known reason
and the run may still succeed; a *policy mismatch* means the branch wasn't
synced because safety could not be established, and the run must fail. A
later change must never reclassify a mismatch into 0024's warning semantics —
the two differ in whether the operator is obliged to act, not in severity of
wording. A dedicated outcome variant is acceptable if it makes that harder to
collapse by accident; `Outcome::Error` plus the mandatory non-zero exit status
satisfies it as written.

**What the operator is told.** The message names the branch, the offending
commit (oid), which control file differs (`.gitprismignore` or
`.gitprism.toml`, not both conflated), and the remedy: update the protected
`GITPRISM_POLICY_SHA256` deployment variable to the approved policy (via
`gitprism policy-hash` against the approved checkout), or reconcile the
branch so its control file matches the pinned policy exactly. No guessing, no
partial application — nothing for that branch is pushed to dest.

**No second policy source.** This adds a consistency *check*, nothing else.
It does not consult, honor, or merge a branch's own exclusions.
[decisions/0026](0026-protected-versioned-policy.md)'s "one verified
`ExcludeList` for every source-to-dest branch... does not load branch-tip
versions" stands unchanged — there is still exactly one `ExcludeList`, loaded
once, applied to every branch. 0036 said the opposite (a second,
per-branch-additive matcher); this decision does not reintroduce it in any
form.

# Why

**Fail-closed matches the new rule directly.** Git provides no safe primitive
for "which of these two differing policy files is the approved one" — that is
exactly a guess-intent question, and the rule's own worked example (rejecting
an expected-old-OID push lease because "a non-fast-forward rejection is
intentional control flow, not an error to engineer around") is the same
shape: a mismatch here is also intentional signal, not noise to route around.

**Per-branch scope, not whole-run.** Reasoned from existing precedent, not
invented: decisions/0024 already decided that a branch-local problem
shouldn't cost every other branch its sync, and decisions/0007 already
decided that a real conflict this tool refuses to guess through gets a hard
stop rather than a skip. This decision applies 0007's stop *scoped* the way
0024 already scopes a branch-local problem — one branch's `Outcome::Error`,
not an aborted run.

**Non-zero exit is not optional.** `Reporter`'s colored per-branch lines
(decisions/0020) are for a human watching scrollback; CI decides pass/fail
from the process exit status. A halted branch that still returns `Ok(())` to
`main` would report success to CI while dest silently never received that
branch's changes — indistinguishable, from the outside, from a boring
no-op sync. Stating this in the affirmative: the halt must be loud on both
channels, not just the one a human happens to be watching.

**Relationship to decisions/0018.** 0036 recorded that its union would
convert the leak into a branch silently never mirrored: a branch whose entire
remaining content becomes covered by its own new exclusions filters to a
no-op against a landing branch, so `already_merged_into_a_landing_branch`
(`src/commands/sync.rs`, ~line 641) classifies it as
already-merged-and-cleaned-up and it is never created on dest at all — the
disclosure moves rather than closes. Fail-closed does not have that side
effect: `already_merged_into_a_landing_branch` is untouched by this decision
— confirmed by reading it, ~line 641 — since no filtering behavior changes at
all. A branch's filtered content is exactly what it is today; a mismatching
branch produces an explicit, named, operator-facing halt instead of vanishing
into a benign-looking skip. This is a real advantage of fail-closed over
0036's union, not merely a wash.

**The control-file-only branch is the sanctioned workflow, not an edge
case.** The way an operator changes `GITPRISM_POLICY_SHA256` at all is to
create a branch whose only change is the new control file, verify it with
`gitprism policy-hash`, update the protected variable, then land the branch.
Under today's code, that branch's control-file-only diff, filtered, is
frequently a no-op against a landing branch's tree, and
`already_merged_into_a_landing_branch` would classify it as
already-merged-and-cleaned-up — silently never created on dest, with no
signal that anything needed the operator's attention. Under this decision, it
instead halts with an explicit named message. That is an improvement, not a
regression: the halt is exactly the operator gate this workflow needs — it
forces `GITPRISM_POLICY_SHA256` to be updated (or the branch reconciled)
before the branch's control-file change silently would have been absorbed one
way or the other.

This does **not** eliminate the misleading message for every control-file-only
branch, only for the security-relevant one. The halt fires while the branch's
file still differs from the pin. Once the operator has updated
`GITPRISM_POLICY_SHA256` and the working tree carries the approved policy, a
branch holding that same new file matches the pin, does not halt, and — since
both control files are self-excluded (`src/exclude.rs`) — still filters to a
no-op and was, at the time this decision was written, still reported as
"already merged into … , cleaned up there" despite never having existed on
dest. Correcting that message was a separate item, since resolved (the note
no longer claims deletion or prior existence); this decision fixes the
variant where safety could not be established, not the wording of the benign
one.

# Consequences

* **Files compared, and against what.** Per replayed commit's tree, source→dest
  only: `.gitprismignore` and `.gitprism.toml` entries, compared byte-for-byte
  against `VerifiedPolicy.ignore_raw`/`config_raw` (`src/policy.rs`) — the same
  bytes already verified against `GITPRISM_POLICY_SHA256` before parsing.
  Confirmed these bytes are available: `VerifiedPolicy` holds them, and
  `sync::run` (~lines 101-108) loads `verified_policy` once, before either
  sync phase, and currently only destructures `config`/`exclude_list` out of
  it — `config_raw`/`ignore_raw` need to be bound and threaded through too, no
  re-read or re-verification required.
* **Reads stay bounded.** Reading a replayed commit's control-file blob for
  comparison goes through the same [decisions/0032](0032-bounded-repository-controlled-data.md)
  `MAX_CONTROL_FILE_BYTES` bound (`src/limits.rs`) already governing every
  other control-file read; no new unbounded surface.
* **dest→source is unaffected.** `sync_pair_from_dest_with_key` takes no
  `ExcludeList` parameter and reads no control file at all; nothing about
  this decision touches it.
* **`already_merged_into_a_landing_branch` is unchanged**, verified by
  reading it (`src/commands/sync.rs`, ~line 641) — it still filters all three
  trees through the one verified `exclude_list` exactly as decisions/0018's
  addendum specifies. This decision adds a check that runs before/alongside
  replay, not a change to what gets filtered or how "already merged" is
  decided.
* **Honest cost.** Every legitimate branch-level control-file change now
  requires operator action — a `GITPRISM_POLICY_SHA256` update or a
  reconciling commit — before that branch can sync at all, including the
  sanctioned policy-change workflow itself. That friction is exactly what the
  new `AGENTS.md` rule accepts in exchange for never guessing intent; it is
  not free, and is not presented as free.
* **Tests the implementation commit must add:**
  * a branch where a replayed commit's control file is present and differs
    from the pinned policy — that branch halts, nothing for it is pushed to
    dest;
  * a branch where a replayed commit simply has no control file at all —
    replays normally, unaffected;
  * a branch whose control file matches the pinned policy byte-for-byte —
    syncs normally, not treated as a mismatch;
  * `run()`'s overall exit status is non-zero when any branch halts this way;
  * other branches in the same run still sync normally when one branch halts;
  * the control-file-only branch (the `GITPRISM_POLICY_SHA256`-change
    workflow itself): a branch whose only diff from its landing branch is an
    updated control file halts with the explicit mismatch message instead of
    being silently classified already-merged-and-cleaned-up by
    `already_merged_into_a_landing_branch`.

# Prior art

Not re-checked beyond 0036's own finding, which this decision relies on
directly: no per-branch or per-commit additive/consistency filtering
precedent exists among josh, Copybara, git-subtree, git-filter-repo, or
jujutsu. Where 0036 read that absence as license to invent one, this decision
reads the same absence, under the new `AGENTS.md` rule, as the reason not to.

# Addendum: an unreadable control-file entry is that branch's mismatch, not the whole run's

`read_control_file_blob` (`src/commands/sync.rs`) originally only handled two
outcomes for a tree entry: absent, or a blob whose bytes it returned for
comparison. A pending commit whose `.gitprismignore`/`.gitprism.toml` entry
was a directory or a submodule gitlink made `repo.find_blob` fail, and a blob
over `MAX_CONTROL_FILE_BYTES` hit an `anyhow::bail!` — both propagated via `?`
straight out of `find_control_file_policy_mismatch`, `sync_pair_to_dest_with_key`,
and `run` itself, aborting the entire invocation before any later branch got
its turn.

That contradicted this decision's own per-branch guarantee: safety not being
establishable for one branch's pending commit is exactly the shape this
decision already classifies as *that branch's* mismatch (`Outcome::Error`,
per-branch halt, run continues, non-zero exit at the end) — not a run-wide
abort that lets one branch's malformed tree deny service to every other
branch. A non-blob entry or an oversized blob is safety-not-established in
the same sense a differing byte sequence is; neither can be compared against
the pinned policy, so neither can be trusted.

`read_control_file_blob` now returns a classification (`ControlFileRead`:
absent, blob, not-a-regular-file, too-large) instead of erroring on the
latter two, and `find_control_file_policy_mismatch` maps them onto a
`PolicyMismatchReason` so the operator-facing message states what was
actually wrong instead of always claiming a byte mismatch. Absence is
unaffected — still not a mismatch.
