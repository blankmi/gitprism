---
type: Decision
title: An undecodable advertised dest ref makes the branch listing incomplete, never a fabricated name
description: `git::remote_branch_names` stops decoding `git ls-remote --heads` output with `String::from_utf8_lossy`. An advertised ref whose name isn't valid UTF-8 is dropped from `names` and marks the listing incomplete — the same signal decisions/0046 Addendum 2, Finding F already defined for hitting the branch limit — so reconstruction degrades to a per-branch refusal instead of fetching a U+FFFD name that cannot exist and aborting the whole run. `RemoteBranchListing.truncated: bool` becomes a completeness type so the operator-facing reason distinguishes the two causes. Amended 2026-08-31 (review finding M-03): incompleteness carries every cause rather than the first one seen, and only the branch-limit horizon blocks a "dest has no branch by this name" claim — an unrelated undecodable ref no longer disables decisions/0046 Addendum 2, Finding G's deleted-ref recovery.
tags: [architecture, branches, git, error-handling, byte-safety]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-31T00:00:00Z }
---

# Context

[decisions/0030](0030-byte-safe-git-data.md) requires Git data to be handled
as bytes and unsupported non-UTF-8 refs to be *rejected* before mutation,
never transformed. `git::remote_branch_names` (`src/git.rs`) violates that
rule at the one boundary where dest's ref names enter gitprism:

```rust
let stdout = String::from_utf8_lossy(&output.stdout);
```

A repository review (2026-08-31, finding M-02) traced what that lossy
conversion does to a destination that advertises a branch whose name
contains an invalid UTF-8 byte:

1. The invalid bytes become U+FFFD, producing a name gitprism invented and
   no ref anywhere is called.
2. `validate_branch_name` — `git2::Branch::name_is_valid` — accepts U+FFFD,
   so the fabricated name survives the loop's own validity filter and lands
   in `names`.
3. `reconstruct_mapping_index` (`src/commands/sync/anchor.rs`) seeds
   `run_cache.dest_ref_exists.insert(name, true)` for every listed name, so
   the fabricated branch is recorded as *existing on dest*.
4. `fetch_dest_head_for_reconstruction` therefore fetches it. The fetch
   fails, because that ref does not exist.
5. The failure path refreshes the listing through the same function, which
   repeats the same lossy conversion and reports the same fabricated name
   as still present — so the "dest ref is gone, treat it as absent" escape
   at that site is never taken, and the fetch error propagates.

Reconstruction's completeness is a whole-run fact, so that propagated error
aborts the entire invocation. A single malformed or hostile ref on dest
denies every branch — including every correctly configured one — its sync,
for a ref the source-side operator may have no ability to remove. The
function's own doc comment claims a name that fails validation is "skipped
rather than failing the whole listing"; for this input class it was neither
skipped nor honestly reported.

[decisions/0046](0046-dest-anchors-come-from-exact-authenticated-mappings.md)
Addendum 2, Finding F already faced the structurally identical question for
a *different* cause of an unreadable dest branch — more than
`MAX_SOURCE_BRANCHES` of them — and answered it: return what was read plus
an explicit incompleteness signal, and let reconstruction taint its exact
lookups (`MappingIndex::note_incomplete_reconstruction`) so the run degrades
to per-branch refusals rather than a whole-run abort.

# Decision

1. `remote_branch_names` parses `git ls-remote --heads` output **as bytes**.
   Lines are split on `\n`, the ref field is taken as a byte slice, and only
   a slice that is valid UTF-8 and passes `validate_branch_name` becomes a
   `String` in `names`. `String::from_utf8_lossy` is not used on ref bytes
   anywhere in this path.

2. An advertised `refs/heads/*` ref whose name is not valid UTF-8 is
   **skipped and makes the listing incomplete**. It is never fabricated into
   a replacement-character name, and never counted as an existing dest
   branch.

3. `RemoteBranchListing.truncated: bool` becomes
   `incomplete: Option<ListingIncomplete>`, with one variant per cause —
   the `MAX_SOURCE_BRANCHES` horizon (Finding F, unchanged behaviour) and an
   undecodable advertised ref, carrying that ref's `git::escape_bytes`
   rendering. Every existing `!listing.truncated` guard becomes
   `listing.incomplete.is_none()`, so both causes suppress the negative
   existence claim ("dest has no branch by this name") identically.

4. `reconstruct_mapping_index` passes the cause's description to
   `MappingIndex::note_incomplete_reconstruction`, so the resulting
   per-branch refusal names *why* the listing could not be trusted rather
   than reporting a branch-limit horizon for a malformed-ref cause.

5. A ref line that is well-formed UTF-8 but fails `validate_branch_name`
   keeps today's behaviour: skipped, listing still complete. That is
   decisions/0024's precedent for a discovered oddity gitprism can't act on,
   and — unlike an undecodable name — such a ref is one gitprism has fully
   read and positively judged.

# Why

Three options were weighed against the finding.

**Skip and keep the listing complete.** Simplest, and it removes the
fabricated name. Rejected: reconstruction would then claim it scanned all of
dest while a dest branch it could not decode may carry authenticated markers
for a source commit already observed. That is exactly the "a visible mapping
hides an unscanned contradiction" hole decisions/0046 Addendum 1 closed;
reintroducing it silently is worse than the abort being fixed.

**Fail the whole listing.** The strictest reading of decisions/0030's
"reject before mutation". Rejected: it keeps the property this finding is
about — one dest-controlled ref halting every branch's sync — and merely
makes the halt intentional. Finding F already rejected that asymmetry for
the branch-count cap on the same reasoning: dest's ref names are not
something a source-side operator can necessarily fix, and gitprism has never
constrained dest that way.

**Skip and mark the listing incomplete.** Chosen. It satisfies
decisions/0030 (nothing is transformed; the undecodable ref is rejected, not
renamed), preserves decisions/0046's completeness invariant (an unread dest
branch taints exact lookups), and matches AGENTS.md's preference for a clear
operator-visible degradation over both a silent guess and an avoidable
whole-run stop. It also needs no new mechanism: the incompleteness channel
from `remote_branch_names` through `note_incomplete_reconstruction` to a
per-branch refusal already exists and is already tested.

# Consequences

* A single undecodable ref on dest makes every exact mapping lookup that run
  conservative, so branches that would otherwise anchor exactly halt per
  branch (decisions/0045) until dest's ref name is corrected. Correctly
  named branches still sync. This is a real availability cost, deliberately
  taken over an unsound "complete" claim.
* `RemoteBranchListing`'s shape changes; `remote_branch_names`'s doc comment
  no longer claims every invalid name is skipped without consequence.
* Reconstruction cannot fetch a name that no ref carries, so the refresh
  loop in `fetch_dest_head_for_reconstruction` stops being reachable by this
  input class at all.
* The fix is byte-level, so it needs a test that advertises a genuinely
  byte-invalid ref from a real destination repository rather than a name
  containing a literal U+FFFD character, which is valid UTF-8 and would pass
  the old code unchanged.

# Addendum (2026-08-31): incompleteness is a set of causes, and only the branch-limit horizon blocks an absence claim

Review finding M-03 against this decision's own implementation: point 3 above
said "every existing `!listing.truncated` guard becomes
`listing.incomplete.is_none()`, so both causes suppress the negative existence
claim identically". That is too strong, and it disabled a safe recovery.

`fetch_dest_head_for_reconstruction` treats a failed fetch as a *deleted* dest
ref only when the refreshed listing can support the claim "dest has no branch
by this name" (decisions/0046 Addendum 2, Finding G). Under this decision's
first implementation, one unrelated undecodable ref anywhere on dest made the
listing incomplete, so a valid branch genuinely deleted between the listing and
its fetch propagated its fetch error and aborted the whole run — reintroducing,
by a different route, the availability failure this decision was written to
remove.

The two causes are not equivalent for that claim:

* **The branch-limit horizon** stops the scan. An unlisted name may still have
  a dest ref beyond the horizon, so absence is genuinely unprovable.
* **An undecodable ref** does not stop the scan — every later line is still
  read (this is tested). Every decodable, valid branch dest advertises is
  therefore still in `names`. An undecodable name cannot *be* the branch being
  asked about either: that branch's name is valid UTF-8, so their bytes
  differ. Absence of a distinct valid branch stays fully established.

Both causes still taint the *mapping index* globally and identically: an
undecodable dest branch may carry authenticated markers for a source commit
already observed, and that is what the global taint exists for. Only the
per-branch absence claim is separable.

Amendments to the decision above:

1. Incompleteness is a **set of causes**, not one cause. Point 3's
   `incomplete: Option<ListingIncomplete>` is replaced by a
   `ListingCompleteness { branch_limit: bool, undecodable_ref: Option<String> }`
   that records both. The single-cause enum was recording only whichever cause
   came first, so a listing that hit an undecodable ref *and then* the branch
   limit reported only the undecodable ref — under which the split below would
   wrongly conclude absence was establishable. Carrying both is what makes the
   distinction safe to draw at all.
2. The completeness type exposes `can_establish_absence()`, true unless the
   branch-limit horizon was hit. Every site claiming a branch does *not* exist
   on dest uses it: the deleted-ref recovery in
   `fetch_dest_head_for_reconstruction`, and the negative-existence seeding for
   unlisted source branches in `reconstruct_mapping_index`. The latter also
   restores what that site did before this decision for this input class,
   without the fabricated name.
3. `describe()` names every recorded cause, so a refusal caused by both a
   horizon and an undecodable ref says so rather than reporting one of them.
