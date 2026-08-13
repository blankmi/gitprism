# Concepts

* [josh](josh.md) - Git virtualization proxy for bidirectional, incremental, reversible history filtering. Closest prior art to this project.
* [copybara](copybara.md) - Google's origin/destination code-migration tool. Relevant for its workflow-mode vocabulary and its commit-trailer approach to sync state.
* [jujutsu](jujutsu.md) - Rust git-compatible VCS. Relevant for its git-backend split (gix locally, real `git` subprocess for push/fetch) — the direct precedent for gitprism's hybrid backend decision.
* [git-subtree](git-subtree.md) - Built-in git command for embedding/splitting a subdirectory as its own history; native but unidirectional-ish and uncached.
* [git-filter-repo](git-filter-repo.md) - The tool the project owner explicitly ruled out, and why.
