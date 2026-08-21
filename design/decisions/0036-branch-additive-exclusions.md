---
type: Decision
title: A branch's own exclusions add to the trusted policy, never subtract from it
description: Effective source→dest exclusion is the authenticated deployment policy OR each replayed commit's own .gitprismignore, evaluated as independent matchers so untrusted branch content can only add exclusions, never negate the trusted ones; amends decisions/0026's "does not load branch-tip versions of the ignore file" clause.
tags: [security, filtering, config, branches]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-21T00:00:00Z }
---

# Context

[decisions/0026](0026-protected-versioned-policy.md) states plainly: "One
`sync` invocation uses one verified `ExcludeList` for every source-to-dest
branch. It does not load branch-tip versions of the ignore file." Confirmed
in `src/commands/sync.rs::run` (~lines 101-103): `verified_policy.exclude_list`
is loaded once, from the working tree, before either sync phase starts, and
the same `ExcludeList` value is passed into every branch's
`sync_pair_to_dest_with_key` call.

[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) mirrors every
branch discovered on source to dest, with no config entry required.

Combined, an external security review found a real disclosure path: a
feature branch that adds sensitive content and, in the same or a later
commit, correctly adds its own `.gitprismignore` entry saying "do not
export this" has that instruction silently ignored — only the working
tree's policy governs what's excluded, not the branch's own. The content is
mirrored to dest regardless of what the branch itself says must never leave
source.

This is not just wrong filtering for one invocation. `design/log.md` records
a still-open item: a file that already reached dest and is *later* added to
`.gitprismignore` stays on dest forever, because the diff/merge model has no
delta to filter against — dest is fast-forward-only and there is no
deletion commit. So the disclosure above is irreversible: by the time the
branch's own exclusion could take effect (if it ever did), the content it
names is already on dest permanently.

`src/exclude.rs` (~lines 51-62) already excludes `.gitprismignore` and
`.gitprism.toml` themselves unconditionally and un-negatably, before
consulting the matcher at all. That is self-exclusion of gitprism's own
control files, not exclusion of arbitrary branch-named content, and is
untouched by this decision.

# Decision

Effective source→dest exclusion is:

```text
trusted.is_excluded(path) || branch.is_excluded(path)
```

`trusted` is decisions/0026's digest-verified `ExcludeList`, loaded once
from the working tree, unchanged. `branch` is a second `ExcludeList`, built
per branch per run, from the union of every `.gitprismignore` found across
the commits `pending_commits` replays for that branch this run
(`build_pending_dest_tip`) — not the branch tip alone. gitprism replays
commits individually, so a branch that adds `internal/` in commit 1 and
deletes that line again in commit 3 must still have commit 1's addition
honored: commit 1's content already reaches dest before commit 3 is ever
applied, and — per the log item above — content that reaches dest cannot be
withdrawn afterwards. A pattern named by any replayed commit becomes an
exclusion for the whole branch's replay; none is forgotten because a later
commit deleted the line.

The two matchers stay independent — never concatenated into one `Gitignore`
build. `branch` is consulted through `is_excluded`, never mixed as raw
pattern text into `trusted`'s own matcher.

`branch` needs no authentication and is deliberately outside
`GITPRISM_POLICY_SHA256`. The rest of decisions/0026 stands unchanged: the
deployment policy remains digest-pinned, still verified before parsing, and
is still the *minimum* exclusion set — `trusted`'s patterns can never be
weakened by anything a branch does. Only 0026's "does not load branch-tip
versions of the ignore file" clause is amended, and only in the direction of
adding exclusions, never removing them.

# Why

**Independent matchers, not concatenation.** Concatenating `branch`'s lines
into `trusted`'s pattern list would let a branch-supplied `!negation` line
interact with the trusted policy and un-exclude protected content — handing
untrusted repository input the power to widen what's exported, the exact
opposite of what this decision exists to prevent. Two independent matchers,
ORed by result, keep `branch` monotone: it can only ever add an exclusion,
never remove one, because a `!` line inside it can only negate a pattern
within that same matcher. This is the property that makes it safe to
consult untrusted repository input for an exclusion decision at all.

**Union over every replayed commit, not the tip.** `build_pending_dest_tip`
applies `pending_commits`' output one commit at a time, each merged onto
dest's growing chain tip (decisions/0016). A tip-only branch matcher would
drop commit 1's `internal/` line the moment commit 3 removes it, even though
commit 1's content already applied against dest earlier in the same replay.
Reading the union across every replayed commit preserves monotonicity for
the whole replay, the same property that makes `trusted` itself safe to
consult.

**Why the digest is not extended to cover branch content.**
`GITPRISM_POLICY_SHA256` pins one static value with no per-branch dimension
— it authenticates *a* policy, not *this branch's* policy, so requiring a
branch's file to match it would just reproduce today's bug (one fixed value,
blind to what any given branch asks to exclude). Because the branch matcher
is monotone, it needs no authentication in the first place: under the
union, untrusted input can only subtract from what's exported, never add
exposure. A future reader who "fixes" this unauthenticated read by gating it
behind the digest would reintroduce the disclosure this decision closes.

# Consequences

* **The stated disclosure path is closed.** A branch that adds sensitive
  content and its own exclusion for that content no longer has the
  exclusion discarded — `branch.is_excluded` is consulted for every commit
  in the replay.
* **Remaining limitation, stated honestly.** A branch that introduces
  sensitive content *without* adding a corresponding exclusion is still
  exported — gitprism cannot infer sensitivity from content alone. This is
  qualitatively different from the problem closed here: today's bug
  discarded an instruction the branch actually gave; this leftover gap is
  the branch never giving one at all.
* **Interacts with decisions/0018's deleted-branch heuristic.** Once a
  branch can add its own exclusions, a branch whose entire content is
  covered by those new exclusions filters to a no-op against a landing
  branch's tree. `already_merged_into_a_landing_branch`
  (`src/commands/sync.rs`, ~line 641) then finds the filtered trees equal
  and classifies the branch as already-merged-and-cleaned-up (decisions/0018,
  Case 2), so it is never created on dest at all. This moves the failure
  mode rather than removing it: the same branch that leaks today becomes a
  branch that is silently never mirrored. Not a new failure shape —
  decisions/0018 already accepts it ("a branch that looks 'already merged'
  purely by coincidence... is treated identically") for a different cause;
  this decision just adds a new way to reach it.
* **Which exclusion list that comparison uses.** `already_merged_into_a_landing_branch`
  filters all three trees (base, landing, branch) through the *candidate
  branch's own* effective (union) list on both sides of the comparison —
  the two filtered trees must be comparable to each other, and the landing
  branch's own tree was never filtered against the candidate branch's
  exclusions before. Consequence: a branch can make itself look merged by
  excluding its own remaining content. No *content* is lost either way —
  dest never receives what's excluded regardless of which list is compared
  — only the presence of the dest ref, the same tradeoff decisions/0018
  already accepted; not re-decided here.
* **Implementation shape.** `ExcludeList` (`src/exclude.rs`) gains the
  ability to hold more than one matcher; `is_excluded` returns true if *any*
  held matcher excludes the path. Every existing `filter_tree` call site
  (`src/commands/sync.rs`) is unchanged — each already just calls
  `exclude_list.is_excluded`.
* **Control-file self-exclusion is unchanged.** `.gitprismignore` and
  `.gitprism.toml` remain unconditionally and un-negatably excluded
  (`src/exclude.rs`, ~lines 51-62), checked before either matcher,
  regardless of anything a branch's own file says.
* **Cost.** The ignore file must now be read from each replayed commit's
  tree, not once per run — bounded per read by decisions/0032's existing
  `MAX_CONTROL_FILE_BYTES` limit (`src/limits.rs`), so this is a per-commit
  repeat of an already-bounded read, not a new unbounded surface.
* **Scope.** This applies to source→dest only. dest→source
  (`sync_pair_from_dest_with_key`) takes no `ExcludeList` parameter at all
  and is unaffected.
* **Tests the implementation commit must add:**
  * a branch that adds sensitive content and its own exclusion in the same
    commit — the content must not reach dest;
  * a branch that adds sensitive content in one commit and the exclusion
    for it in a later commit — the earlier commit's content must still be
    excluded (proves union-over-replay, not tip-only);
  * a branch that adds an exclusion, then a later commit removes that line
    again — the earlier exclusion must still apply to the commits it
    covered (proves a later commit can't retroactively un-exclude);
  * a branch `!negation` line must not un-exclude anything `trusted`
    excludes (proves matcher independence);
  * a branch whose entire content becomes excluded by its own added
    exclusions is classified as already-merged by
    `already_merged_into_a_landing_branch` and not recreated on dest
    (documents the decisions/0018 interaction above, not a bug);
  * `ExcludeList` holding two matchers: excluded by either one, excluded by
    neither, and negation scoped to its own matcher only.

# Prior art

Checked `design/references/` for per-branch or additive filtering precedent
before deciding: none found. josh's workspace file is a single, versioned,
tip-read filter with no per-branch or per-commit dimension — the same
tip-only shape decisions/0026 already modeled gitprism on. Copybara's
workflow config names path filters once per workflow, not per source
revision. git-subtree and git-filter-repo both operate on a fixed,
externally supplied path set for the whole rewrite. jujutsu doesn't do
cross-repo filtering at all. No precedent found among the tools already
reviewed for a second, untrusted, per-commit-additive exclusion source
layered on top of a trusted one.

## Superseded (2026-08-21): see decisions/0037

**[decisions/0037](0037-branch-policy-mismatch-fails-closed.md)** replaces
this decision's union with a fail-closed halt: a mismatching control file
stops the branch instead of being merged into policy. This file's own Prior
art section found no precedent for the union it proposed, and `AGENTS.md`
(commit `e213f72`, added after this decision) now defaults novel automation
of exactly that shape to "stop and involve the operator." The Context above
— the disclosure path this decision diagnoses — is still correct and is
referenced, not repeated, by 0037.
