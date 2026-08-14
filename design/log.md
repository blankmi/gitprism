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
