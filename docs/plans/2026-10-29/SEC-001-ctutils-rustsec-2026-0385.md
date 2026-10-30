# Plan SEC-001 — remediate RUSTSEC-2026-0385 and gate releases on dependency policy

| | |
| --- | --- |
| Finding | CI run 40849312781 on `a2fa7a0`: "Dependency policy" and "Security audit" fail |
| Priority | P0 (operational: `main` CI red, affected releases published); exploit severity not established |
| Effort | Small |
| Decision required | No — dependency bump and release operations; playbook 0002 changes |
| Depends on | — |
| Status | In progress: step 1 merged (#31); RUSTSEC-2026-0418 fixed (#32) |

## Problem

[RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385.html)
(published 2026-10-23): `ctutils` < 0.4.4 can emit branches in
`Choice::to_*_mask` and in `u128` comparisons on 32-bit targets. Patched in
0.4.4 ([RustCrypto/utils#1569](https://github.com/RustCrypto/utils/pull/1569)).

```text
ctutils 0.4.2 ← digest 0.11.3 ← hmac 0.13.0, sha2 0.11.1 ← gitprism
```

`src/marker.rs:379` authenticates mapping markers with
`HmacSha256::verify_slice`, which decision 0025 requires to be constant-time.

Affected releases, from each tag's `Cargo.lock`:

| Tag | `ctutils` |
| --- | --- |
| v0.1.0 – v0.1.8 | not present |
| v0.1.9, v0.1.10, v0.1.11 | 0.4.2 |

Release gap: CI on `main` has been failing on this advisory since `94f4098`
(07:10Z), and v0.1.11 was still tagged from `main` at 09:10Z.
`release.yml` builds, tests and publishes without running audit or
cargo-deny, and does not depend on the CI result for the tagged commit.

## Exploitability assessment

Unestablished. Reachability was checked at the source level only, not at the
assembly level. `digest` `verify_slice` calls `[u8]::ct_eq`. In `ctutils`
0.4.2 that delegates to `cmov::CmovEq`, which compares the slices in
`u32`/`u64` chunks and converts the result through `to_u8`/`black_box`. That
path does not call `Choice::to_*_mask` or the `u128` comparisons. Even if it
were reachable, an attack would need forged markers to reach a keyed worker,
plus repeated precise timing measurements. An attacker can vary the supplied
MAC while preserving the authenticated payload, so fixed-payload chosen-tag
queries are possible. Exploitation remains unestablished because source
inspection does not show this path reaching the advisory's affected
operations, and a usable side channel has not been demonstrated. No key
rotation or mapping-history change is warranted. The advisory is patched and
not ignored.

## Steps

### Step 1 — dependency PR (lockfile only)

* Run `cargo update -p ctutils`. It resolves 0.4.2 → 0.4.7.
* `git diff` must show only the `ctutils` entry in `Cargo.lock`.
* Validate locally:
  * `cargo audit`
  * `cargo deny check --all-features`
  * `cargo test --workspace --all-features --locked`
  * `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
* Merge once CI is green on all jobs.

### Step 2 — release gate PR

* Move the `Dependency policy` job from `ci.yml` into a reusable
  `.github/workflows/dependency-policy.yml` (`on: workflow_call`) that takes a
  required `ref` input. Keep the same pinned actions.
* Pass `ref` explicitly. Under `workflow_dispatch` the default checkout is the
  dispatching branch, not `inputs.tag`.
* Grant `contents: read` and `checks: write` on every calling job. A reusable
  workflow cannot have more permissions than its caller
  ([GitHub docs](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations)).
* `ci.yml` calls it with `ref: ${{ github.sha }}`.
* `release.yml`:
  * Add job `dependency-policy`. It needs `verify-tag` and calls the reusable
    workflow with `ref: ${{ needs.verify-tag.outputs.commit }}`, the commit
    already verified by `verify-tag`. This is the same commit that `build` and
    `publish` check.
  * Add `dependency-policy` to `publish.needs`. If the audit fails or the
    advisory database is unavailable, the job fails and `publish` does not run.
* `release.yml` does not check for a separate CI result on the exact commit.
  It already builds and tests that commit on all four targets, and now runs
  audit and deny on the tag's own `Cargo.lock`.
* Update playbook 0002 "Publication":
  * Publication depends on dependency policy passing for the tag.
  * Tag only from a `main` commit whose CI run succeeded.

### Step 3 — verify the gate (negative test)

* After step 2 merges, run
  `gh workflow run release.yml --ref main -f tag=v0.1.11`.
* Acceptance requires both:
  * `dependency-policy` fails with output identifying RUSTSEC-2026-0385.
  * `publish` is skipped.
* A failure from permissions or configuration does not pass the test.

### Step 4 — release v0.1.12

* Check that CI on `main` at the step 1 and step 2 merge commit passed.
* Commit `Bump version to 0.1.12`, updating `Cargo.toml` and the `gitprism`
  package entry in `Cargo.lock`, and push it.
* Wait for CI on that exact commit to pass.
* Tag `v0.1.12` and push the tag.
* Confirm `dependency-policy`, all builds and `publish` succeed, and that the
  release has 4 archives plus `SHA256SUMS`.

### Step 5 — notes on affected releases

* Prepend to the release notes of v0.1.9, v0.1.10 and v0.1.11:

  > Includes `ctutils` 0.4.2, covered by RUSTSEC-2026-0385. Upgrade to
  > [v0.1.12](https://github.com/blankmi/gitprism/releases/tag/v0.1.12).

* Keep the existing tags and artifacts.

### Step 6 — design log

Add a `design/log.md` entry for 2026-10-29 covering:

* the advisory
* the affected dependency path and releases (v0.1.9–v0.1.11)
* the fix: `ctutils` 0.4.7, released in v0.1.12
* the exploitability assessment above
* the release-gate change and the playbook 0002 update

Decision 0025 is unchanged.

## Out of scope

* Key rotation, marker format changes, mapping history rewrites.
* Assembly-level constant-time verification of `verify_slice`.
* Scheduled audit runs on `main` between pushes.
