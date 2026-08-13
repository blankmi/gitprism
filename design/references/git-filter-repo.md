---
type: Reference
title: git-filter-repo
description: The standard modern tool for git history surgery; explicitly ruled out for this project because it rewrites history and requires force-pushing every run.
resource: https://github.com/newren/git-filter-repo
tags: [prior-art, git, ruled-out]
sources:
  - { id: issue227, resource: "https://github.com/newren/git-filter-repo/issues/227", title: "(Force-)pushing rewritten commits to GitHub has no effect" }
  - { id: manpage, resource: "https://manpages.debian.org/testing/git-filter-repo/git-filter-repo.1.en.html", title: "git-filter-repo(1)" }
generated: { by: "human:michael.blank@evia.de", at: 2026-08-13T00:00:00Z }
status: stable
---

# What it is

The recommended successor to `git filter-branch`: fast, flexible history rewriting
(path filtering, author rewrites, etc.), operating on a fresh clone.[^manpage]

# Why it's ruled out here

It rewrites commit history wholesale on every run. Every rewritten commit gets a new
hash, so keeping a downstream in sync means force-pushing every single time, and every
other clone of that downstream must be reset to match or it recontaminates the rewrite
on its next ordinary `pull && push`.[^issue227] That's fine for a one-time repo
extraction or history cleanup; it's the wrong shape for a repo meant to stay
continuously in sync via ordinary fast-forward pushes.

# Relevant difference from our problem

This isn't a gap to design around — it's confirmation that the project owner's
instinct to avoid it is correct for a continuous-sync tool. It stays here as the
documented reason, not as a candidate.
