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
