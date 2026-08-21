## 2026-08-13

**Creation**: Bootstrapped the `design/` OKF bundle. Captured the initial workflow as `requirements/0001-workflow-and-scope.md` and recorded prior-art research on josh, git-subtree, and git-filter-repo under `references/`.

**Update**: Decided [decisions/0001](decisions/0001-build-bespoke-rust-tool.md) — bespoke Rust engine, josh as design reference only, not a dependency.

**Update**: Added [references/copybara](references/copybara.md) — Google's origin/destination migration tool; contributes a workflow-mode vocabulary (SQUASH/ITERATIVE/CHANGE_REQUEST) and a stateless, commit-trailer-based alternative to josh's git-notes+cache for where sync bookkeeping lives.

**Update**: Added [references/jujutsu](references/jujutsu.md) and decided [decisions/0002](decisions/0002-hybrid-git-backend.md) — git2-rs for local object/merge work, real `git` subprocess for push/fetch, following jj's precedent.

**Update**: Decided [decisions/0003](decisions/0003-mapping-state-in-commit-trailers.md) — commit-message trailers (`Gitprism-Source-Commit` / `Gitprism-Dest-Commit`) hold the sync state; same mechanism gives resume-point and loop-prevention for free. Precedented by Copybara's `GitOrigin-RevId` and git-subtree's `git-subtree-split` trailers.

**Update**: Decided [decisions/0004](decisions/0004-exclude-list-versioned-in-source.md) — exclude-list is a committed, gitignore-style file inside source, following josh's workspace-file precedent.

**Update**: Decided [decisions/0005](decisions/0005-branch-pairs-are-a-configured-list.md) — branch pairs are a configured list from day one; an initial "single pair, generalize later" framing was argued down on the grounds that N pairs cost the same as 1 once trailer scans are recognized as already ref-scoped.

**Update**: Decided [decisions/0006](decisions/0006-setup-uses-real-shared-history.md) — source's initial commit is a real child of dest's tip commit (git-subtree `add`-style), so every future source<->dest merge has a native git merge-base instead of a hand-rolled substitute.

**Update**: Decided [decisions/0007](decisions/0007-conflict-policy-hard-stop.md) (hard-stop on real conflicts) and [decisions/0008](decisions/0008-ship-resolve-helper.md) (ship a `gitprism resolve` helper so a human can act on that from outside the pipeline run that hit it).

**Update**: Decided [decisions/0009](decisions/0009-push-race-refetch-and-recompute.md) — a lost ff-only push race on the source→dest side is handled by refetch-and-recompute, not rebase.

**Update**: Added the `playbooks/` bundle and [playbooks/0001](playbooks/0001-gitlab-pipeline-triggers.md) — GitLab CI trigger setup (push for source→dest, push-piggyback + manual + schedule for dest→source) is deployment guidance, not a gitprism architecture decision, since the tool is trigger-agnostic by construction.

**Update**: Decided [decisions/0010](decisions/0010-preserve-author-stamp-committer.md) (preserve author, gitprism stamps committer — matching git's own cherry-pick/rebase convention) and [decisions/0011](decisions/0011-exclude-list-is-gitignore-syntax.md) (exclude-list is `.gitprismignore`, exact `.gitignore` syntax, self-excluding by default). These close out the remaining open questions from `requirements/0001`.

## 2026-08-14

**Update**: First code lands. All architecture decisions were stable and no
requirements were open, so this was a scaffolding choice, not a design decision:
`cargo init`, `clap` (derive) for the CLI, `anyhow` for error handling. Three
subcommands stubbed out matching the design directly — `setup` (decisions/0006),
`sync` (playbooks/0001 — one command, both directions, per configured pair),
`resolve <pair>` (decisions/0008) — each currently just fails loudly with "not yet
implemented" rather than pretending to work. No git logic yet; no config format
decided yet (`--config` flag exists but nothing parses it).

**Update**: Decided [decisions/0012](decisions/0012-config-versioned-in-source.md) —
config is `.gitprism.toml`, versioned in source (same as the CI pipeline file the
user already adds themselves), self-excluding like `.gitprismignore`
([decisions/0011](decisions/0011-exclude-list-is-gitignore-syntax.md)). `setup`
reads it from disk before source's first commit exists, since source has no history
yet at that point; every later command reads it from the committed tree.

**Update**: First `setup` implementation lands, plus a clarification to
[decisions/0012](decisions/0012-config-versioned-in-source.md): "no repo yet" at
`setup` time means no *commit history* yet, not no git repository — `setup` requires
an already-`git init`'d, completely empty repo (discovered by walking upward from cwd,
same as `git` itself), erroring like any other git command outside a repo rather than
auto-`init`-ing one. Also settled through code review: `setup` grafts every configured
pair (not just one primary branch), rolls back this run's branches if a later pair
fails partway through (including a checkout conflict), and checks out the first
pair's branch with a safe (non-forced) checkout so a stray local file that collides
with dest's content is reported as a conflict rather than silently overwritten.

**Update**: `sync`'s source→dest direction lands (dest→source, which can hit
[decisions/0007](decisions/0007-conflict-policy-hard-stop.md)'s conflict hard-stop, is
still unimplemented). Same discovery convention as `setup`
([decisions/0012](decisions/0012-config-versioned-in-source.md)): no source-location
config, cwd is a git-style discovery start point. Per configured pair: fetch dest's
tip, find the resume point by scanning dest's history for the most recent
`Gitprism-Source-Commit` trailer ([decisions/0003](decisions/0003-mapping-state-in-commit-trailers.md)),
falling back to `merge_base(source, dest)` — always `setup`'s original graft
parent — the first time a pair is ever synced. Every pending source commit is
filtered using *that commit's own* `.gitprismignore`, not the current tip's, so a
change to the exclude-list takes effect from the commit that made it, not
retroactively ([decisions/0004](decisions/0004-exclude-list-versioned-in-source.md),
[decisions/0011](decisions/0011-exclude-list-is-gitignore-syntax.md)); a commit that
filters to no change from its parent is skipped entirely rather than pushed empty
(requirements/0001); a commit already carrying `Gitprism-Dest-Commit` (dest→source's
own prior output) is skipped as loop prevention. The whole pending chain is built as
loose commit objects and pushed by raw oid in one `git push <oid>:<dest_branch>` —
real `git`'s own non-force default is what actually enforces fast-forward-only here,
not custom logic. A rejected (non-fast-forward) push is handled by refetching dest and
recomputing the chain from scratch against its new tip, bounded to 3 retries
([decisions/0009](decisions/0009-push-race-refetch-and-recompute.md)) — no new
decision needed for any of this, it's what 0003/0004/0009/0011 already specify.

**Update**: Code review caught three real bugs in that first `sync` pass, all fixed:
(1) the exclude-list was read per-*commit* from that commit's own historical tree,
contradicting [decisions/0004](decisions/0004-exclude-list-versioned-in-source.md)'s
explicit "apply the current list at processing time, not a historical
reconstruction" — fixed by loading it once per pair, from source's current tip, before
filtering anything. (2) source→dest was replacing dest's entire tree with source's
filtered snapshot on every push; since dest→source isn't implemented yet, dest can
carry independent content (e.g. a PR merged straight to dest) that snapshot silently
drops even though the ref update is a legitimate fast-forward — fixed by refusing to
sync a pair at all unless dest's tip is a point gitprism itself already accounts for
(its own last push, or unmoved since `setup`'s graft), rather than picking a
merge/patch policy that's properly dest→source's job. (3) every push failure was
retried as if it were the fast-forward race
[decisions/0009](decisions/0009-push-race-refetch-and-recompute.md) describes,
including auth/hook/network failures that must fail immediately — fixed by
classifying `git push`'s own rejection wording and only retrying a genuine
non-fast-forward rejection.

**Update**: A fourth review round caught a real gap in fix (2) above: the
`Gitprism-Source-Commit` trailer on dest's tip is trusted as the resume boundary
without checking that *this* clone's own source branch actually descends from it. A
second, divergent clone of source (e.g. one that hasn't fetched a commit another
clone already synced) would otherwise rebuild its own full snapshot on dest and
silently drop that already-synced content — a fast-forward ref update that's still a
real data loss. Fixed by requiring `source_tip` to be a descendant of the trailer's
commit (`Repository::graph_descendant_of`) before accepting it, treating "this clone
doesn't even have that object" (which a sibling clone's un-pushed source commit never
would be) the same as a confirmed non-ancestor — both refuse the sync with the same
message pointing at fetching the latest source history.

**Update**: Decided [decisions/0013](decisions/0013-repo-urls-optional-fall-back-to-env-vars.md)
— `[source].url`/`[dest].url` become optional in `.gitprism.toml`, falling back to
`GITPRISM_SOURCE_URL`/`GITPRISM_DEST_URL` when omitted, so a credential-bearing or
per-environment remote URL never has to be committed into source's own history. This
surfaced while starting dest→source: that direction needs somewhere to push its
result (source's own remote), which the config schema had never named.

**Update**: `sync`'s dest→source direction lands (decisions/0003, 0006, 0007, 0008,
0013). Per configured pair, after source→dest: fetch dest's tip, find the resume
boundary by scanning *source's own* history for the most recent
`Gitprism-Dest-Commit` trailer (a real scan, unlike source→dest's tip-only check —
source's tip moves for reasons that have nothing to do with dest→source), cherry-pick
every pending dest commit not already loop-prevented (`Gitprism-Source-Commit`
present) onto source's tip via `git2`'s `cherrypick_commit`, and push the result to
source's own remote — ff-only, same push/race-retry mechanics as source→dest. A real
conflict hard-stops the pair (decisions/0007): whatever cherry-picked cleanly is
still pushed, the conflict is reported with the exact commit/branch for
`gitprism resolve` (decisions/0008, still a stub) to act on later. `run` now runs
dest→source *before* source→dest per pair — source→dest's own refusal check
(`dest_resume_point`) needed a third recognition case (source's history already
carries a marker naming dest's tip exactly) so it stops treating dest's independent
content as unrecognized once dest→source has already reflected it in, which only
works if dest→source has already had its turn this run.

**Update**: Implementing that ordering surfaced a real, pre-existing bug: source→dest
built every pushed commit as a *full filtered snapshot* of that source commit's
entire tree, safe only because source was previously guaranteed a strict superset of
dest. Once dest→source can land content on source's tip ahead of an older,
not-yet-pushed source commit, that older commit's full-snapshot would silently
*regress* dest's independent content back out — and this isn't rare, it's the normal
case once both directions run against actively-developed repos. Decided
[decisions/0014](decisions/0014-source-to-dest-becomes-diff-based-and-can-conflict.md):
source→dest now applies each pending commit as its own filtered diff (via `git2`
diff + `apply_to_tree`, filtering excluded-path deltas *before* application, not
after) onto dest's growing chain tip, instead of a snapshot replace — behavior-
identical to the old approach for any linear history (every pre-existing test), but
correct once dest→source is in the mix. This supersedes
[decisions/0009](decisions/0009-push-race-refetch-and-recompute.md)'s claim that
source→dest has "no merge semantics at all": it can now genuinely conflict too, the
same real dest↔source content-divergence shape decisions/0007 already describes for
dest→source, and gets the identical hard-stop treatment.

**Update**: Code review caught three real bugs in the dest→source landing above, all
fixed, no new decisions needed: (1) a cherry-picked dest commit that merges to a true
no-op (its content already matched source) was skipped entirely without a marker
commit — since the resume boundary *is* the newest `Gitprism-Dest-Commit` trailer on
source's history (decisions/0003), this left that trailer stuck on an older dest oid
forever, permanently blocking source→dest from ever recognizing the real dest tip as
accounted for. Fixed by always building a marker commit, even a content-empty one —
unlike source→dest's own empty-commit rule (requirements/0001), that rule was never
meant to apply to this direction. (2) after a successful dest→source push, the local
checkout's own branch ref was force-moved directly, without touching the working
tree/index — left the checkout looking dirty relative to its own HEAD, and could
silently discard a locally-diverged commit on a retry (which rebuilds against a
freshly-*fetched* remote tip, not necessarily this checkout's local view). Fixed with
a dedicated `advance_local_source_branch`: verifies the move is a real fast-forward of
the branch's current local value first, and if that branch is the one actually
checked out, updates the working tree via a safe (non-forced) checkout instead of a
bare ref write. (3) `GITPRISM_SOURCE_URL`/`[source].url` was resolved unconditionally
at the top of `sync_pair_from_dest`, so a config that legitimately omits it (per
decisions/0013, valid whenever a pair never needs to push anything to source) still
failed the whole run even when nothing was pending. Fixed by resolving it lazily, only
at the point a push (or a retry's refetch) actually needs it.

**Implemented `gitprism resolve`** (decisions/0007, 0008), filling the gap those two
decisions left open. Settled first, in conversation: how the human tells gitprism a
resolution is done. Decided an explicit `--continue` flag
([decisions/0015](decisions/0015-resolve-real-git-cherry-pick-explicit-continue.md)),
matching git's own `rebase`/`cherry-pick`/`merge --continue` convention rather than
having the bare command guess start-vs-resume from `.git/CHERRY_PICK_HEAD` on its own.

`gitprism resolve <pair>` recomputes the exact same pending-dest-commit list `sync`
would build next (a new shared `pending_dest_commits` helper, factored out of
`sync_pair_from_dest`'s inline boundary/ancestor-check so the two can never disagree),
takes the oldest one, and drives a **real** `git cherry-pick` subprocess against it —
deliberately not `git2`, unlike every cherry-pick `sync` itself does (which stays
entirely in the object database and never touches the working tree). Decisions/0008
promises the human "100% standard git" conflict-resolution — real files, real
markers, `git add` — which only exists if the conflict is reproduced against the
actual working tree. A clean apply is finished immediately, no human needed; a real
conflict leaves ordinary conflict markers and stops (told apart from any other
subprocess failure by git's own exit-code convention: `1` conflict, everything else a
plain error).

Whichever way it applied, gitprism never trusts git's own auto-committed result — it
would carry whatever `user.name`/`user.email` this checkout's local git config has,
not gitprism's configured committer, and no `Gitprism-Dest-Commit` trailer. Finishing
means rebuilding the commit via `build_source_commit`, made `pub(crate)` so `resolve`
reuses the exact function `sync` itself calls: original author preserved, gitprism's
committer stamped, trailer appended (decisions/0003, 0010) — then pushed to source,
ff-only (decisions/0009), a single attempt rather than `sync`'s refetch-and-recompute
retry loop, since `resolve` is a rare, human-supervised path.

Scoped to dest→source's cherry-pick conflict shape only for now — source→dest's own
conflict (decisions/0014, a failed diff-apply rather than a cherry-pick) isn't wired
into `resolve` yet, tracked as a follow-up in decisions/0015's "Consequences".

A real subprocess conflict turned up a fixture-only gotcha worth remembering: building
a commit via `git2`'s `repo.commit()` moves the branch ref but does *not* by itself
keep the working tree/index in step with it — `sync`'s own test fixtures never needed
to care (its cherry-pick/diff-apply logic never touches the checkout), but `resolve`'s
real `git cherry-pick` subprocess refuses to run at all against a stale index
("your local changes would be overwritten"). Fixed in the test fixtures with a forced
`checkout_head` after each fixture commit; not a product bug, since real checkouts
built by actual `git` commands never have this problem.

**Update**: Code review caught three real bugs in `resolve`, all fixed, no new
decisions needed. (1) `resolve --continue` read `CHERRY_PICK_HEAD` and stamped/pushed
whatever commit it named without ever checking that commit was actually the pair's own
expected next pending dest commit — a cherry-pick a human started by hand (unrelated
to gitprism, or the wrong pair entirely) would get labeled with a `Gitprism-Dest-Commit`
trailer as if it were, silently corrupting resume/loop-prevention (decisions/0003) for
both directions from then on. Fixed by recomputing `pending_dest_commits` at the top of
`resolve --continue` too (using the current HEAD as source_tip — a conflicted or
not-yet-committed cherry-pick never advances HEAD, only `CHERRY_PICK_HEAD` records
which commit is being picked) and refusing to continue unless `CHERRY_PICK_HEAD`'s oid
matches `pending.first()` exactly. (2) A human resolving a real conflict by keeping
source's existing content outright (a legitimate resolution, discarding dest's incoming
change entirely) produces an empty merge result — git's own `cherry-pick --continue`
refuses to finish that without being told `--allow-empty`, which this code
misinterpreted as an unresolved conflict, reporting no conflicted paths and never
building the marker commit sync's own resume-trailer invariant requires even for a
no-op dest pick (decisions/0003, mirroring `sync`'s own no-op-cherry-pick rule). Fixed
two ways: the initial `cherry_pick` now always passes `--empty=keep` (git's flag for
exactly this, not accepted by `--continue`), and `cherry_pick_continue` tells a real
remaining conflict apart from an empty-but-resolved one by checking for unmerged paths,
finishing the latter by hand with `git commit --allow-empty` (which clears the
sequencer state exactly like a normal `--continue` would). (3) The local branch-ref
update in `finish` was documented as "a plain, non-forced local update" but actually
passed `force: true` with no check at all — a concurrent local ref move in that window
would have been silently overwritten, contrary to the stated safety guarantee. Fixed
by switching to `git2`'s `reference_matching` (an atomic compare-and-swap): the update
now only succeeds if the branch still names the exact commit gitprism's own cherry-pick
just produced, and fails loudly (`GIT_EMODIFIED`) otherwise instead of clobbering
whatever moved it.

**Update**: An end-to-end review — driving the built binary against real two-repo
fixtures rather than only the unit suite — found three real bugs in source→dest, all
fixed, no new decisions taken. Every one of them sits in the space
[decisions/0014](decisions/0014-source-to-dest-becomes-diff-based-and-can-conflict.md)'s
equivalence argument explicitly excluded ("any purely linear history — every existing
source→dest test"), which is also exactly what the test suite covered.

(1) **Binary files could never reach dest.** `apply_filtered_diff` built its diff with
default `DiffOptions`, so libgit2 emitted binary deltas with no payload and
`apply_to_tree` failed with `ApplyFail` — the same error code a genuine text conflict
produces, so a binary file was reported as a decisions/0007 conflict and the pair
hard-stopped, with no recovery path (`gitprism resolve` is dest→source only,
decisions/0015). Fixed with `show_binary(true)`.

(2) **Already-synced source commits were re-applied whenever dest's tip was an
independent dest commit** (a merged PR — the normal operating case of
[playbooks/0001](playbooks/0001-gitlab-pipeline-triggers.md)). `dest_resume_point`
answered two different questions with one value: "is dest's tip safe to build on?" and
"which source commits does dest already have?". Its independent-dest-commit case
returned the *graft point* as the revwalk boundary, so `pending_commits` re-yielded
every commit gitprism had already pushed. Where the patch re-applied cleanly this
silently duplicated content on dest (an already-synced `line1` became `line1\nline1`);
where it didn't, it was misreported as a conflict and the pair was bricked — every
later run failed identically, and `resolve` answered "nothing pending from dest".
Fixed by splitting the two questions: `dest_tip_is_accounted_for` keeps the tip-only
safety recognition unchanged, while the boundary now comes from `newest_source_marker`,
a real scan of dest's own history for the newest `Gitprism-Source-Commit` trailer
(decisions/0003), falling back to the graft only when dest carries no gitprism-written
commit at all. An unusable newest marker (absent from this clone's odb, or not an
ancestor of `source_tip`) refuses rather than falling back to an older one, which would
be the same bug with a wider blast radius. This also strengthens the divergent-clone
guarantee: a clone that never fetched the source commit dest was last synced from is
now refused even when dest's tip is an independent commit, where it previously pushed
its own divergent history onto dest.

(3) **A merge commit in source duplicated the merged content on dest.**
decisions/0014 specifies each pending commit is applied as its diff "against its
immediate parent, mainline for merge commits", but the revwalk has already applied the
side-branch commits individually by then, and `apply_to_tree` is patch application, not
a three-way merge, so a repeated add appends instead of no-op'ing: an ordinary
`git merge --no-ff` put `line1\nline1` on dest and reported success. Fixed with a
source-space cursor — each pending commit is diffed from the *previously examined*
pending commit, so the deltas telescope from the boundary with nothing repeated and
nothing skipped. Identical to `parent(0)` for linear history (so 0014's equivalence
argument still holds there), it preserves a hand-resolved "evil" merge's own content
(unlike skipping merge commits), and it needs no `parent_count` special-casing, so root
commits and octopus merges fall out for free. The cursor must advance across *skipped*
commits too — a loop-prevented commit's content came from dest and is already there, so
leaving the cursor behind would push dest's own content back at dest.

This is also why the same bug never existed on dest→source: `cherrypick_commit` is a
three-way merge, and three-way merging an identical change is idempotent.

**Open, not yet decided** (these need a conversation, not a patch):

* ~~decisions/0014's stated mechanism no longer describes the implementation, and its
  equivalence argument should be restated.~~ — resolved, superseded by
  [decisions/0016](decisions/0016-both-directions-merge-via-real-git-merge-tree.md):
  0014's original "diff against the commit's own parent" is correct as written once the
  merge is idempotent.
* ~~Should source→dest move from patch application to a real three-way merge over
  pre-filtered trees, matching dest→source's cherry-pick?~~ — resolved, see
  [decisions/0016](decisions/0016-both-directions-merge-via-real-git-merge-tree.md),
  which moves *both* directions to a real `git merge-tree`. Fix (3) left a known wart
  the cursor cannot avoid: when source's history interleaves two branches, the
  intermediate dest commit for whichever branch the walk emits second is a state that
  never existed on source (it temporarily removes the other branch's files, restored by
  the merge's own commit). The tip is always correct, but a conflict hard-stopping
  mid-chain can leave dest fast-forwarded *to* such a commit — verified: dest's tip
  ended up missing a file source had pushed one commit earlier. Three-way merge fixes
  both the duplication and the churn at the root; it also changes conflict detection
  from `ApplyFail` to `index.has_conflicts()`, and 0014's reason for filtering before
  applying is satisfiable by pre-filtering the trees instead.
* ~~Trailers are not pair-qualified: both marker scans accept any `Gitprism-*-Commit`
  trailer regardless of which branch pair wrote it, so dest branches merged into one
  another can hand a pair the other pair's marker.~~ — resolved, see
  [decisions/0019](decisions/0019-marker-scans-are-first-parent-only.md), which makes
  both scans first-parent-only rather than pair-qualifying the trailer itself.
* ~~`trailer_value` matches `Key: value` anywhere in a message rather than only in the
  final trailer block, so a commit message that merely *quotes* a trailer (a squash
  merge concatenating bodies, say) can poison the resume scan — verified: it stops the
  pair with "isn't an ancestor of dest's current tip", and that commit's content never
  reaches dest.~~ — resolved, see
  [decisions/0025](decisions/0025-authenticated-mapping-markers.md), which makes resume
  and loop-prevention trust only the single canonical final state block, authenticated
  by an HMAC over the commit itself; a quoted trailer can't produce a valid MAC, so it
  parses as ordinary text rather than a trusted marker.
* A file that already reached dest and is *later* added to `.gitprismignore` stays on
  dest forever; the diff model has no delta to filter. decisions/0004's "apply the
  current list at processing time" reads as though it should be scrubbed, which would
  need an explicit deletion commit since dest is fast-forward-only.
* `gitprism resolve` still doesn't cover source→dest's conflict shape
  (decisions/0015's tracked follow-up), yet `sync`'s own source→dest conflict message
  tells the operator to run it — and it answers "nothing pending from dest".

**Update**: Decided [decisions/0016](decisions/0016-both-directions-merge-via-real-git-merge-tree.md)
— both directions stop doing their own merge work and delegate to a real
`git merge-tree --write-tree` subprocess over pre-filtered trees, replacing
source→dest's non-idempotent patch application *and* dest→source's
`cherrypick_commit`. This closes two of the open questions above (decisions/0014's
wording, and the three-way merge question) and, in doing so, removes the cursor that
had just been introduced: with an idempotent merge, decisions/0014's original "diff
against the commit's own parent"
mechanism is correct as written, and the interleaved-branch churn plus the stranding
exposure both disappear.

The owner's decision rule drove it — "what git already does well should be done by
git, not reinvented", alongside "don't solve everything automatically, let an operator
do it". Prior art was checked first (their third rule) and is cited in 0016: josh's own
reverse-apply turns out to be a real `git2` three-way merge with a hard-error on
ambiguity, not patch application; jj gets idempotency from merge algebra (`A+(A-B)=A`,
documented as "what Git and Mercurial do"); Copybara and git-subtree both need a
working tree and both keep separate trailer bookkeeping; git's own docs treat patch
application as the fallback rather than the mechanism; and `git merge-tree --write-tree`
is git's non-experimental, no-working-tree plumbing for precisely this, already used in
production by GitLab's Gitaly for server-side merges. `git replay` was considered and
rejected — experimental, and it handles neither merge commits nor root commits.

Also filled a long-standing gap in [references/josh](references/josh.md): the "reverse
merge mechanics could not be confirmed from the docs" note is now answered from josh's
actual source, with permalinks, including the four open issue reports that live in that
same reconciliation path.

Consequence for [decisions/0002](decisions/0002-hybrid-git-backend.md): its "local
object-graph work — including the dest→source merge — goes through `git2-rs`" clause no
longer holds for the merge itself. `git2` keeps everything else local (revwalks, trailer
scans, tree construction and filtering, commit building, ancestry checks); only the
merge moves to the subprocess side. 0002's reason for choosing git2 over gix — that
gix's merge support was incomplete and "merge is core, not incidental" — is moot for the
same reason.

decisions/0016 is written as `status: draft` with no `verified` stamp: the decision is
the owner's, the write-up isn't, so it needs his review before it counts as settled.

**Update**: Decided [decisions/0017](decisions/0017-source-to-dest-mirrors-every-branch.md)
— the two directions no longer share one configured list. Prompted by the owner
spelling out the actual development workflow for the first time: feature branches are
created ad hoc off source's main, pushed to source, filtered and synced to dest,
merged into dest's main when finished, and that merge is what syncs back; source's
main also takes direct commits independent of any feature branch. decisions/0005's
symmetric shared-pairs-list model doesn't fit that — a static config can't track
branches created at an unpredictable rate with unpredictable names.

source→dest now discovers every branch on source at run time and mirrors each one
under its own name — no config entry, no renaming. dest→source keeps an explicit
configured list, simplified from `{source_branch, dest_branch}` pairs to plain branch
names, since there's nothing left to remap and only a small set of long-lived branches
(main, typically) legitimately need content ported back — feature branches are
transient and disappear once merged into dest's main. Branch deletion was raised and
explicitly decided against: gitprism never deletes a branch on either side, matching
the project's existing "don't auto-fix, fail loud" rule — a stale mirrored branch on
dest is left for an operator to clean up.

Only a lightweight prior-art check so far (git's own `refs/heads/*:refs/heads/*`
mirroring treats "every branch, same name" as the unremarkable default; gitprism still
needs its own enumeration step since each branch has to be filtered and merge-tree'd
per decisions/0016 before it can be pushed, unlike a raw mirror push) — not yet
checked with 0016's rigor against josh's own ref-handling. Flagged in 0017 itself as a
reason it isn't marked stable yet. No code has changed for this decision: config
schema, source→dest's discovery loop, and test reshaping are all still open, deferred
until the write-up itself is confirmed.

**Update**: Implemented decisions/0017. `Config.pairs: Vec<BranchPair>` is gone;
`Config.branches: Vec<String>` replaces it, TOML shape `branches = ["main", "release-2.0"]`
in place of `[[pairs]]` blocks. `setup` grafts every name in that list the same way it
grafted pairs before — `graft_pair` is now `graft_branch`, keyed by a plain string.
`sync::run` no longer loops "per pair, both directions" — it runs dest→source for every
branch in `config.branches` first, *then* discovers every local branch git2 reports on
source (`repo.branches(Some(BranchType::Local))`, sorted for deterministic order) and
runs source→dest for each one found, config entry or not. `sync_pair_to_dest`/
`sync_pair_from_dest` both take a plain `branch: &str` now instead of a `&BranchPair`.
`gitprism resolve`'s CLI argument was renamed `pair` → `branch` to match (its own
`config.pairs.find(source_branch == ...)` lookup is now `config.branches.find(...)`) —
not called for by the concrete task list, but `BranchPair`'s removal left nothing else
for "pair" to mean.

One real gap surfaced only once the end-to-end tests were written, not foreseen by the
write-up itself: source→dest's existing "refuse to build on a dest tip we don't
recognize" safety check (decisions/0009) assumed a same-named dest branch always
already exists — true for every pair `setup` had grafted, false for a brand-new branch
discovered by decisions/0017 that dest has never seen at all. Fetching a nonexistent
dest ref just fails, which isn't the right shape for "nothing to be unsafe about yet."
Fixed with a new `git::remote_ref_exists` (a real `git ls-remote --exit-code` check,
run before deciding whether to fetch at all): when dest genuinely has no branch by this
name yet, source→dest seeds its chain from the nearest `Gitprism-Dest-Commit` trailer
already reachable in the new branch's own ancestry (inherited from whatever branch it
was created from, typically one `setup` grafted) instead of from a dest ref that isn't
there — no fetch needed for that content either, since it's the literal parent object
of some ancestor commit already sitting in source's own object database.

Added seven end-to-end tests in `commands::sync`'s style (real on-disk source/dest
repos, driving the public `run()` entry point) and two in `git`'s: one branch that gets
mirrored to dest with zero config entry and its excluded path still stripped
(`run_mirrors_an_ad_hoc_source_branch_with_no_config_entry`); one confirming dest→source
never reflects a non-configured branch's independent dest content back into source
— asserted via source→dest's own pre-existing safety refusal correctly firing on a
branch nothing will ever bring back into "recognized" state
(`run_does_not_pull_back_independent_content_from_a_non_configured_branch`); and
`git::remote_ref_exists`'s own true/false cases. Every existing pair-shaped test was
reshaped rather than rewritten from scratch — `BranchPair { source_branch, dest_branch }`
constructions became a plain `&str`, `[[pairs]]` TOML literals became `branches = [...]`
placed *before* any `[section]` header (TOML scopes a bare key to whichever table
preceded it, so the config shape sketched when this decision was made — `branches`
listed after `[dest]` — silently nested it inside `Dest` and dropped it every time
during the actual TDD pass; corrected here without changing the field itself).

`cargo test`: 82 passed, 0 failed. `cargo clippy --all-targets`: clean. `cargo fmt
--check`: clean. decisions/0017's own frontmatter is untouched (`status: draft`) —
same precedent decisions/0016 set: implementing the code doesn't settle the owner's own
review of the write-up.

## 2026-08-17

**Update**: Manual testing against `sync.rs` surfaced two of decisions/0017's own
deferred "deletion-adjacent edge cases," with opposite correct answers. Decided
[decisions/0018](decisions/0018-branch-deletion-failure-modes.md): (1) a
round-tripped branch (`config.branches`) whose dest ref has been deleted is a genuine
error — `sync_pair_from_dest` fetched it unconditionally and let git's own raw
"couldn't find remote ref" text leak through as an unhelpful, run-aborting failure;
now checked with `git::remote_ref_exists` first (the same idiom `sync_pair_to_dest`
already uses for a mirror-only branch's first sync) and failed loudly with a clear,
gitprism-authored message instead. (2) a mirror-only (discovered, unconfigured)
branch whose dest ref has been deleted *after* being merged into a round-tripped
branch via an ordinary PR is expected, routine cleanup, not something to resurrect —
previously indistinguishable from "never synced yet," so `sync_pair_to_dest`
unconditionally rebuilt and repushed it every run, undoing the cleanup.

Prior art checked before choosing how to tell the two cases apart: GitLab's own
push-mirror feature documents this exact asymmetry as intentional product behavior
("When a branch is merged into the default branch and deleted in the source
project, it is deleted from the remote mirror on the next push. Branches with
unmerged changes are kept." —
[GitLab docs](https://docs.gitlab.com/user/project/repository/mirror/push/)); and
git-trim ([foriequal0/git-trim](https://github.com/foriequal0/git-trim)), a real
tool built around exactly this "merged vs. stray" classification for local
branches, does it with no persisted state at all — recomputed from the object graph
every run — and explicitly content-based rather than oid-ancestry-based, since it
"can detect common merge styles such as merge with a merge commit, rebase/ff merge
and squash merge," the last of which leaves no ordinary ancestor relationship to
check. Case 2's fix reuses exactly that shape: for each landing branch in
`config.branches`, compute `merge_base(branch_tip, landing_tip)` in source's own
history and run the same `git merge-tree` primitive decisions/0016 already uses
(base = merge-base tree, ours = landing's tree, theirs = branch's tip tree); if the
result is clean and equals landing's tree unchanged, the branch's content is already
fully present there and its missing dest ref is left alone rather than recreated.

decisions/0018 is written as `status: draft` with no `verified` stamp — same
precedent as 0016/0017: the decision is the owner's, the write-up isn't, so it needs
his review before it counts as settled.

**Implemented decisions/0018.** `sync_pair_from_dest` (`src/commands/sync.rs`) now
calls `git::remote_ref_exists` before its dest fetch and `anyhow::bail!`s a clear,
gitprism-authored message naming the branch when it comes back `false`, instead of
calling `git::fetch` unconditionally and letting git's own raw "couldn't find remote
ref" subprocess error abort the run. `sync_pair_to_dest`'s `!dest_ref_exists` branch
now calls a new `already_merged_into_a_landing_branch` helper — for each branch named
in `config.branches`, compute `merge_base(branch_tip, landing_tip)` in source's own
history and run the same `git merge-tree` primitive decisions/0016 already uses (base
= merge-base tree, ours = landing's tree, theirs = branch's tip tree); a clean merge
whose result equals landing's tree unchanged means the branch's content is already
fully present there — before falling through to the existing rebuild-and-push
behavior, and returns early (logging, not erroring) the moment it finds a match.

Writing the test for Case 2 surfaced a real, pre-existing, already-logged gap
(above, "Trailers are not pair-qualified"): a first draft of the test simulated a PR
merge as a genuine two-parent git commit naming gitprism's own mirrored commit for
the feature branch as the second parent — realistic, but it folded that commit's own
`Gitprism-Source-Commit` trailer into the round-tripped branch's ancestry, which made
`newest_source_marker`'s unrelated, already-known "accepts any `Gitprism-*-Commit`
trailer regardless of which branch wrote it" gap misidentify the *feature* branch's
own commit as the round-tripped branch's resume boundary, wrongly failing the
round-tripped branch's own sync with "isn't at a point this clone can safely build
on." Not a decisions/0018 bug — worked around in the test fixture with a
single-parent commit (the same tree, without the second parent), since a squash-merge
PR is just as realistic a fixture and doesn't touch the unrelated gap at all.

A second case the decision's first draft didn't anticipate, caught by the
pre-existing `run_mirrors_an_ad_hoc_branch_with_no_commits_of_its_own` regression
test still needing to pass: a brand-new branch with zero commits of its own is
trivially "already merged" into whatever landing branch it was cut from (their trees
are identical, since nothing has diverged yet from either side) — which would have
wrongly suppressed decisions/0017's own guarantee that even a zero-commit branch
still gets its first mirror created. Fixed by skipping a landing branch entirely when
`branch_tip` equals its own `merge_base` with that landing branch — there's nothing
to have been merged or cleaned up if the branch never diverged from it in the first
place; this guard is now written into decisions/0018's own "Consequences" section
too.

Four new tests in `commands::sync`: one for Case 1 (deleting a round-tripped branch's
dest ref via `repo.find_reference(...).delete()`, after moving the bare dest repo's
own `HEAD` elsewhere first since git2 refuses to delete a bare repo's current `HEAD`
branch, and confirming sync fails with gitprism's own "out of sync" message rather
than a raw `git fetch` failure); one driving the full three-sync-run sequence Case 2
describes (mirror a feature branch, merge it into dest's main via a real
single-parent commit as a squash-merge PR stand-in, let dest→source reflect that
merge back into source, delete the feature branch's dest ref, then confirm a third
sync does *not* recreate it); one confirming a genuinely unmerged remainder
(simulating a squash merge that only captured part of the branch, leaving a second
file behind) still gets rebuilt and pushed normally — this one already passed before
any code changed, serving as the regression guard for the fall-through path; and the
pre-existing zero-commit-branch regression test above, unmodified, now also proving
the merge-base equality guard.

`cargo test`: 86 passed, 0 failed. `cargo clippy --all-targets`: clean. `cargo fmt
--check`: clean.

**Update**: Decided [decisions/0019](decisions/0019-marker-scans-are-first-parent-only.md)
— resolves the "trailers are not pair-qualified" entry above, which
decisions/0018's own Case 2 test had already run into and worked around (its
test body explicitly avoided a real two-parent merge for exactly this
reason). Reproduced directly: mirror a feature branch to dest
(decisions/0017), merge its dest tip into a round-tripped branch's dest tip
via a real, two-parent `git merge` (not decisions/0018's squash-shaped
single-parent stand-in), and the round-tripped branch's own dest→source sync
— run in the same invocation right after — has `newest_source_marker` walk
into the merged-in branch's own `Gitprism-Source-Commit`-bearing commit
(reachable via the merge's second parent) before ever reaching the
round-tripped branch's own boundary, returning a source-space oid the
round-tripped branch never descends from and hard-stopping the whole sync
with "isn't at a point this clone can safely build on" — a false refusal,
not a real one.

Fixed with `Revwalk::simplify_first_parent()` (confirmed against the
installed `git2 = "0.21.0"`) in both `newest_source_marker` and
`newest_dest_marker`: git's own `--first-parent` history-simplification
mechanism, so a merge commit's non-first parents (where a merged-in branch's
own commits live) are never reachable by either scan at all. Documented,
not engineered around: this assumes the tracked branch stays first-parent
of its own merges — true for GitHub/GitLab/Azure DevOps' "merge PR" button
and for `git merge` run from the target branch, not for a merge performed
the other way around.

**Implemented decisions/0019.** Both `newest_source_marker` and
`newest_dest_marker` (`src/commands/sync.rs`) now call
`revwalk.simplify_first_parent()` alongside their existing `push`/
`set_sorting` calls; their doc comments were rewritten to explain why the
walk is first-parent-only now instead of full-ancestry, and to name
decisions/0019's documented limitation (the tracked branch must stay
first-parent of its own merges).

New regression test,
`run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge`:
mirrors a feature branch to dest (decisions/0017), merges it into a
round-tripped `main` on dest via a real two-parent commit (main first,
feature-x's own gitprism-authored mirror commit second — decisions/0018's
own fixtures deliberately used a single-parent stand-in instead, exactly to
avoid this), then runs `sync` again. Confirmed to fail before the fix with
the false "isn't at a point this clone can safely build on" refusal this
decision's Context section traces in detail, and to succeed after it, with
dest's `main` left unmoved (it already carries everything source has) and
source's `main` correctly carrying feature-x's content via dest→source.

decisions/0018's own two test fixtures that had explicitly worked around
this gap (their comments said so) had their comments updated to point at
the new test and at decisions/0019 rather than describe an open gap that no
longer exists — their fixtures themselves are unchanged, since each still
means to exercise its own decision in isolation.

`cargo test`: 87 passed, 0 failed — no regressions in the existing
multi-parent-merge tests
(`run_does_not_duplicate_a_no_ff_merges_content_on_dest`,
`run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`),
decisions/0018's own Case 1/Case 2/fall-through tests, or
`newest_dest_marker`'s "always finds setup's own graft commit" guarantee —
all of those already keep the tracked branch as first parent of its own
merges. `cargo clippy --all-targets`: clean. `cargo fmt --check`: clean.

## 2026-08-18

**Fixed a real bug in decisions/0018's Case 2 check**, documented as an
addendum to [decisions/0018](decisions/0018-branch-deletion-failure-modes.md)
rather than a new decision — the design itself ("content-based, via
`git merge-tree`, checked against a landing branch's current tip") was
already right; the code just compared the wrong trees.
`already_merged_into_a_landing_branch` ran its three-way merge over raw,
unfiltered source-side trees, while every other cross-side content
comparison in `sync.rs` (`build_pending_dest_tip` in particular) filters
through `filter_tree`/the current exclude-list first, since dest only ever
sees the filtered subset of source (decisions/0004, 0011). A mirror-only
branch's own commit touching an excluded path (`.gitprismignore`) alongside
an ordinary mirrored change — routine in a source-is-a-superset repo — made
the raw `theirs` tree carry content the landing branch's raw `ours` tree
never received and never will, so the merge came back clean but unequal to
`ours`, and the function wrongly concluded "not merged." `sync_pair_to_dest`
then resurrected the branch on dest every single run, undoing the PR's own
cleanup — not an edge case, but the ordinary case for any project that
excludes anything.

Fixed by filtering `base_tree`, `landing_tree`, and `branch_tree` through
`filter_tree` before the merge, using the same `exclude_list`
`sync_pair_to_dest` already loads once per run — passed in as a new
parameter rather than reloaded. No new state, no second filtering mechanism.

New regression test,
`run_does_not_resurrect_a_mirror_only_branch_merged_except_for_excluded_paths`:
same fixture shape as decisions/0018's own Case 2 test, with
`.gitprismignore` on `main` excluding `secret.txt` and `feature-x`'s own
commit touching both `feature.txt` and `secret.txt` together. Confirmed to
fail against the pre-fix code (third sync recreated `feature-x` on dest) and
pass once the fix landed.

`cargo test`: 88 passed, 0 failed. `cargo clippy --all-targets`: clean.
`cargo fmt --check`: clean.

## 2026-08-18 (later)

**Drafted [decisions/0020](decisions/0020-sync-status-output-is-a-pinned-progress-display.md)**:
`sync`'s status output (five plain `eprintln!` call sites, all in
`src/commands/sync.rs` — confirmed via grep that `setup`/`resolve` have
nothing equivalent) becomes an `indicatif`-backed display, following
Gradle's own rich/plain console duality: a pinned overall progress bar plus
current-step line at the bottom, completed branch-operations scrolling
above as colored, scannable summary lines (green/yellow/red for
done/skipped/error; cyan for round-tripped branch names vs. plain for
decisions/0017's mirror-only ones; explanatory text demoted to a note line,
Cargo's convention), with the bar's total computed upfront — source's branch
list read once before either sync phase starts, not discovered mid-run — and
a full plain-text fallback whenever stderr isn't a terminal (the CI case
`design/playbooks/0001` already documents). Dimming the mirror-only majority
was considered and rejected: it borrows a "de-emphasize" meaning that
doesn't fit (they're decisions/0017's normal, zero-config case, not a lesser
one) and reads worse on a fast scan than a plain line. Purely a
presentation-layer decision — no change to merge/push/conflict logic, and no
existing regression test asserts on stderr text. Not yet implemented.

## 2026-08-18 (later still)

**Drafted [decisions/0021](decisions/0021-setup-accepts-a-clean-clone-of-dest.md)**:
raised by the project owner — `git clone <dest-url> source && cd source &&
gitprism setup` is a plausible, arguably more natural bootstrap sequence than
the one `setup` currently requires, but it's rejected outright today, since a
plain clone always leaves a local branch checked out and `setup`'s precondition
(decisions/0012's "completely empty repo, no branches, no commits") is an
unconditional gate. Resolved by narrowing that gate: a local branch is now
accepted if it's in `config.branches` *and* its tip is identical (by oid) to a
fresh fetch of dest's own current tip for that name — reusing setup's existing
fetch-everything-upfront pass, no new fetch mechanism. Anything else (an
unrecognized branch name, detached HEAD, or a local branch whose tip diverges
from dest's, including a previous `setup` run's own graft commit sitting one
commit ahead) still hard-fails exactly as before. Amends decisions/0012's
Consequences rather than replacing them. Not yet implemented — `setup.rs`'s
precondition check, its ordering relative to config parsing, and
`rollback_branches`' reset-vs-delete distinction for pre-existing branches all
still need to change; no code written yet.

## 2026-08-18 (later still)

**Drafted [decisions/0022](decisions/0022-setup-gains-an-interactive-first-run-wizard.md)**:
raised by the project owner — when `setup`'s config file doesn't exist yet,
prompt for it interactively instead of just failing. Settled through
conversation: the wizard asks for dest's URL (prefilled from an existing
`origin` remote — the decisions/0021 clone-first case), branches (`git
ls-remote --heads` against dest, `main`/`master`/`develop`/`release*` sorted to
the top, plus a manual free-text catch-all, since dest can have far more
branches than fit on one screen), source's URL (skippable per decisions/0013),
and committer identity (prefilled from local git config) — then offers to
repoint (or add) the local `origin` remote to source's URL, since this
checkout is source going forward. A missing config with no attached terminal
(checked via `console::user_attended()`) still hard-fails exactly as today.
Prior-art check: `dialoguer` (same vendor family as `console`/`indicatif`,
already dependencies) has no filtering on its `MultiSelect`; `inquire` does
(type-to-filter plus page size), which is the actual feature needed once dest
has more branches than fit on screen — chosen over staying in the existing
vendor family for that reason specifically. Not yet implemented — no code
written, no new dependency added yet.

## 2026-08-18 (yet again)

**Implemented decisions/0021**: config is now parsed before `setup.rs`'s
precondition check runs (the check needs `config.branches` to recognize
expected branch names), and the old single `has_existing_branches ||
repo.head().is_ok()` gate is replaced with a narrower one folded into the
existing "fetch every branch's dest tip before writing anything" loop, no
second fetch pass. A detached HEAD, or any local branch whose name isn't in
`config.branches`, still hard-fails with the same message as before. A local
branch that is in `config.branches` may now already exist, provided its tip
is identical to a fresh fetch of dest's own current tip for that name;
otherwise setup hard-fails with `"gitprism setup: source's local branch
{branch:?} already has content that doesn't match dest's current tip —
refusing to graft over independent history"`. `graft_branch` needed no
change at all — confirmed `repo.commit(Some(refname), ...)` already enforces
fast-forward-from-current-tip itself (libgit2's `update_ref` requires the
first parent to be the ref's current target when the ref already exists),
which is exactly a no-op graft onto a verified-matching pre-existing branch.

`rollback_branches` now works off a `TouchedBranch` list carrying, per
branch, `original_oid: Option<Oid>`, and resets a pre-existing branch back
to its original oid instead of deleting it, while a branch setup created
fresh this run is still deleted, exactly as before — threaded through both
the commit-phase and checkout-failure rollback call sites.

New tests: `run_succeeds_against_a_pre_existing_branch_matching_dests_tip`
(a local branch already sitting at exactly dest's fetched tip — the "clean
clone" case — grafts normally, same assertions as the original happy-path
test), `run_fails_loudly_when_a_pre_existing_branch_diverges_from_dests_tip`
(a local branch with real independent history under a configured name hard
fails, naming the branch and dest's tip in the message), and
`run_rolls_back_a_pre_existing_branch_to_its_original_tip_when_a_later_branch_fails`
(two configured branches, one pre-existing and matching dest's tip, the
other forced to fail its ref write via the existing lock-file trick —
confirms the pre-existing branch survives at its original oid, not deleted
and not left on the graft commit, while the branch this run would have
newly created is not left behind). All pre-existing tests kept their
original intent unchanged, including
`run_fails_loudly_against_a_non_empty_source_repo`, which still hard-fails
because its `"unrelated"` branch name isn't in config.

`cargo test`: 111 passed, 0 failed. `cargo clippy --all-targets`: clean.
`cargo fmt --check`: clean.

## 2026-08-18 (still going)

**Drafted [decisions/0023](decisions/0023-setup-adopt-flag-merges-dest-into-real-independent-history.md)**:
surfaced by actually running decisions/0021's build against the project
owner's real GitLab repo (`~/work/catenax-connector`) — it hard-failed, and
diagnosis showed why: that repo isn't a clone of dest at all (`origin` is
source's own GitLab URL), has real independent commits on `main` predating
any of this, and carries two other local branches (`ai-setup`, `backup`) not
in `config.branches`. Neither decisions/0006's single-parent graft nor
decisions/0021's identical-to-dest check has any way to express "this history
already existed independently before dest was involved."

Resolved through conversation: a new `--adopt` flag lets a configured
branch's real independent history merge with dest instead of hard-failing,
via the same `git merge-tree --write-tree` primitive decisions/0016 already
uses for both sync directions (base = git's empty tree, since no common
ancestor exists by construction; ours = the branch's own tree; theirs =
dest's tip) — clean merges get `.gitprism.toml`/`.gitprismignore` inserted
and commit with two parents (branch's own previous tip first, dest's tip
second, ordinary `git merge` convention), a real conflict hard-stops per
decisions/0007, naming the paths, with no `-X ours`/`-X theirs`
auto-resolution — the project owner's own answer. `--adopt` requires
`HEAD` to already be on the branch being adopted (no forced checkout, no
moving `HEAD`), and only ever affects that one branch. Separately amends
decisions/0021: setup no longer enumerates every local branch and requires
each to be in `config.branches` — it now only ever looks at branches
actually named there, so `ai-setup`/`backup` are left alone entirely, per the
project owner's explicit answer, rather than blocking the run just for
existing.

Prior art backing the shape, both already in this project's own references:
`git subtree add`'s actual primary use case is merging external history into
an *already-existing* repo (decisions/0006 had only followed its "graft onto
emptiness" shape until now); `git merge --allow-unrelated-histories` is git's
own precedent for gating a no-common-ancestor merge behind an explicit flag
rather than inferring intent — directly justifying `--adopt` as an opt-in
rather than auto-detected behavior.

Left deliberately open: what an operator does after `--adopt` hard-stops on
a conflict. No `resolve`-equivalent exists for this one-time case yet — for
now the hard-stop message points at completing a real
`git merge --allow-unrelated-histories` by hand and stamping
`Gitprism-Dest-Commit` themselves, since every other command only depends on
that trailer's presence, not on how the commit was produced. Not yet
implemented — no code written.

## 2026-08-18 (once more)

**Status audit, no design change**: decisions/0016 through 0021 were all
sitting at `status: draft` with no `verified` stamp despite each having its
own "Implemented decisions/00XX" entry above and a matching commit
(`48068ca`, `ae601b5`, `ff74994`, `d774484`, `bf95ae1`, `bde40ab`) — confirmed
by cross-checking git log against `src/`: `git::merge_tree` is live in
`sync.rs` (0016), branch discovery/mirroring and the deletion checks are in
place (0017, 0018), both marker scans call
`Revwalk::simplify_first_parent()` (0019), the `indicatif` progress display
is wired into `sync` (0020), and `setup.rs`'s precondition check is the
narrowed per-branch oid comparison (0021). decisions/0022 and 0023 were left
at `draft`, correctly — neither `dialoguer`/`inquire` nor an `--adopt` flag
exists anywhere in `src/` or `Cargo.toml` yet, matching their own "Not yet
implemented" log entries above. Flipped 0016-0021 to `status: stable` with a
`verified: [{ by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }]`
stamp, same shape 0001-0015 already use — the project owner confirmed this
counts as his review, having used/exercised each through today's check.

## 2026-08-18 (yet once more)

**Revised decisions/0023**, replacing the `--adopt`-flag draft above before
any of it was implemented. Talking through the design, the project owner
pushed back on the flag itself: "I can create a new folder, run git init,
create a config and then run gp setup... where is the difference if my local
state is the same as on dest for the branches configured?" Tracing what
`graft_branch` actually does confirmed the point — decisions/0021 already
works by reusing that one function unchanged, because libgit2's
first-parent-must-match-current-ref-target check passes trivially when local
tip equals dest tip. Empty repo, clean clone of dest, and real independent
history aren't three cases needing three mechanisms; they're one
reconciliation operation (merge-base, then `merge_tree`) with three points on
a spectrum.

decisions/0023 now generalizes 0021's oid-equality precondition into a real
merge-base check (`repo.merge_base`, already used elsewhere in `sync.rs`'s
`graft_point` and `already_merged_into_a_landing_branch`): a merge-base
existing means reconcile via decisions/0016's `merge_tree` primitive into a
real two-parent commit, hard-stopping on any real conflict per decisions/0007
exactly as before; no merge-base at all is a permanent, unconditional
hard-fail with no flag to bypass it. Asked directly whether to keep any
override for the one edge a clean merge-tree result can't catch — two
unrelated trees with zero overlapping paths merging silently with no
conflict to hard-stop on — the project owner's answer removed the override
question entirely: "If the history is unrelated fail, no flag to work around
it." Combining truly unrelated histories is left to real git, deliberately,
before `setup` ever runs again.

Superseded, not just amended: 0021's mechanism becomes a special case of
0023's general rule rather than a separately-implemented precondition.
Renamed the file from
`0023-setup-adopt-flag-merges-dest-into-real-independent-history.md` to
`0023-setup-reconciles-pre-existing-branches-via-merge-base.md` to match.
Still not yet implemented — no code written; still needs a subagent-delegated
TDD implementation, independent review, and explicit go-ahead before
committing, matching how decisions/0021 was actually built.

## 2026-08-18 (and once more)

**Implemented decisions/0023.** Delegated to a subagent with a detailed
TDD-first spec covering the merge-base generalization, the two-parent commit
ordering, the new "no history in common" hard-fail wording, and the subtle
test-suite consequence that an unconfigured local branch (`ai-setup`,
`backup`) must now be *invisible* rather than blocking, which flips the
expectation of the old `run_fails_loudly_against_a_non_empty_source_repo`
test entirely. `cargo test`/`clippy`/`fmt` all came back clean from the
subagent; independently re-ran all three myself and read the full diff by
hand (`src/commands/setup.rs` only, as instructed) before trusting it.

While reviewing, tested a scenario decisions/0023 never actually considered
when drafted: running `setup` a second time against its own prior graft
output. It should have hard-failed (decisions/0021's own stated consequence:
"a repo that already had `gitprism setup` run against it once still
hard-fails... re-running setup remains unsupported") but instead silently
succeeded — a prior graft's parent already *is* dest's old tip, so the new
merge-base generalization found a real merge-base and happily merged dest's
newer tip in, producing a second commit with no error at all. Confirmed this
directly with a throwaway probe test before raising it (also recovered
`setup.rs` from a self-inflicted `git checkout --` mishap mid-review — no
work was actually lost, since the full file had just been read).

Asked the project owner how to handle it: hard-fail on setup's own graft, or
accept idempotent re-merging. Chose the hard-fail, matching decisions/0021's
original guarantee. Added a TDD-first test
(`run_fails_loudly_when_a_pre_existing_branch_is_setups_own_prior_graft`) and
the fix: `setup` now recognizes a pre-existing branch's tip already carrying
a `Gitprism-Dest-Commit` trailer (decisions/0003 — the same bookkeeping every
other command already trusts) and hard-fails before the merge-base question
even arises, since a prior graft's parent always has one. `sync.rs`'s private
`trailer_value` helper became `pub(crate)` so `setup.rs` could reuse it
rather than duplicating trailer parsing. Updated decisions/0023's Decision,
Why, and Consequences sections to document this as case 2 of the reconciled
rule, not just a code-level patch.

Final state: 114 tests passing (up from 113 pre-0023), `cargo clippy
--all-targets` clean, `cargo fmt --check` clean. Not yet committed — pending
the project owner's review of the working tree.

## 2026-08-18 (again)

**Fixed a bug**, not a new decision: right after `gitprism setup`, `git status`
showed `.gitprism.toml` and `.gitprismignore` staged for deletion whenever the
first configured branch was a pre-existing branch (decisions/0021's clean-clone
case, or decisions/0023's merge-reconciliation case) rather than one setup
created fresh. Root cause: `graft_branch`/`merge_branch` force-move the branch's
ref straight to the new commit via a plumbing `repo.commit`, bypassing the
index entirely; by the time `checkout_branch` ran, HEAD already resolved to
that same commit, so libgit2's checkout — which defaults its conflict/dirty
baseline to HEAD's current tree — saw baseline and target as literally the same
tree object and concluded nothing needed writing. Any path the graft/merge
added that wasn't already on disk (the control files always; for a real
decisions/0023 reconciliation, potentially any dest-only file too) landed in
HEAD's tree but never in the index or working tree.

Wrote a failing test first
(`run_leaves_the_index_in_sync_with_head_for_a_pre_existing_first_branch`),
confirmed it reproduced only with a *real* prior checkout in place (a bare ref
move isn't enough — `run_succeeds_against_a_pre_existing_branch_matching_dests_tip`
never caught this because it never checks out anything first). Fixed
`checkout_branch` to detach HEAD to the branch's actual pre-run tip (or an
unborn scratch ref, reusing `rollback_branches`'s existing trick, for a branch
setup just created) before calling `checkout_tree`, so checkout's baseline
reflects what was really on disk instead of degenerately matching the target;
HEAD lands back on the branch afterward. Ruled out `.force()` first — git2's own
bundled examples hit this same "force is required to make the working
directory actually get updated" quirk after moving a ref directly — but it
silently overwrote the untracked-file-collision case
(`run_fails_loudly_instead_of_overwriting_a_conflicting_untracked_file`), which
must stay a hard error.

Added a second test
(`run_checks_out_a_dest_only_file_from_a_real_reconciliation_not_just_the_control_files`)
proving the fix isn't control-file-specific: a genuinely new dest-only file
introduced by a decisions/0023 merge reconciliation is now correctly
materialized too. Corrected decisions/0021's Consequences section, which had
asserted (incorrectly) that this case was already handled.

Final state: 116 tests passing (up from 114), `cargo clippy --all-targets`
clean, `cargo fmt --check` clean.

## 2026-08-18 (a third time)

Real-world bug report against a build of `52fe184` (decisions/0023): running
`gitprism sync` on a repo `gitprism setup` had just grafted failed with "no
Gitprism-Dest-Commit trailer found anywhere in source's history — has
`gitprism setup` been run for this pair?" while syncing a branch named
`ai-setup` — coincidentally the exact stray-branch name `setup.rs`'s own
doc-comment uses as an example of a branch `config.branches` never mentions.

Traced to decisions/0017's own flagged-but-unverified assumption: source→dest
discovers and mirrors every local branch on source, and for one with no dest
ref yet, reads its boundary off the nearest `Gitprism-Dest-Commit` trailer in
its own first-parent history (`newest_dest_marker`) rather than fetching
anything to check against. That trailer only exists if the branch descends
from something `gitprism setup` grafted or merged. `ai-setup` predated
`setup` and was never named in `config.branches`, so it never got one —
genuinely unrelated history, not a broken setup. The bail aborted `sync`
entirely, including every other branch's own dest→source/source→dest work.

Drafted decisions/0024: initially proposed skipping the branch with a warning
(decisions/0018-style), but reconsidered — decisions/0023 already treats the
identical shape of problem ("no merge-base with dest at all") as a permanent
hard-fail during `setup`, not a silent no-op, and `sync` discovering the same
thing later shouldn't reach a different answer. Revised to a hard stop
(decisions/0007) reported through the `Reporter` machinery decisions/0020
built for every other outcome, mirroring the existing real-conflict code
path: `reporter.complete(Outcome::Error, ...)`, `reporter.finish()`, then
`anyhow::bail!`.

Implemented and then re-reconsidered against real output. Two problems
surfaced: `run()` wraps every `sync_pair_to_dest` call in
`.with_context(|| format!("syncing {branch:?} source -> dest"))?`, so the
hard-fail's `anyhow::bail!` propagated straight through it — `main`'s default
`Result` printing then showed the identical detail text twice (once as the
`Reporter`'s own line, once as a bare `Error: syncing "ai-setup" source ->
dest` / `Caused by:` chain whose wrapped continuation lost its indent in a
real terminal), and the hard stop aborted every other branch too, including
every properly round-tripped one, over a branch nobody had ever named in
`config.branches` in the first place.

Revised decisions/0024 a second time: matches decisions/0018's own Case 2
precedent instead of decisions/0023's — a mirror-only branch surprise gets a
skip-with-note (yellow, reusing the existing `Outcome::Skipped` rather than
adding a fourth variant), not a hard stop; decisions/0023's precedent is
about a branch `config.branches` itself names, not one decisions/0017 merely
discovered. `sync_pair_to_dest`'s `!dest_ref_exists` branch now calls
`reporter.complete(Outcome::Skipped, ...)` then `return Ok(());` on finding
no marker — no `Err`, so nothing for `run()`'s `.with_context` or `main`'s
printer to duplicate, and the run continues past it. `config.branches`' own
two call sites into the same marker scan (`dest_tip_is_accounted_for`,
`pending_dest_commits`) keep their hard-fail, unaffected. Renamed the
decision file from "...hard-fails-a-mirror-only-branch-with-no-dest-ancestry"
to "...warns-and-continues-past-a-mirror-only-branch-with-no-shared-history"
to match. Code fix delegated to a sub-agent, reviewed directly afterward —
not yet marked stable, pending the user's final confirmation.

Rebuilt and ran the real binary: the reused `Outcome::Skipped` printed the
literal word "skipped", indistinguishable on a fast scan from decisions/0018's
genuinely benign "already merged, cleaned up" skip sitting in the same run.
Those aren't the same thing — one is a one-time no-op, the other recurs every
run until an operator acts on it. Revised decisions/0024 a third time: added
a new `Outcome::Warning` (yellow, same color as `Skipped` — decisions/0020
has no fourth color to spend — but its own `"warning"` label) instead of
reusing `Skipped`, which decisions/0024's second draft had explicitly
deferred until "a real case shows `Skipped`'s existing meaning is actually
inadequate" — this was that case. `sync_pair_to_dest`'s no-marker branch now
reports `Outcome::Warning` instead of `Outcome::Skipped`; every existing
`Skipped` call site is unchanged. Code change delegated to a sub-agent again,
reviewed directly afterward — still not marked stable, pending the user's
final confirmation.

Revised decisions/0024 a fourth time: `Outcome::Warning` renders magenta
instead of reusing `Skipped`'s yellow — the user asked for the two to be
visually distinct, not just distinct in label. Extends decisions/0020's
green/yellow/red/cyan palette with a fourth color rather than spending yellow
on two different meanings. `progress.rs`'s `color()` match arm and its unit
test updated accordingly; still not marked stable.

## 2026-08-20 — subprocess and configuration boundary hardening

Implemented the review's Git/process hardening: configured branches are
validated for Git branch syntax and uniqueness before any subprocess or
mutation; fetch accepts only validated branch names and constructs a fully
qualified `refs/heads/<branch>` source ref; remote operands are rejected when
they begin with `-` and Git's `--` terminator is used. Pushes use
`--porcelain`, parse stable ref-status records for non-fast-forward retries,
and disable interactive prompting. Git diagnostics are bounded, credential
redacted, and control-character escaped. Updated decisions 0002, 0009, and
0013 with implementation-hardening addenda.

**Update**: Decided [0025](decisions/0025-authenticated-mapping-markers.md) —
mapping trailers remain readable but are untrusted unless accompanied by a
canonical HMAC-SHA256 state block keyed by `GITPRISM_STATE_KEY`. The key is
validated before network or mutation work and removed from every Git
subprocess environment; reserved marker-looking message lines are stripped
before gitprism generates its authenticated block.

**Update**: Decided [0026](decisions/0026-protected-versioned-policy.md) —
versioned `.gitprism.toml` and `.gitprismignore` remain reviewable, but
`setup`, `sync`, and `resolve` require the externally protected
`GITPRISM_POLICY_SHA256` digest over their exact raw bytes before parsing or
mutation. `sync` uses one verified exclude list for the entire run, and the
read-only `gitprism policy-hash` command prints the deployment pin.

**Update**: Decided [0027](decisions/0027-source-to-dest-resolution-state.md) —
source-to-dest conflicts use an authenticated Git operation ref, a filtered
synthetic patch, and a unique linked worktree. The source checkout remains
untouched; continuation validates source, policy, patch, and worktree state,
rejects excluded-path edits, and applies the allowed resolution delta back onto
the original destination tree before an ff-only push.

**Update**: Decided [0028](decisions/0028-operation-lock-and-local-advance-cas.md) —
mutating commands take a non-blocking lock in Git's common directory, and
dest-to-source local advancement is preflighted against a safe checkout and
finished with a compare-and-swap ref update. A concurrent external Git move is
never overwritten after a successful remote push.

## 2026-08-20 — operator resolution identity and Git process boundary

**Update**: Decided [0029](decisions/0029-operator-resolution-identity-and-process-boundary.md)
— conflicts remain fail-fast and operator-resolved: gitprism reproduces the
conflict, the operator edits and stages it, and explicit continuation records
the chosen result. Dest-to-source temporary cherry-pick commits now receive
the configured committer identity while preserving the picked author. Git
children scrub the state key and resolved URL fallbacks but retain normal
authentication variables such as `GIT_ASKPASS`. Git itself, its configuration,
credential helpers, and repository hooks remain a documented trust boundary;
gitprism does not claim to sandbox them. Operator command snippets use literal
`<branch>` placeholders so hostile branch names cannot become shell fragments.

## 2026-08-20 — setup rollback error reporting

Implemented the rollback safety already required by decisions 0021 and 0023:
setup now preserves its primary failure while reporting every branch, HEAD, and
control-file recovery failure; expected missing fresh branches remain harmless,
while unexpected lookup failures are surfaced. Control-file restoration
snapshots the actual source-root files, including absence, bytes, and Unix
permissions, and external `--config` input cannot replace a different
source-root config. Control-file removal failures now enter the same rollback
path. Fetch side effects remain outside rollback by design. Added deterministic
aggregation and removal-failure tests; no new architecture decision was needed.

## 2026-08-20 — byte-safe Git data

**Update**: Decided [0030](decisions/0030-byte-safe-git-data.md) — Git paths
remain raw bytes for comparisons and diagnostics where possible, malformed
bytes are escaped deterministically for terminal output, and unsupported
non-UTF-8 commit messages, tree names, and refs fail explicitly before any
commit or ref mutation. Added coverage for malformed merge-tree/index paths,
Unicode commit messages, raw remote redaction, and component-aware checkout
collision checks.

## 2026-08-20 — centralized Git process runner

**Update**: Decided [0031](decisions/0031-centralized-git-process-runner.md)
— all production Git children now use one standard-library runner with null
stdin, `GIT_TERMINAL_PROMPT=0`, concurrent bounded stdout/stderr capture, a
300-second default deadline, and a strictly bounded operator override. Timeout
or output overflow kills and reaps the direct child; incomplete output is
never parsed as if it were complete.

## 2026-08-20 — bounded repository-controlled data

**Update**: Decided [0032](decisions/0032-bounded-repository-controlled-data.md)
— control files and small Git state files are read through bounded regular-file
helpers; commit messages, branch collections, tree traversal, conflict reports,
pending history, and marker scans have static fail-closed limits. Decision 0019's
“unbounded marker scan” wording is superseded: scans still do not hide the graft
point, but stop with an error when the safety budget is exceeded.

## 2026-08-20 — authenticated resolution worktree and safe file recovery

**Update**: Decided [0033](decisions/0033-authenticated-resolution-worktree-and-safe-file-recovery.md)
— source-to-dest continuation now authenticates the exact generated worktree
locator, verifies its registration and common Git directory before opening it,
and fails closed for tampered or legacy state. Bounded reads validate the
opened handle, while setup rollback uses create-new replacement handles that
cannot follow raced symlinks or hard links. Conflict handling remains
fail-fast and operator-driven.

## 2026-08-20 — release distribution policy

**Update**: Added [playbooks/0002](playbooks/0002-release-distribution.md) to
record the approved release policy: exact `v<package-version>` tags, Linux
x86_64/macOS arm64/macOS x86_64/Windows x86_64 archives, `SHA256SUMS` for
every archive, intentionally unsigned artifacts, and no crates.io publication.

**Update**: The tagged release workflow now verifies `v<package-version>`
tags, builds/tests/smoke-checks the four approved targets, uploads the archives,
generates `SHA256SUMS`, and draft-gates GitHub publication. Artifacts remain
intentionally unsigned.

## 2026-08-20 — control files stay byte-exact through checkout

**Update**: Decided [0034](decisions/0034-control-files-stay-byte-exact-through-checkout.md)
— Windows CI surfaced that libgit2's checkout applies ordinary `core.autocrlf`/
`.gitattributes` text filtering to `.gitprism.toml`/`.gitprismignore` like any
other tracked file, silently invalidating decision 0026's pinned digest on a
machine where `core.autocrlf=true` even though nothing in the repository
changed. `policy::restore_control_files_exact` now re-writes both straight
from their blob bytes after every checkout that might place them; every other
file keeps its ordinary checkout attributes, and disabling filters for the
whole tree was rejected as overreach.

**Update**: Windows CI also exposed a `git worktree add`/`remove` failure
underneath [0033](decisions/0033-authenticated-resolution-worktree-and-safe-file-recovery.md):
`fs::canonicalize`'s Windows result is an extended-length (`\\?\`-prefixed)
path, which Git for Windows' MSYS-based git does not reliably accept as a
`worktree add`/`remove` argument. The authenticated worktree path itself
still keeps `fs::canonicalize`'s exact output everywhere — 0033's
symlink-substitution check depends on that canonical form matching itself —
`git::worktree_add`/`worktree_remove` now strip the verbatim prefix only for
the literal subprocess argument they pass to `git`.

**Update**: Code review on the PR caught two more bugs in
`policy::restore_control_files_exact` itself. Re-staging via
`Index::add_path` hashes through the working-tree filter's clean side, so a
control file whose own committed blob already contains CRLF got
re-normalized to LF before hashing — the index diverged from HEAD despite
nothing having changed; fixed by pointing the index entry straight at the
tree's own oid/mode instead of hashing anything. The raw restore write also
followed whatever already occupied the path, including a symlink or
hardlink planted there between checkout and restore — inconsistent with
0033's no-follow stance; fixed by reusing `setup`'s own remove-then-
`create_new` recovery pattern, now shared as
`policy::write_regular_file_no_follow`. See 0034's Consequences section for
detail.

**Update**: A second review round on the same PR caught two more bugs, both
about file mode rather than content, both on Unix only. (1)
`write_regular_file_no_follow` chmod'd the new file by path after closing
it — the same race its own no-follow write exists to close, since whatever
occupies the path by the time the chmod runs gets its permissions changed,
not necessarily the file just created. Fixed by applying the mode to the
still-open `File` handle before dropping it. (2)
`restore_control_files_exact`'s raw write carried over content but not
mode, so an executable control file (`100755`) silently became `100644` on
disk while the index still pointed at the executable blob — dirty under
`core.filemode=true`. Fixed by passing the tree entry's mode into the same
helper. A new Unix test commits a control file as
`FileMode::BlobExecutable`, calls the restore directly, and asserts both the
on-disk permission bits and `status_file`'s cleanliness. See 0034's
Consequences section for detail. `cargo test`: 189 passed, 0 failed.
`cargo clippy --all-targets`: clean. `cargo fmt --check`: clean.

**Update**: A third review round caught a bug in fix (2) above: applying a
git-tracked mode to the handle verbatim (the same way a *captured* mode is
restored exactly) bypasses the umask, so a restrictive umask (e.g. `077`)
that an ordinary checkout would have honored gets silently widened back to
the git mode's raw bits (`0644`/`0755`) instead of the umask-constrained
result (`0600`/`0700`) a real checkout would produce — git tracks only the
executable bit, never group/world permissions, so those bits were never
git's to dictate in the first place. Fixed by splitting
`write_regular_file_no_follow`'s single `mode: Option<u32>` into a
`RestoreMode` enum: `SubjectToUmask` passes the mode as the `open()`
creation mode (umask-constrained, what `restore_control_files_exact`
wants — mirroring a real checkout), `Exact` still applies to the open
handle after creation (umask-bypassing, what `setup`'s own recovery
wants — reproducing a previously captured mode byte-for-byte). Added
`libc` as a dev-dependency (std has no umask API) for a new Unix test that
sets `umask 077` under a mutex — following `config::ENV_VAR_LOCK`'s
precedent, since the umask is one process-global a test can't mutate
unguarded — and confirms a `100755`/`100644` control file restores to
`0700`/`0600`. See 0034's Consequences section for detail. `cargo test`:
190 passed, 0 failed. `cargo clippy --all-targets`: clean. `cargo fmt
--check`: clean.

**Update**: A fourth review round caught a real gap in that umask test
itself: its mutex only synchronizes against other tests that also take it,
and no other test in the suite has any reason to expect the process umask
to change, so none of them do — meaning every other test that creates a
file (including this file's own executable-bit test) could observe
whatever umask this test happened to have set while running concurrently,
and a panic between setting it and restoring it would have left the wrong
umask in place for the rest of the run, with no `Drop` to catch that.
Fixed by re-executing this one test alone, filtered by its own
libtest-assigned thread name (`std::thread::current().name()`), in a
freshly spawned subprocess (`std::env::current_exe()` plus `--exact
--nocapture`) — the umask change and any panic are now contained to a
process nothing else in the suite ever runs in; the parent test only
checks the subprocess's exit status, surfacing its captured stdout/stderr
on failure. The mutex is gone; no test anywhere else needed to change to
stay correct. See 0034's Consequences section for detail. `cargo test`:
190 passed, 0 failed. `cargo clippy --all-targets`: clean. `cargo fmt
--check`: clean.

## 2026-08-21 — housekeeping

**Update**: Struck the open question on `trailer_value` matching a quoted trailer
anywhere in a message. It was stale:
[decisions/0025](decisions/0025-authenticated-mapping-markers.md) already replaced
that loose scan with a single canonical final state block that resume and
loop-prevention only trust once its HMAC verifies, closing the exposure described.
No code changed.

**Update**: An external security review found the README's "Mapping state key"
section overstated a secrecy guarantee: it said `GITPRISM_SOURCE_URL` /
`GITPRISM_DEST_URL` fallback values are "scrubbed" from Git subprocesses
alongside `GITPRISM_STATE_KEY`, implying the resolved URL is unavailable to
them. It isn't — `fetch`, `remote_ref_exists`, and `push` in `src/git.rs` all
pass the resolved URL to Git as a command-line argument, visible to any
same-user process (`ps`, `/proc/<pid>/cmdline`) and capturable in CI logs or
crash reports, regardless of the environment scrubbing. Corrected the README
to state that plainly, kept the true claim (the gitprism-specific env vars
aren't inherited, so a hook can't read them out of the environment), and added
guidance against embedding credentials in repository URLs at all. Also dropped
the credential-bearing rationale from the `[source].url` / `[dest].url` bullet,
which recommended the env-var fallback for exactly the URLs this now warns
against; the per-environment rationale stays.
[decisions/0013](decisions/0013-repo-urls-optional-fall-back-to-env-vars.md)
states the same credential-bearing rationale in its Context section and was
left unchanged — flagged for separate handling. No code changed.

**Update**: Added a 2026-08-21 addendum to
[decisions/0013](decisions/0013-repo-urls-optional-fall-back-to-env-vars.md)
withdrawing its credential-secrecy rationale, the item flagged above:
`fetch`/`remote_ref_exists`/`push` in `src/git.rs` pass the resolved URL to
Git as a command-line argument, so env-scrubbing does not make it secret. The
decision itself — `[source].url`/`[dest].url` optional with env-var fallback —
is unchanged; only the withdrawn rationale is superseded. Updated 0013's
`design/decisions/index.md` entry to match. No code changed.

## 2026-08-21 — pending history is first-parent

**Update**: Added [decisions/0035](decisions/0035-pending-history-is-first-parent.md).
An external review found, and this decision confirms by reproduction, that
`pending_commits` walks the full DAG while `build_pending_dest_tip` and
`build_pending_source_tip` derive their three-way-merge base from
`parent(0)` — so a merge's own side-branch commits get replayed against a
base tree the dest/source chain was never at, and a conflict a human already
resolved inside the merge commit gets hard-stopped again on the side
branch's own diff. `pending_commits` gains `Revwalk::simplify_first_parent()`,
the same idiom decisions/0019 applied to the marker scans, closing the gap
0019's own Consequences explicitly left open ("no change to
`pending_commits` ... or `build_pending_dest_tip`/`build_pending_source_tip`'s
own walks"). Also corrects this file's earlier claim, in the entry
documenting decisions/0016, that the interleaved-branch churn had
"disappear[ed]" — it hadn't; the test comment in
`run_does_not_duplicate_a_no_ff_merges_content_on_dest`
(`src/commands/sync.rs`, ~lines 3281-3288) still records it as an open
design question, and this decision is what actually removes it. No code
changed in this commit; the implementation and its test coverage are a
follow-up.

**Update**: Implemented. `pending_commits` (`src/commands/sync.rs`) gained
`revwalk.simplify_first_parent()` between `hide()` and `set_sorting()` — the
one-line change decisions/0035 specified, following the same
`.context(...)` idiom as the two `simplify_first_parent()` calls decisions/0019
already added. No other production code changed.

Added the test coverage decisions/0035's Consequences enumerated:
`run_honors_a_conflict_resolved_by_hand_inside_a_merge_commit` (the
regression — confirmed failing before the fix with a genuine `shared.txt`
merge-tree conflict hard-stop, exactly as 0035 predicted, then passing
after); `run_applies_a_clean_two_parent_merge_without_replaying_the_side_branchs_own_commit`
(asserts dest's own commit count, since the two pre-existing merge tests
assert only final content and can't tell the two walks apart);
`pending_commits_still_hides_a_boundary_reachable_only_via_a_merges_second_parent`
(a direct unit test on `pending_commits` confirming `hide()`'s own ancestor
exclusion still reaches a boundary's non-first parent);
`run_applies_a_squash_merged_source_commit_as_a_single_dest_commit` and
`run_applies_a_rebased_linear_source_history_commit_by_commit` (regression
cases, named and commented as such — both single-parent, unaffected);
`run_carries_a_real_two_parent_merge_on_dest_into_source_as_one_net_change`
(dest→source's first real-merge coverage, since `pending_commits` is
shared); and `run_correctly_merges_a_new_source_merge_onto_a_dest_tip_shaped_by_the_old_full_dag_walk`
(the migration case — dest hand-built to match the old full-DAG walk's
three-commit output, then a new merge processed under the new walk against
it). All seven passed without needing to weaken or alter any pre-existing
test.

Also corrected the test comment this entry's own claim was about: it
actually lives in `run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`
(not `run_does_not_duplicate_a_no_ff_merges_content_on_dest`, as both this
entry and decisions/0035 itself misattributed it) — rewritten to record that
decisions/0035 removed the interleaved-branch churn, instead of describing
it as an open design question.

`cargo test`: 197 passed, 0 failed (up from 190). `cargo clippy --all-targets
-- -D warnings`: clean. `cargo fmt --check`: clean.

## 2026-08-21 — branch-additive exclusions

**Update**: Added [decisions/0036](decisions/0036-branch-additive-exclusions.md).
An external security review found a real disclosure path: decisions/0026
loads one verified `ExcludeList` from the working tree per `sync` run and
never loads a branch-tip version, confirmed in `src/commands/sync.rs::run`
(~lines 101-103). A feature branch that adds sensitive content and
correctly adds its own `.gitprismignore` entry excluding it has that
instruction discarded — the content is mirrored to dest anyway — and per
this file's own still-open "a file that already reached dest and is later
added to `.gitprismignore` stays on dest forever" item, that disclosure is
irreversible.

Decided: effective source→dest exclusion becomes `trusted.is_excluded(path)
|| branch.is_excluded(path)`, two independent `ExcludeList` matchers rather
than one concatenated file — concatenation would let a branch's own `!`
negation un-exclude trusted content, while independent matchers keep the
branch list monotone (add-only). `branch` is the union of every
`.gitprismignore` found across the commits being replayed for that branch
this run, not the branch tip alone, so an exclusion added in an earlier
commit can't be undone by a later one deleting the line — replayed content
already reached dest by then. `branch` is deliberately outside
`GITPRISM_POLICY_SHA256`: the digest is one static value with no per-branch
dimension, and extending it to branch tips would just reproduce today's bug
rather than fix it.

Also records the amendment's own limits: a branch whose entire remaining
content becomes covered by its own new exclusions filters to a no-op against
a landing branch, so decisions/0018's `already_merged_into_a_landing_branch`
classifies it as already-merged-and-cleaned-up and it is never created on
dest — the leak moves rather than disappears, and 0018 already accepted this
shape of tradeoff for a different cause. A branch that never adds an
exclusion for sensitive content it introduces is still exported unchanged;
gitprism cannot infer sensitivity. Checked `design/references/` for
per-branch or additive-filtering precedent: none found among josh, Copybara,
git-subtree, git-filter-repo, or jujutsu.

No code changed in this commit. Implementation (an `ExcludeList` holding
more than one matcher, reading `.gitprismignore` per replayed commit, and
the enumerated test list) is a follow-up.

## 2026-08-21 — branch policy mismatch fails closed

**Update**: Added [decisions/0037](decisions/0037-branch-policy-mismatch-fails-closed.md),
superseding [decisions/0036](decisions/0036-branch-additive-exclusions.md).
`AGENTS.md` (commit `e213f72`) added a working-style rule preferring operator
intervention over novel automation, defaulting to "stop and involve the
operator" when Git has no safe primitive for the recovery and resolution
would require guessing intent. 0036's own Prior art section already found no
precedent for its proposed per-branch additive-exclusion union among josh,
Copybara, git-subtree, git-filter-repo, or jujutsu, which makes it exactly
the novel automation the new rule defaults against. The project owner chose
fail-closed over the union.

Decided: a source→dest branch replaying a commit whose `.gitprismignore` or
`.gitprism.toml` is present and byte-differs from the digest-pinned,
already-verified working-tree policy (decisions/0026) halts that branch —
`Outcome::Error`, naming the branch, commit, offending file, and remedy
(update `GITPRISM_POLICY_SHA256` or reconcile the branch) — while other
branches keep syncing and the overall `sync` exit status is non-zero. Absence
of a control file is not a mismatch, since the trusted policy already applies
regardless of a commit's own content; only present-and-different is
ambiguous. No second policy source is introduced: decisions/0026's one
verified `ExcludeList` for every branch stands unchanged, and
`already_merged_into_a_landing_branch` (`src/commands/sync.rs`, ~line 641) is
confirmed untouched, so 0036's leak-moves-to-silent-non-mirroring side effect
does not recur — a mismatching branch now halts loudly instead of vanishing.
Also notes the sanctioned `GITPRISM_POLICY_SHA256`-change workflow (a
control-file-only branch) now halts with an explicit message rather than
risking silent already-merged classification, an improvement, and states
plainly that every legitimate branch-level control-file change now needs
operator action first — the cost the new rule accepts.

No code changed in this commit. Implementation (threading
`VerifiedPolicy.config_raw`/`ignore_raw` into `sync_pair_to_dest_with_key`,
the per-commit comparison, and the enumerated test list) is a follow-up.

**Update**: Implemented decisions/0037. `run()` now binds
`verified_policy.ignore_raw`/`config_raw` (previously unconsumed) alongside
`config`/`exclude_list`, and threads both into `sync_pair_to_dest_with_key`.
The check is a pre-pass: for each branch, after `dest_tip`/`boundary` are
resolved (either from a real dest fetch or, for a not-yet-mirrored branch,
`newest_dest_marker_opt_for_branch`) but *before* `decisions/0018`'s
"already merged into a landing branch" classification and before
`build_pending_dest_tip` builds or pushes anything, `pending_commits(boundary,
source_tip)` is walked (skipping the same `Setup`/`DestToSource`
loop-prevented commits `build_pending_dest_tip` itself never replays) and
each commit's root-tree `.gitprismignore`/`.gitprism.toml` entry is read
(bounded by `limits::MAX_CONTROL_FILE_BYTES`, `src/commands/sync.rs`'s new
`read_control_file_blob`) and compared byte-for-byte against the pinned raw
bytes. Absence is skipped (not a mismatch); a present-and-different file
returns a `PolicyMismatch { commit, filename }` and the branch reports
`Outcome::Error` (via `policy_mismatch_message`, naming the branch, commit,
and offending filename, and telling the operator to re-run
`gitprism policy-hash` or reconcile the branch) with **nothing pushed** —
`sync_pair_to_dest_with_key` returns `Ok(true)` instead of erroring, so
`run()`'s branch loop moves on to the next branch rather than aborting
(decisions/0024's precedent). Moving the mismatch check ahead of the
"already merged" classification is what makes the sanctioned
`GITPRISM_POLICY_SHA256`-change workflow (a branch whose only diff is an
unapproved control file) halt loudly instead of being silently read as
already-merged-and-cleaned-up. `run()` accumulates an
`any_branch_halted_for_policy_mismatch` flag across the branch loop, calls
`reporter.finish()` once every branch is processed (same as the clean-exit
path), and only then `anyhow::bail!`s if any branch halted — so the pinned
bar always ends cleanly and the run's exit status is non-zero without
cutting any other branch's sync short.

Chose not to add a new `Outcome` variant: reused `Outcome::Error` (already
distinct from `Outcome::Warning` since decisions/0024) plus the mandatory
non-zero `run()` exit, which decisions/0037 itself accepts as sufficient —
a mismatch already can't be mistaken for `Outcome::Warning`'s benign,
run-still-succeeds shape, and a new variant would only duplicate
`Outcome::Error`'s existing color/label without changing behavior.

Six tests added to `src/commands/sync.rs`: a differing `.gitprismignore`
halts the branch with nothing pushed; same for `.gitprism.toml`; a commit
with no control file at all replays normally; a commit whose control file
matches the pin byte-for-byte syncs normally; one branch halting still lets
another branch sync while `run()` itself returns an error (asserting both
halves); and a control-file-only branch (the sanctioned policy-change
workflow) halts with the mismatch message rather than being classified
already-merged-and-cleaned-up. Pre-implementation, tests 1/2/3/4/6 (written
against the wrapper's new `Result<bool>` signature) failed to compile against
the old `Result<()>` signature (`cannot apply unary operator '!' to type
'()'`); isolating test 5 alone against the unmodified code showed the real
behavioral gap directly: `run()` returned `Ok(())` and printed `main: skipped
(source -> dest) — up to date, nothing to sync` — the differing
`.gitprismignore` was silently absorbed since it filters to no visible tree
change, exactly the disclosure risk decisions/0037 closes.

Verification: `cargo test` — 203 passed (197 baseline + 6 new), 0 failed;
`cargo clippy --all-targets -- -D warnings` — clean; `cargo fmt --check` —
clean.

## 2026-08-21 — branch authority determines history rewriting

**Update**: Added [decisions/0038](decisions/0038-branch-authority-determines-whether-history-may-be-rewritten.md),
formalizing `requirements/0001` step 3 as amended separately in `cdac783`:
force semantics depend on which branch authority owns the history, not on
push direction. Round-tripped branches (`config.branches`) stay
fast-forward-only both ways, with persistent divergence handed to the
operator without a prescribed reconciliation method. Mirror-only branches
may be force-updated on source→dest to mirror a deliberately rewritten
source branch, but only after decisions/0009's bounded refetch-and-recompute
is exhausted — decisions/0009 itself is unchanged, this only defines what
happens after its retries run out. Force is made opt-in and visible via an
explicit two-variant `PushMode` every push call site must declare, rather
than a separate force helper. Classified the four existing `git::push` call
sites by branch authority: `src/commands/sync.rs:509` (source→dest, role
depends on current `config.branches` membership), `src/commands/sync.rs:1404`
(dest→source, always round-tripped), `src/commands/resolve.rs:335` and
`:579` (dest pushes in source→dest resolve — must be classified by
`config.branches` membership too, since `Direction::SourceToDest` accepts
any local branch, not just round-tripped ones), and `src/commands/resolve.rs:1324`
(source push in dest→source resolve, always round-tripped). Recorded a
standing operator hazard: branch authority is decided by current
`config.branches` membership, so editing that list silently changes which
branches gitprism may rewrite. Rejected `--force-with-lease` for any branch
type, verified directly against real git: a lease with a correct expected
value still force-pushed a divergent history and the remote reported
`(forced update)`; separately, plain push already rejects every race that
could lose a concurrent writer's work, and the one race a lease adds
detection for (remote moved to a commit gitprism already had) is harmless.
Reconciled with `requirements/0001`'s unchanged git-filter-repo objection:
that objection is to force-push as the permanent steady-state sync
mechanism, which this decision does not create — the steady state stays
fast-forward, and force fires only as an exceptional response to a
deliberate source-side rewrite. Prior art: GitLab push mirroring (already
cited in decisions/0018 as the closest analogue) force-updates a diverged
mirror by default, confirmed directly from GitLab's docs; no other
already-reviewed tool (josh, Copybara, git-subtree, jujutsu, git-filter-repo)
was found to force-update a mirror to match an authoritative upstream. No
code changed in this commit; `requirements/0001` was amended separately in
`cdac783`.

## 2026-08-21 — mirror-only source rewrites rebuild the projection

**Update**: Added [decisions/0039](decisions/0039-mirror-only-source-rewrites-rebuild-the-projection.md),
extending decisions/0038. Starting 0038's implementation surfaced that its
force path is unreachable as written: `dest_resume_point_for_branch`
(`src/commands/sync.rs`) refuses a rewritten mirror-only branch — rebase,
amend, or reset — before any push is attempted, because
`dest_tip_is_accounted_for`'s Case 1 still recognizes the old, pre-rewrite
dest tip via gitprism's own stale marker, `newest_source_marker` returns the
pre-rewrite source tip as the boundary, and `source_tip` no longer descends
from it. This investigation is what stopped 0038's implementation and
produced this decision instead of a workaround.

Decided: a rewritten mirror-only branch is a positively identified state —
mirror-only (absent from `config.branches`), a dest ref exists, a prior
gitprism marker is found, and source no longer descends from the previous
boundary — checked directly, not reached by exhausting decisions/0009's
retries. Explicitly rejected framing this as "force after failed retries":
that reads as a general escalation policy a future contributor could extend
to a round-tripped branch's persistent divergence, which is exactly the
operator boundary 0038 draws and `AGENTS.md`'s operator-intervention rule
protects. Decisions/0009's refetch-and-recompute keeps its existing,
narrower meaning — handling a genuine concurrent race — unchanged in both
push modes. On a detected rewrite, the projection rebuilds from the same
`(boundary, dest_tip)` `newest_dest_marker_opt_for_branch` already reads off
source's own graft-derived first-parent history for the `!dest_ref_exists`
case (decisions/0006) — no second rebuild mechanism — then force-pushes via
0038's `ForceMirrorOnly`. Named the authority invariant explicitly:
independent dest advancement on a mirror-only branch may be discarded only
because the branch is currently absent from `config.branches`
(decisions/0017 guarantees dest→source never touches such a branch, so
nothing on its dest ref is dest's own independent contribution). Narrows
0038's `resolve.rs` guidance: `resolve` never runs this fresh resume-point
detection, so every `resolve.rs` push stays fast-forward-only regardless of
branch authority, refining rather than contradicting 0038's own table.
Restates, without re-deriving, 0038's config-role hazard (branch authority
is decided by current `config.branches` membership, re-evaluated every run).

Checked `design/references/` for prior art on rebuilding a projection from a
shared base after a detected rewrite, as distinct from simply force-updating
a ref: found nothing — GitLab's push-mirror docs (already cited in
decisions/0018/0038) describe the force-update outcome but not a rebuild
step, since a plain mirror push has no filtering stage to rebuild in the
first place.

No code changed in this commit — decisions/0038's `PushMode` enum and this
decision's rewrite-detection/rebuild path are still to be implemented
together.

**Update**: Implemented decisions/0038's `PushMode` together with 0039's
rewrite detection and rebuild.

`git::push` (`src/git.rs`) now takes a required `PushMode` (`FastForwardOnly`
| `ForceMirrorOnly`) at every call site; the refspec construction itself is
pulled into a small `push_refspec` helper so the two modes' exact wire shape
(plain vs. `+`-prefixed) is unit-tested directly. Per-call-site
classification: `sync.rs`'s source→dest push (`sync_pair_to_dest_with_key`)
is the only site that can select `ForceMirrorOnly`, and only after
establishing both that `branch` is absent from `config.branches` and that
`mirror_only_rewrite_detected` positively identified a rewrite; `sync.rs`'s
dest→source push, the round-tripped `resolve.rs` source-to-dest paths
(`start_source_to_dest`, `finish_source_to_dest`), and `resolve.rs`'s
dest-to-source `finish` all stay `FastForwardOnly` unconditionally, matching
0039's narrowing of 0038's `resolve.rs` guidance. The two test-only call
sites (`resolve.rs`'s and `sync.rs`'s `bare_source_remote_seeded_at`
fixtures) were mechanically updated to `FastForwardOnly`.

Rewrite detection is `mirror_only_rewrite_detected` (`src/commands/sync.rs`),
called only from the arm where `dest_resume_point_for_branch` has already
returned `Ok(None)` for a branch absent from `config.branches` — the four
conditions are checked directly there, not by counting retries:
`dest_tip_accounted_for` (a new three-way split of the old boolean
`dest_tip_is_accounted_for`, distinguishing "a prior gitprism sync landed
here" from "dest is still sitting at the graft") must say
`ViaPriorGitprismSync`; `newest_source_marker` must find a boundary; that
boundary must exist in this clone's odb; and `source_tip` must fail
`graph_descendant_of` against it. Decisions/0009's retry loop is untouched —
it still only fires on an actual push rejection, and a rewrite is
re-detected fresh on every loop iteration rather than assumed from a retry
count. On a detected rewrite, the rebuild target comes from the existing
`newest_dest_marker_opt_for_branch(repo, source_tip, ...)` call the
`!dest_ref_exists` arm already uses — no second base-finding path.

The exhausted-retries message (`sync.rs:519`, `sync.rs:1404` before this
change) is now built by one pure function,
`divergence_after_exhausted_retries_message(branch, ff_target)`: names the
branch, says the two sides diverged, and defers to the operator via
"ordinary git" — verified by a direct unit test to never contain "merge",
"rebase", or "cherry-pick".

Tests added (`src/git.rs`, `src/commands/sync.rs`): `push_refspec`'s two
wire-shape unit tests; `push_force_mirror_only_overwrites_a_diverged_dest_branch_outright`
(an outright force, no lease semantics — no expected-old-value is ever
passed to `push`); three rewrite fixtures —
`sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_a_rebase`,
`..._by_an_amend`, and
`..._reset_to_an_earlier_commit_plus_a_new_commit` — each confirming the old
mirror history is replaced and dest ends at the rewritten content;
`sync_pair_to_dest_incorporates_a_benign_race_on_a_mirror_only_branch_via_recompute_not_force`,
which pre-pushes a gitprism-shaped dest commit naming exactly the source tip
already being synced (`boundary == source_tip`, so rewrite detection is
never even reached) and confirms the next sync extends it by ordinary
fast-forward rather than replacing it; the authority-invariant pair
`sync_pair_to_dest_discards_a_mirror_only_branchs_content_naming_an_unrelated_source_commit`
and `..._stops_..._instead`, built from one shared fixture that gives dest a
*validly marked* gitprism commit naming a sibling source commit this clone
never had — identical dest-side state, differing only in `feature-x`'s
`config.branches` membership, proving that membership alone licenses the
discard; and the message unit test above. Pre-implementation, the rebase
test failed with the old refusal ("isn't at a point this clone can safely
build on..."), reproduced by temporarily disabling the new detection arm;
the benign-race test failed to compile at all against the pre-task `git::push`
signature (13 errors, including its own call site), since it directly
exercises the new `PushMode` API.

Verification: `cargo test` — 213 passed, 0 failed (baseline 203 plus 10 new:
3 in `git.rs`, 7 in `sync.rs`). `cargo clippy --all-targets -- -D warnings` —
clean. `cargo fmt --check` — clean after `cargo fmt`.

## 2026-08-21 — mirror-only skip message states what was observed

**Update**: `sync_pair_to_dest_with_key`'s decisions/0018 Case 2 skip note
read "already merged into {landing:?}, cleaned up there" — asserting a
deletion that may never have happened, since the same note fires for a
branch that never had a dest ref at all (its filtered content simply
coincides with the landing branch's). Replaced with a new pure function,
`mirror_only_skip_note(landing)` (`src/commands/sync.rs`), returning "its
filtered content is already fully present in {landing:?}" — true in both
cases the classification covers, with no claim of deletion, cleanup, or
prior existence on dest. Unit-tested directly
(`mirror_only_skip_note_names_the_landing_branch_and_asserts_no_deletion_or_prior_existence`),
following the precedent already set by `policy_mismatch_message` and
`divergence_after_exhausted_retries_message`: extract the note, test the
pure function, since the reporter has no capturing sink. No existing test
asserted the old wording as a behavioral expectation, so none needed
updating. `decisions/0037`'s note that this correction was "a separate,
still-open item" is updated to record it as resolved; `decisions/0018`
itself never quoted the old string verbatim, so it needed no change.
`decisions/0020`'s two verbatim quotes of the pre-indicatif `eprintln!`
output (its own historical "today's call sites" example, dated before this
fix) were left as-is — they document what the code said at the time that
decision was written, not a claim about current wording.

Verification: `cargo test` — 214 passed, 0 failed (baseline 213 plus 1 new).
`cargo clippy --all-targets -- -D warnings` — clean. `cargo fmt --check` —
clean.

## 2026-08-21 — unreadable control-file entry stays a per-branch mismatch

**Update**: Fixed `read_control_file_blob` (`src/commands/sync.rs`) aborting the whole `run` — instead of just that branch (decisions/0037) — when a pending commit's `.gitprismignore`/`.gitprism.toml` was a directory/gitlink (`find_blob` failing) or over `MAX_CONTROL_FILE_BYTES` (`anyhow::bail!`); both now classify as that branch's `PolicyMismatch` via a new `PolicyMismatchReason`, and `policy_mismatch_message` states the actual reason. Addendum recorded in `decisions/0037`.

Verification: `cargo test` — 217 passed, 0 failed (baseline 214 plus 3 new).
`cargo clippy --all-targets -- -D warnings` — clean. `cargo fmt --check` —
clean.

## 2026-08-21 — 0038 amended to flag its own overturned retry-escalation framing

**Update**: [decisions/0038](decisions/0038-branch-authority-determines-whether-history-may-be-rewritten.md)
still framed mirror-only force as "escalate after decisions/0009's retries
are exhausted" throughout its front-matter description and body, despite
[decisions/0039](decisions/0039-mirror-only-source-rewrites-rebuild-the-projection.md)
explicitly overturning that framing as load-bearing (0039: "force is not an
escalation after failed retries"). Left as-is, a `status: stable` record
reads as current guidance and could lead a future reader to implement the
rejected escalation shape without ever reaching 0039. Corrected the
front-matter description, added a supersession note before `# Context`
naming what 0039 overturned (the retry-escalation trigger) and what still
stands (the branch-authority rule itself), and annotated every stale
in-body passage inline with a bracketed pointer to 0039 rather than
deleting or rewriting the original reasoning. `--force-with-lease` passages
were left completely untouched — that question is under separate active
review. No code changed.

## 2026-08-21 — mirror-only force becomes a compare-and-swap lease

**Update**: Decided [decisions/0040](decisions/0040-mirror-only-force-is-a-compare-and-swap-lease.md)
— an external review found `PushMode::ForceMirrorOnly`'s unconditional
`+<oid>:refs/heads/<branch>` refspec silently overwrites a concurrent dest
advance between fetch and push, making decisions/0009's refetch-and-recompute
retry loop unreachable for that race. Fixed by pushing with an explicit
`--force-with-lease=refs/heads/<branch>:<fetched-dest-oid>` instead, verified
against real git: a stale lease is rejected with the same `!`/`[rejected]`
porcelain shape `is_non_fast_forward_rejection` already matches, so the
existing retry loop already routes it into decisions/0009's mechanism with no
new plumbing. `PushMode::ForceMirrorOnly` gains a required `expected_dest:
Oid` field. Revises decisions/0038's blanket "no lease mechanism anywhere"
conclusion for mirror-only force only — a lease still must never enable
force on a round-tripped branch — annotated in place the same way `119c16b`
annotated 0038's retry-escalation passages. No code changed.
