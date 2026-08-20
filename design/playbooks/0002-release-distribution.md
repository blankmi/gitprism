---
type: Playbook
title: gitprism release distribution
description: How to validate, package and publish gitprism's approved unsigned GitHub release archives. Operational guidance; source remains unpublished on crates.io.
tags: [operations, release, github]
status: stable
generated: { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
verified:
  - { by: "human:michael.blank@evia.de", at: 2026-08-20T00:00:00Z }
---

# Release distribution

This playbook defines the approved distribution policy for gitprism. It is
operational guidance, not part of the tool's synchronization architecture.

## Tagging

Each release tag must be exactly `v<package-version>`, where
`<package-version>` is the version in `Cargo.toml`. A tag with any other name
is not a release input.

## Artifacts

Each release must provide one archive for each supported distribution target:

* Linux x86_64
* macOS arm64
* macOS x86_64
* Windows x86_64

Every release must also include a `SHA256SUMS` file covering every archive.
Artifacts are intentionally unsigned. `SHA256SUMS` can detect corruption or
an incomplete transfer, but it does not authenticate who produced or published
the artifacts; provenance verification is outside the approved policy.

## Publication

gitprism remains unpublished on crates.io (`publish = false`). The supported
distribution path is the
[canonical GitHub repository](https://github.com/blankmi/gitprism) and its
source checkout;
operators build from source with the documented Rust and Git requirements or
download a GitHub release archive. The pinned release workflow accepts an
existing exact tag, builds/tests/smoke-checks all four targets, collects the
archives, generates `SHA256SUMS`, creates one draft release with all archives
and the checksum file, and publishes the release after the assets are present.
A first tagged release still needs to be run and inspected by an operator.

Installer and package-manager integrations are optional and are not release
blockers under this policy.
