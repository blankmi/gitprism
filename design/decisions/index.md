# Concepts

* [0001-build-bespoke-rust-tool](0001-build-bespoke-rust-tool.md) - Build our own filtering/mapping engine; use josh as a design reference, not a dependency.
* [0002-hybrid-git-backend](0002-hybrid-git-backend.md) - git2-rs for local object/merge work; real git subprocess for push/fetch.
* [0003-mapping-state-in-commit-trailers](0003-mapping-state-in-commit-trailers.md) - Source<->dest mapping lives in commit-message trailers; same mechanism drives resume and loop-prevention.
* [0004-exclude-list-versioned-in-source](0004-exclude-list-versioned-in-source.md) - Exclude-list is a committed, gitignore-style file inside source, not external config.
* [0005-branch-pairs-are-a-configured-list](0005-branch-pairs-are-a-configured-list.md) - N branch-pairs cost the same as 1; config is a list from day one.
* [0006-setup-uses-real-shared-history](0006-setup-uses-real-shared-history.md) - source's first commit is a real child of dest's tip; every future merge gets a native git merge-base.
* [0007-conflict-policy-hard-stop](0007-conflict-policy-hard-stop.md) - A real dest<->source conflict hard-stops that pair's sync rather than auto-resolving or skipping.
* [0008-ship-resolve-helper](0008-ship-resolve-helper.md) - `gitprism resolve` reproduces the conflict, hands off to normal git UX, and closes the trailer bookkeeping loop.
* [0009-push-race-refetch-and-recompute](0009-push-race-refetch-and-recompute.md) - A lost ff-only push race is handled by refetching dest and recomputing, not rebasing.
* [0010-preserve-author-stamp-committer](0010-preserve-author-stamp-committer.md) - Original author is preserved; gitprism stamps itself as committer, matching git's own rewrite conventions.
* [0011-exclude-list-is-gitignore-syntax](0011-exclude-list-is-gitignore-syntax.md) - Exclude-list is `.gitprismignore` using exact `.gitignore` syntax, self-excluding by default.
* [0012-config-versioned-in-source](0012-config-versioned-in-source.md) - Config (repo locations, branch pairs, committer identity) is `.gitprism.toml`, versioned in source, self-excluding like `.gitprismignore`; `setup` bootstraps it from disk before source's first commit exists.
* [0013-repo-urls-optional-fall-back-to-env-vars](0013-repo-urls-optional-fall-back-to-env-vars.md) - `[source].url`/`[dest].url` are optional in `.gitprism.toml`, falling back to `GITPRISM_SOURCE_URL`/`GITPRISM_DEST_URL` when omitted — keeps credential-bearing/environment-specific remotes out of source's committed history.
* [0014-source-to-dest-becomes-diff-based-and-can-conflict](0014-source-to-dest-becomes-diff-based-and-can-conflict.md) - source→dest applies each pending commit as its own filtered diff, not a full-tree snapshot — required once dest→source can land content between two not-yet-pushed source commits; supersedes decisions/0009's "no merge semantics," extending decisions/0007's hard-stop to source→dest too.
* [0016-both-directions-merge-via-real-git-merge-tree](0016-both-directions-merge-via-real-git-merge-tree.md) - Both sync directions delegate the 3-way merge to a real `git merge-tree --write-tree` subprocess over pre-filtered trees, replacing source→dest's non-idempotent patch application *and* dest→source's `cherrypick_commit`; supersedes decisions/0014's mechanism and amends decisions/0002's local-merge clause.
* [0015-resolve-real-git-cherry-pick-explicit-continue](0015-resolve-real-git-cherry-pick-explicit-continue.md) - `gitprism resolve <pair>` drives a real `git cherry-pick` subprocess (so conflicts get ordinary working-tree markers), identifies the commit by recomputing sync's own pending list, and resumes only via an explicit `--continue` flag matching git's own convention. Dest→source only for now; source→dest's diff-conflict shape is a follow-up.

