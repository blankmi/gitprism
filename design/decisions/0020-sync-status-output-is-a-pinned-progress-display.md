---
type: Decision
title: Sync's status output becomes a pinned progress bar over colored, scannable branch/result lines
description: sync's five eprintln! call sites (all in src/commands/sync.rs) are replaced with an indicatif-backed display, Gradle rich-console-style — a pinned bottom region (overall progress bar plus a current branch/step line) with every completed branch-operation printed as a permanent line above it once it finishes. Each completed line is a short colored summary (done/skipped/error in green/yellow/red; the branch name in cyan if it's round-tripped per config.branches, plain if it's a decisions/0017 mirror-only branch) with any explanatory "why" text demoted to an indented note line beneath it, Cargo's verb/noun-plus-note convention. The overall total is computed upfront — source's branch list is read once before either phase starts, not discovered mid-run — so the bar's denominator is accurate from the very first line. Falls back to today's plain sequential lines whenever stderr isn't a terminal (CI, redirected logs), the same non-interactive case design/playbooks/0001 already documents.
tags: [ux, cli, output, sync]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-18T00:00:00Z }
---

# Context

`sync`'s only status output today is five `eprintln!` call sites in
`src/commands/sync.rs`, all plain sentences with no color or structure, e.g.:

```
main: fetching dest (finding resume point before merging from source)
feature/foo: fetching dest (mirror-only branch, not round-tripped)
feature/foo: not recreating on dest — already merged into "main" and cleaned up there (expected for a mirror-only branch)
```

(Confirmed via `rg 'eprintln!|println!' src` that this is the entire surface
— `setup` and `resolve` have no equivalent per-item status output today, so
this decision is scoped to `sync` alone.)

[decisions/0017](0017-source-to-dest-mirrors-every-branch.md) made source→dest
discover and mirror *every* branch on source unconditionally, with no config
entry required — so a real run's branch count, and therefore its line count,
now scales with however many branches happen to exist on source, most of
which are the mirror-only majority rather than the small `config.branches`
set. Plain undifferentiated lines don't scale with that: there's no visual
way to tell a round-tripped branch's line from a mirror-only one, or a
routine skip from an actual error, without reading every word.

Prior art checked before designing this:

* **Gradle's console output** distinguishes a "rich" mode — a pinned status
  region at the bottom of the terminal showing overall progress and the
  currently-executing task, with already-completed output scrolling above it
  untouched — from a "plain" mode that degrades to linear text the moment
  output isn't attached to a real terminal (or `--console=plain` is passed
  explicitly). The rich/plain duality, not just the visual, is the part worth
  copying: gitprism runs both interactively and headless in CI
  (`design/playbooks/0001-gitlab-pipeline-triggers.md`), the same two
  contexts Gradle's own duality exists for.
* **Cargo's own CLI output** uses a right-aligned, bold-colored verb ("
  Compiling", "   Fetching") for the scannable summary, keeping any further
  explanation on a separate line rather than folding it into the colored
  line itself — real prior art for a Rust tool's own audience specifically.
* [Evil Martians' survey of progress-display
  patterns](https://evilmartians.com/chronicles/cli-ux-best-practices-3-patterns-for-improving-progress-displays)
  ranks spinner / "X of Y" / progress bar by information density and
  recommends "X of Y" (a progress bar being that pattern plus a visual
  gauge) as the default for any step sequence you can actually count —
  branches here are exactly that: countable up front, not open-ended.
* [`indicatif`](https://github.com/console-rs/indicatif) is the standard,
  actively maintained Rust crate for this shape: `MultiProgress` supports one
  or more pinned bars plus `MultiProgress::println(...)`, which prints a
  permanent line above the pinned group without disturbing it — the direct
  mechanism for "completed lines scroll above a pinned progress area."
  Non-TTY output is auto-detected via its `ProgressDrawTarget` and the bar
  simply doesn't render, which is the seed of the plain-mode fallback this
  decision also requires.

# Decision

1. **New direct dependency: `indicatif`** (pinned to a specific version in
   `Cargo.toml` at implementation time, matching this project's existing
   convention of pinning every dependency rather than using a wildcard). It
   pulls in the `console` crate transitively, which is also used directly for
   arbitrary text coloring (the branch-name/result coloring below isn't
   expressible as a progress-bar template alone).

2. **Layout**: one `indicatif::MultiProgress` group, pinned at the bottom of
   the frame, holds a single bar showing overall progress (`[k/n] branches`)
   with the current branch/direction/step as its message (e.g. `main → dest:
   fetching dest`). Every completed branch-operation is emitted via
   `MultiProgress::println(...)`, becoming an ordinary, permanent line in
   scrollback above the bar — Gradle's shape, not a bespoke one.

3. **Total (`n`) is computed upfront, not discovered mid-run.** Today,
   `run()` only lists source's local branches (for the source→dest phase)
   *after* the dest→source phase for `config.branches` has already finished
   (`sync.rs:122`–`134`). That listing moves earlier — read once, before
   either phase starts, purely to size the bar — so `n =
   config.branches.len() + <source's branch count>` is known and fixed from
   the very first line, rather than the bar's denominator jumping once
   partway through the run. This is the one real control-flow change `run()`
   needs; the listing itself is unchanged (a local, read-only `git branch`
   enumeration, no fetch involved), just moved earlier and reused rather than
   repeated.

4. **Colors**:
   * Result: `done` green, `skipped` yellow, `error` red — the
     near-universal convention (Cargo, Gradle, GitHub Actions all agree on
     this mapping).
   * Branch name: **cyan if the branch is in `config.branches`
     (round-tripped)**, plain/default terminal foreground otherwise (a
     decisions/0017 mirror-only branch). Deliberately *not* dimming the
     mirror-only majority — dim conventionally signals "de-emphasized, look
     here last," which misrepresents mirror-only branches (they're the
     normal, zero-config case decisions/0017 was built for, not a lesser
     one), and a dim line is measurably harder to pick out on a fast scan
     than a plain one. Coloring the smaller round-tripped set instead makes
     it the one that visually pops, without implying the larger set is
     unimportant. Cyan specifically because green/yellow/red are already
     spoken for by the result color, and cyan already carries the same
     "notable, not urgent" role in Cargo's `note:` lines and npm's info
     output — no new meaning to learn, no clash.

5. **Line shape**: a short colored summary (result + branch name + direction,
   e.g. `done   main → dest`), with any explanatory text today's `eprintln!`
   call sites currently inline (e.g. "already merged into \"main\" and
   cleaned up there", "lost a push race, recomputing") demoted to an
   indented note line underneath — Cargo's convention of keeping the colored
   line itself short and the reasoning separate.

6. **Non-TTY fallback**: when stderr isn't a terminal — the exact situation
   `design/playbooks/0001` documents (a CI pipeline trigger) — `indicatif`
   already doesn't render the pinned bar at all by default. The colored
   summary/note-line formatting follows the same detection and falls back to
   plain text equivalent in content to today's `eprintln!` lines, so a CI
   log still reads as a linear, undecorated sequence of branch/status lines —
   not a bar, not ANSI color codes, not a two-line summary/note split that
   only makes sense with a terminal's line-wrapping.

# Why

* **Gradle's rich/plain duality is the closest existing prior art to what's
  needed here**, and it's a duality, not just a rich-mode look — gitprism's
  own two real run contexts (interactive terminal, headless CI) are the same
  two Gradle's split already exists for.
* **`indicatif` implements the exact mechanism this needs**
  (pinned bar plus print-above-without-disturbing, automatic TTY detection)
  as an actively maintained crate — no bespoke ANSI cursor handling to build
  or maintain.
* **Cargo's verb/noun-plus-note-line convention keeps the common case
  scannable** even when a line has real explanation to give — the option of
  cramming everything onto one line was considered and rejected precisely
  because decisions/0017 already made the common case be a long list of
  mostly-uneventful mirror-only branches; a wall of same-length, same-shape
  sentences is what's being moved away from.
* **Distinct hues over dimming, argued through directly**: dimming was the
  first idea and was wrong — it borrows a "de-emphasize" meaning that doesn't
  apply to the majority-case branches, and reads worse on a fast scan than a
  plain line does. Giving the smaller, higher-stakes round-tripped set its
  own color achieves "notice this" without that mismatch.
* **The total-upfront choice matches how Gradle itself computes a percentage
  at all** — it can only show "75% EXECUTING" because it builds its whole
  task graph before executing anything; gitprism's equivalent is listing
  source's branches once before either phase starts instead of discovering
  them mid-run, which was accepted as a small, safe reordering (a read-only
  local listing) rather than a growing/re-baselining bar.

# Consequences

* **New dependency**: `indicatif` (plus `console` transitively, and possibly
  as a direct dependency too for the text-styling calls that aren't part of
  a progress-bar template).
* **All five `eprintln!` call sites in `src/commands/sync.rs` are replaced**,
  routed through a small new formatting layer (module/function shape to be
  decided at implementation time, test-first per this project's TDD
  convention). `setup.rs` and `resolve.rs` are unaffected — they have no
  equivalent per-branch status output today.
* **`run()`'s branch-discovery for source→dest moves earlier** — computed
  once, before the dest→source phase starts, to size the bar's total instead
  of being discovered fresh right before the source→dest loop as it is
  today. Still one listing, not two.
* **Purely a presentation-layer change**: no effect on merge/push/conflict
  logic, trailers, resume semantics, or any existing regression test in
  `sync.rs`'s `#[cfg(test)]` module — those assert on real git state (oids,
  refs, trailer contents), never on stderr text, so none of them are expected
  to need updating for this alone.
* **CI/log-file output keeps today's plain-line shape** (Decision, point 6),
  so nothing about `design/playbooks/0001`'s documented CI usage is expected
  to break or need re-tuning for this change.
* **Not decided here**: the exact wording of every summary/note line, or the
  precise `indicatif` template strings/format — implementation detail, to be
  worked out test-first rather than fixed in this decision.
