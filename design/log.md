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
* `trailer_value` matches `Key: value` anywhere in a message rather than only in the
  final trailer block, so a commit message that merely *quotes* a trailer (a squash
  merge concatenating bodies, say) can poison the resume scan — verified: it stops the
  pair with "isn't an ancestor of dest's current tip", and that commit's content never
  reaches dest.
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
