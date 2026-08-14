# gitprism

A Rust tool to sync two git repositories (a **source** and a **dest**) where source
is a superset of dest's content plus files/folders that must never leave source.
Source→dest is filtered and fast-forward-only; dest→source brings dest's independent
changes (e.g. merged PRs) back into source. No `git-filter-repo`-style force-pushing
— see `design/references/git-filter-repo.md` for why that's a hard constraint, not a
preference.

`CLAUDE.md` is a symlink to `AGENTS.md`. Do not read both. Modifications must be done in `AGENTS.md`

# Where the design lives

All architecture context — what's being built, why, and what was considered and
rejected — is recorded in `design/`, an [OKF](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
bundle. **Start at `design/index.md`.**

Layout:

* `design/requirements/` — the workflow this tool must support, in the project
  owner's own terms, plus any questions not yet resolved.
* `design/references/` — prior art checked before designing anything (josh, Copybara,
  git-subtree, git-filter-repo, jujutsu), with real sources, and what each one
  contributed or ruled out.
* `design/decisions/` — architecture decisions, numbered (`0001`, `0002`, ...), each
  with Context / Decision / Why / Consequences. This is the source of truth for what
  the tool actually does; don't infer architecture from a summary here, read the
  decision file.
* `design/playbooks/` — operational/deployment guidance (e.g. CI trigger setup) that
  is explicitly *not* part of the tool's own architecture — gitprism itself is
  trigger-agnostic by construction.

Each subdirectory has an `index.md` listing its concepts and a root `design/log.md`
recording the chronological history of what was added/decided and when.

# Working style for this project

* Decisions get made one at a time, in conversation, checked against real prior art
  before a recommendation is offered — not assumed or batch-decided.
* Every decision is written to `design/decisions/` before it's treated as settled.
  If you're picking up this project again, check there before assuming how something
  works.
* The project owner wants to understand the design, not receive code without having
  walked through the reasoning first.
* Test-driven: write a failing test for the behavior first, then the implementation
  that makes it pass. Rust's built-in `#[test]` + `cargo test` is the framework;
  reach for an added dev-dependency (e.g. `tempfile`) only when the built-in tools
  can't express the test cleanly.
