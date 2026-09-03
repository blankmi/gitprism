//! The first-parent marker revwalks shared by [`super`]'s main loop and
//! [`super::anchor`]'s mapping-index reconstruction and lookup — see
//! decisions/0019 for why every scan here is restricted to first-parent
//! history.

use std::collections::HashSet;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::limits;
use crate::marker::{self, Direction as MarkerDirection};

/// Every commit reachable from `source_tip` via first-parent history
/// (decisions/0019, `Revwalk::simplify_first_parent()`), newest first,
/// scanned for the newest commit carrying a `Gitprism-Dest-Commit` trailer
/// (decisions/0003) — a `Setup` graft (decisions/0006) or a `DestToSource`
/// marker scoped to `branch`. Returns `(that source commit's own oid, the
/// dest-space oid it names)`.
///
/// This is B1 in decisions/0048's dest→source boundary rule — a precondition
/// [`dest_to_source_boundary`] always checks before trusting it, and a lower
/// bound its own dest-side walk can widen past, but never the whole boundary
/// by itself any more.
///
/// `Ok(None)` means no such trailer is reachable at all — for a properly
/// set-up branch this never happens (setup's own graft commit always
/// carries one and is always a first-parent ancestor), but a branch
/// discovered on source with no ancestry to any grafted branch
/// (decisions/0017, decisions/0024) genuinely has none. Which of those two
/// meanings applies is the caller's call — see [`newest_dest_marker`] below
/// and [`super::anchor::dest_anchor_for_branch`].
///
/// First-parent-only makes a merged-in mirror-only branch's own history
/// unreachable from this scan entirely — a full-ancestry walk used to mean
/// a mirror-only branch merged into this one via a real, two-parent merge
/// (decisions/0017 makes every source branch eligible to be merged into
/// another) could hand this scan *that* branch's own `Gitprism-Dest-Commit`
/// trailer, reachable only through the merge's non-first parent, instead of
/// this branch's own. This relies on the tracked branch staying first-parent
/// of its own merges — true for GitHub/GitLab/Azure DevOps' "merge PR"
/// button and for `git merge` run from the target branch, not guaranteed
/// otherwise (decisions/0019's documented limitation).
pub(super) fn scan_for_dest_marker(
    repo: &Repository,
    source_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<(Oid, Oid)>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting source's resume-point scan")?;
    revwalk
        .push(source_tip)
        .context("seeding source's resume-point scan")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .context("ordering source's resume-point scan newest-first")?;
    revwalk.simplify_first_parent().context(
        "restricting source's resume-point scan to first-parent history (decisions/0019)",
    )?;

    for (scanned, oid) in revwalk.enumerate() {
        if scanned >= limits::MAX_MARKER_SCAN_COMMITS {
            anyhow::bail!(
                "source resume-point scan exceeds the {} commit limit",
                limits::MAX_MARKER_SCAN_COMMITS
            );
        }
        let oid = oid.context("walking source's history for a resume point")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in source's history")?;
        if let Some(dest_oid) = marker::verify(
            &commit,
            branch,
            &[MarkerDirection::Setup, MarkerDirection::DestToSource],
            None,
            key,
        ) {
            return Ok(Some((oid, dest_oid)));
        }
    }

    Ok(None)
}

/// [`scan_for_dest_marker`], bailing when nothing is found — used by
/// [`super::anchor::dest_tip_accounted_for`],
/// [`super::anchor::dest_tip_represented_in_source`] (decisions/0048's
/// source→dest safety widening, CODE-001 step 5), and by
/// [`dest_to_source_boundary`] below (which [`super::pending_dest_commits`]
/// calls for B1), whose branch always comes from `config.branches`, where
/// `setup` (decisions/0006, decisions/0023) guarantees the trailer exists.
/// Finding none really does mean "has `gitprism setup` been run for this
/// pair?"
pub(super) fn newest_dest_marker(
    repo: &Repository,
    source_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<(Oid, Oid)> {
    scan_for_dest_marker(repo, source_tip, branch, key)?.ok_or_else(|| {
        anyhow::anyhow!(
            "gitprism sync: no Gitprism-Dest-Commit trailer found anywhere in source's history — has `gitprism setup` been run for this pair?"
        )
    })
}

/// dest's own resume boundary for [`super::pending_commits`]: the most recent
/// commit reachable from `dest_tip` carrying a `Gitprism-Source-Commit`
/// trailer (decisions/0003) — [`newest_dest_marker`]'s mirror, scanning dest
/// space for source's trailer instead of the reverse. Returns `Ok(None)`,
/// not a bail, when nothing is found anywhere: unlike `newest_dest_marker`
/// (which can always assume `setup`'s own graft commit carries a
/// `Gitprism-Dest-Commit` trailer), dest legitimately has no gitprism commit
/// at all before its very first sync.
///
/// The scan does NOT `revwalk.hide` the graft point as an optimization. Hiding
/// it would be a pure optimization on the usual case, but a marker sitting
/// *before* the graft point (e.g. a source repo re-grafted onto a dest
/// gitprism had already written to) would become invisible to the scan, and
/// the caller would then silently fall back to the graft and push instead of
/// refusing. The scan therefore retains the whole first-parent line subject
/// to the static [`limits::MAX_MARKER_SCAN_COMMITS`] work bound; exceeding it
/// fails safe instead of guessing a resume point.
///
/// What the walk *is* bounded to, since decisions/0019, is first-parent
/// history (`Revwalk::simplify_first_parent()`): a full-ancestry walk used
/// to mean that a mirror-only branch merged into this one on dest via a
/// real, two-parent merge (decisions/0017 makes every source branch
/// eligible to be merged into another) could hand this scan *that* branch's
/// own `Gitprism-Source-Commit` trailer — reachable only through the
/// merge's non-first parent — instead of this branch's own, wrongly
/// refusing (or, worse, wrongly resuming from a stale boundary) a perfectly
/// healthy sync. First-parent-only makes a merged-in branch's own history
/// unreachable from this scan entirely. This relies on the tracked branch
/// staying first-parent of its own merges — true for GitHub/GitLab/Azure
/// DevOps' "merge PR" button and for `git merge` run from the target
/// branch, not guaranteed otherwise (decisions/0019's documented
/// limitation).
pub(super) fn newest_source_marker(
    repo: &Repository,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<Oid>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting dest's resume-point scan")?;
    revwalk
        .push(dest_tip)
        .context("seeding dest's resume-point scan")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .context("ordering dest's resume-point scan newest-first")?;
    revwalk
        .simplify_first_parent()
        .context("restricting dest's resume-point scan to first-parent history (decisions/0019)")?;

    for (scanned, oid) in revwalk.enumerate() {
        if scanned >= limits::MAX_MARKER_SCAN_COMMITS {
            anyhow::bail!(
                "dest resume-point scan exceeds the {} commit limit",
                limits::MAX_MARKER_SCAN_COMMITS
            );
        }
        let oid = oid.context("walking dest's history for a resume point")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in dest's history")?;
        if let Some(source_oid) =
            marker::verify(&commit, branch, &[MarkerDirection::SourceToDest], None, key)
        {
            return Ok(Some(source_oid));
        }
    }

    Ok(None)
}

/// decisions/0048's dest→source resume boundary for `branch`: the end of the
/// longest contiguous prefix of dest `branch`'s first-parent line, starting
/// immediately after B1 (the dest commit [`newest_dest_marker`] finds on
/// *source's* history), for which every commit is **represented** in
/// `source_tip`. If every commit up to `dest_tip` is represented, the
/// boundary is `dest_tip` itself.
///
/// [`super::pending_dest_commits`] calls this for dest→source's own resume
/// point; [`super::anchor::dest_tip_represented_in_source`] (CODE-001 step
/// 5) reuses it verbatim to widen source→dest's own safety check for the
/// same shape — see that function's doc for why it additionally requires
/// the newest self-verified `SourceToDest` marker anywhere on the whole of
/// dest's first-parent line (not merely the B1-bounded range this boundary
/// walk itself uses — a follow-up CODE-001 fix found the narrower range
/// unsafe for that particular check) to be branded for the branch being
/// processed (or none at all), not merely that [`newest_source_marker`]
/// finds *something* branch-scoped on the line.
///
/// B1 is a precondition, checked first and unconditionally, exactly as
/// [`super::pending_dest_commits`] enforced it before this function existed:
/// if B1 is missing from the local object database, or
/// `graph_descendant_of(dest_tip, B1)` is false (and `B1 != dest_tip`), this
/// refuses with the same "isn't an ancestor of dest's current tip" message
/// before any further walk — a dest branch force-rewound behind an
/// already-imported commit must be reported, not silently accepted just
/// because its current tip happens to carry some other, older, legitimate
/// marker (decisions/0048's scenario 6; see the decision's "Why" for why
/// checking B1 first is what keeps this fail-closed).
///
/// Only once B1 is confirmed does a second, bounded walk of dest's
/// first-parent line run, from the commit immediately above B1 up toward
/// `dest_tip`, testing each commit in turn for representation — bounded both
/// by reaching `dest_tip` and by [`limits::MAX_MARKER_SCAN_COMMITS`], the
/// same static limit every other marker scan in this module uses
/// (decisions/0032). Locating B1 on this first-parent line can itself fail
/// even after the precondition above has passed — decisions/0019's
/// documented limitation (a tracked branch must stay first-parent of its own
/// merges) applies to this walk exactly as it already applies to
/// [`newest_dest_marker`]'s own scan — so exhausting dest's first-parent
/// line without ever reaching B1 is a clear, named refusal, not a raw
/// walked-off-the-end error.
///
/// A dest commit `D` is represented in `source_tip` when either:
///
/// 1. `D` is a valid `SourceToDest` marker, self-verified against its own
///    recorded branch ([`marker::verify_self`], not necessarily `branch`'s
///    own name), whose source counterpart is an ancestor of, or equal to,
///    `source_tip`. A counterpart missing from the local object database
///    disqualifies `D` from case 1 alone — it may still qualify via case 2;
///    or
/// 2. a valid, self-verified `DestToSource` marker — self-verified the same
///    way, against its own recorded branch, whatever that is — reachable
///    from `source_tip` on source's first-parent history carries a
///    `Gitprism-Dest-Commit` trailer naming `D`'s exact oid, regardless of
///    that marker's own recorded branch.
///
/// Case 2 deliberately names `DestToSource` only, not `Setup`: a `Setup`
/// graft is already B1's own baseline or below it, so a case-2 search above
/// B1 would never find one case 1 or B1 itself hasn't already accounted for.
/// Case 2 applies to any commit shape — a `SourceToDest` marker that fails
/// case 1 can still be saved by case 2; a dest-native or invalid/
/// unauthenticated marker commit can only be saved by case 2, having no
/// counterpart of its own to check under case 1.
///
/// The walk stops at the first commit represented by neither case; the
/// boundary is the commit immediately before it — B1 itself, a qualifying
/// `SourceToDest` marker, or an accepted dest-native commit.
///
/// Every case-2 check needs to know whether *something* reachable from
/// `source_tip` names a given dest oid — answering that per candidate with
/// its own fresh scan of source's history would cost the product of the two
/// bounds (decisions/0048's "Consequences"), so this walk instead builds, on
/// first need, a single set of every dest oid named by a valid, self-
/// verified `DestToSource` marker reachable from `source_tip`
/// ([`dest_to_source_marker_targets`]) and reuses it — an O(1) lookup — for
/// every subsequent case-2 check in this same boundary computation.
pub(super) fn dest_to_source_boundary(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Oid> {
    dest_to_source_boundary_bounded(
        repo,
        source_tip,
        dest_tip,
        branch,
        key,
        limits::MAX_MARKER_SCAN_COMMITS,
    )
}

/// [`dest_to_source_boundary`], with the scan bound injectable — a test-only
/// seam so the `MAX_MARKER_SCAN_COMMITS` bail can be exercised without a
/// fixture repository actually holding that many real commits (the same
/// technique [`super::mapping_index::MappingIndex::reconstruct_with_scan_limit`]
/// already uses for the identical problem). The same bound applies to both
/// the outer walk over dest's first-parent line and, independently, each
/// case-2 scan of source's first-parent line (decisions/0048).
fn dest_to_source_boundary_bounded(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
    scan_limit: usize,
) -> Result<Oid> {
    let (_, b1) = newest_dest_marker(repo, source_tip, branch, key)?;
    if b1 != dest_tip {
        let is_ancestor = repo.find_commit(b1).is_ok()
            && repo
                .graph_descendant_of(dest_tip, b1)
                .with_context(|| format!("checking whether {dest_tip} descends from {b1}"))?;
        if !is_ancestor {
            anyhow::bail!(
                "gitprism sync: source's last-synced dest commit ({b1}) isn't an ancestor of dest's current tip ({dest_tip})"
            );
        }
    }

    // Collect dest_tip..=b1's first-parent line. A first-parent walk can
    // only move child-to-parent, so this necessarily gathers newest-first;
    // it's processed in reverse (oldest-first, i.e. immediately above B1
    // toward dest_tip) below, which is the order decisions/0048's
    // contiguous-prefix rule is defined in.
    let mut chain: Vec<Oid> = Vec::new();
    let mut current = dest_tip;
    let mut scanned = 0usize;
    loop {
        if scanned >= scan_limit {
            anyhow::bail!("dest-to-source boundary scan exceeds the {scan_limit} commit limit");
        }
        scanned += 1;
        let commit = repo.find_commit(current).with_context(|| {
            format!(
                "resolving {current} while walking dest's history for decisions/0048's boundary"
            )
        })?;
        chain.push(current);
        if current == b1 {
            break;
        }
        if commit.parent_count() == 0 {
            anyhow::bail!(
                "gitprism sync: B1 boundary {b1} is not on dest's first-parent line from {dest_tip} (decisions/0019's first-parent limitation)"
            );
        }
        current = commit.parent_id(0).with_context(|| {
            format!(
                "walking dest's first-parent history past {current} looking for decisions/0048's B1 boundary {b1}"
            )
        })?;
    }

    // Case 2's target set: built lazily, at most once, only if some
    // candidate actually fails case 1 and needs it.
    let mut case_two_targets: Option<HashSet<Oid>> = None;

    let mut boundary = b1;
    for &oid in chain.iter().rev().skip(1) {
        let commit = repo.find_commit(oid).with_context(|| {
            format!("resolving {oid} while testing decisions/0048's representation rule")
        })?;

        let case_one =
            match marker::verify_self(&commit, &[MarkerDirection::SourceToDest], None, key) {
                Some(marker) => {
                    marker.counterpart == source_tip
                        || (repo.find_commit(marker.counterpart).is_ok()
                            && repo
                                .graph_descendant_of(source_tip, marker.counterpart)
                                .with_context(|| {
                                    format!(
                                        "checking whether {source_tip} descends from {}",
                                        marker.counterpart
                                    )
                                })?)
                }
                None => false,
            };

        let represented = if case_one {
            true
        } else {
            let targets = match &case_two_targets {
                Some(targets) => targets,
                None => {
                    case_two_targets = Some(dest_to_source_marker_targets(
                        repo, source_tip, key, scan_limit,
                    )?);
                    case_two_targets.as_ref().expect("just inserted above")
                }
            };
            targets.contains(&oid)
        };

        if !represented {
            break;
        }
        boundary = oid;
    }
    Ok(boundary)
}

/// Case 2's supporting index: every dest oid named by a valid, self-
/// verified `DestToSource` marker's `Gitprism-Dest-Commit` trailer reachable
/// from `source_tip` via first-parent history (decisions/0019), collected in
/// one forward scan bounded by `scan_limit` (decisions/0032, 0048's
/// "Consequences") — built once per boundary computation and reused for
/// every case-2 check, rather than rescanned per candidate.
///
/// A `Setup` graft is deliberately excluded — it is already B1's own
/// baseline or below it (decisions/0048).
fn dest_to_source_marker_targets(
    repo: &Repository,
    source_tip: Oid,
    key: &marker::StateKey,
    scan_limit: usize,
) -> Result<HashSet<Oid>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting source's case-2 marker scan")?;
    revwalk
        .push(source_tip)
        .context("seeding source's case-2 marker scan")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .context("ordering source's case-2 marker scan")?;
    revwalk.simplify_first_parent().context(
        "restricting source's case-2 marker scan to first-parent history (decisions/0019)",
    )?;

    let mut targets = HashSet::new();
    for (scanned, oid) in revwalk.enumerate() {
        if scanned >= scan_limit {
            anyhow::bail!(
                "dest-to-source boundary's case-2 source scan exceeds the {scan_limit} commit limit"
            );
        }
        let oid = oid.context("walking source's history for decisions/0048's case-2 markers")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in source's history")?;
        if let Some(marker) =
            marker::verify_self(&commit, &[MarkerDirection::DestToSource], None, key)
        {
            targets.insert(marker.counterpart);
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use git2::Signature;
    use tempfile::tempdir;

    use super::*;

    fn tree_with_file(repo: &Repository, parent: Option<Oid>, name: &str, body: &[u8]) -> Oid {
        let mut builder = repo
            .treebuilder(
                parent
                    .and_then(|oid| repo.find_commit(oid).ok())
                    .as_ref()
                    .and_then(|commit| commit.tree().ok())
                    .as_ref(),
            )
            .unwrap();
        let blob = repo.blob(body).unwrap();
        builder
            .insert(name, blob, git2::FileMode::Blob.into())
            .unwrap();
        builder.write().unwrap()
    }

    fn commit(
        repo: &Repository,
        reference: &str,
        parents: &[Oid],
        tree: Oid,
        message: &str,
    ) -> Oid {
        let signature = Signature::now("test", "test@example.com").unwrap();
        commit_with_signature(repo, reference, parents, tree, message, &signature)
    }

    /// [`commit`], with the signature supplied by the caller instead of
    /// freshly generated — required for any marker commit, whose MAC binds
    /// the signature's own timestamp (`marker.rs`'s `signature_field`): the
    /// same `Signature` used to build the marker message via
    /// [`dest_marker_message`]/[`source_marker_message`] must be the one
    /// the commit itself is created with, or a clock tick between the two
    /// `Signature::now()` calls makes the marker fail to verify. Matches
    /// `add_dest_marker_commit` in `tests::dest_to_source`'s own shared
    /// fixtures.
    fn commit_with_signature(
        repo: &Repository,
        reference: &str,
        parents: &[Oid],
        tree: Oid,
        message: &str,
        signature: &Signature<'_>,
    ) -> Oid {
        let parent_commits = parents
            .iter()
            .map(|oid| repo.find_commit(*oid).unwrap())
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        repo.commit(
            Some(reference),
            signature,
            signature,
            message,
            &repo.find_tree(tree).unwrap(),
            &parent_refs,
        )
        .unwrap()
    }

    fn dest_marker_message(
        branch: &str,
        counterpart: Oid,
        parents: &[Oid],
        tree: Oid,
        signature: &Signature<'_>,
    ) -> String {
        marker::build_message(
            "test dest -> source",
            MarkerDirection::DestToSource,
            branch,
            counterpart,
            "Gitprism-Dest-Commit",
            parents,
            tree,
            signature,
            signature,
            &marker::load_key().unwrap(),
        )
    }

    fn source_marker_message(
        branch: &str,
        counterpart: Oid,
        parents: &[Oid],
        tree: Oid,
        signature: &Signature<'_>,
    ) -> String {
        marker::build_message(
            "test source -> dest",
            MarkerDirection::SourceToDest,
            branch,
            counterpart,
            "Gitprism-Source-Commit",
            parents,
            tree,
            signature,
            signature,
            &marker::load_key().unwrap(),
        )
    }

    fn empty_repo() -> (tempfile::TempDir, Repository, Oid) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let root = commit(&repo, "refs/heads/root", &[], tree, "root");
        (dir, repo, root)
    }

    /// Unit-level counterpart of scenario 6a
    /// (`dest_to_source_refuses_when_the_boundary_marker_names_an_object_missing_from_the_local_odb`
    /// in `tests::dest_to_source`) — exercises `dest_to_source_boundary`
    /// directly with a minimal single-repo fixture, per the plan's step-2
    /// escape hatch, rather than only through a full `sync`/`resolve` run.
    #[test]
    fn refuses_when_b1_names_an_object_missing_from_the_local_odb() {
        let (_dir, repo, root) = empty_repo();
        let missing = Oid::from_str("abababababababababababababababababababab").unwrap();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message = dest_marker_message("main", missing, &[root], source_tree, &signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &signature,
        );

        let key = marker::load_key().unwrap();
        let error = dest_to_source_boundary(&repo, source_tip, root, "main", &key).unwrap_err();
        assert!(
            format!("{error:#}").contains("isn't an ancestor of dest's current tip"),
            "a B1 naming an object missing from the local odb must refuse before any walk: {error:#}"
        );
    }

    /// Unit-level counterpart of scenario 6b
    /// (`dest_to_source_refuses_when_dest_is_force_rewound_past_an_already_imported_commit`) —
    /// dest's current tip (`old_mirror`) carries a legitimate, self-verifying
    /// B2 marker, but B1 (`b1`, named by source's own history) is a
    /// commit *above* it that dest's current line no longer reaches — the
    /// precondition must refuse before the walk ever gets to see that
    /// `old_mirror` would otherwise satisfy B2.
    #[test]
    fn refuses_when_dest_is_force_rewound_behind_an_already_imported_b1() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_tip = commit(&repo, "refs/heads/source", &[root], source_tree, "source");

        let old_mirror_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let old_mirror_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let old_mirror_message = source_marker_message(
            "main",
            root,
            &[root],
            old_mirror_tree,
            &old_mirror_signature,
        );
        let old_mirror = commit_with_signature(
            &repo,
            "refs/heads/old-mirror",
            &[root],
            old_mirror_tree,
            &old_mirror_message,
            &old_mirror_signature,
        );

        // B1: source's own boundary names a dest commit that is NOT an
        // ancestor of `old_mirror` — standing in for a dest force-rewind
        // that discarded it.
        let b1_tree = tree_with_file(&repo, Some(root), "customer.txt", b"customer");
        let b1 = commit(
            &repo,
            "refs/heads/b1",
            &[root],
            b1_tree,
            "an already-imported commit",
        );
        let source_boundary_tree = tree_with_file(&repo, Some(source_tip), "boundary.txt", b"x");
        let source_boundary_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_boundary_message = dest_marker_message(
            "main",
            b1,
            &[source_tip],
            source_boundary_tree,
            &source_boundary_signature,
        );
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[source_tip],
            source_boundary_tree,
            &source_boundary_message,
            &source_boundary_signature,
        );

        let key = marker::load_key().unwrap();
        let error =
            dest_to_source_boundary(&repo, source_tip, old_mirror, "main", &key).unwrap_err();
        assert!(
            format!("{error:#}").contains("isn't an ancestor of dest's current tip"),
            "a dest tip that doesn't descend from B1 must refuse even though it carries its own \
             legitimate B2 marker: {error:#}"
        );
    }

    /// A self-verified `SourceToDest` marker sitting directly above B1, whose
    /// own counterpart is exactly `source_tip` — case 1 in its simplest
    /// shape, with nothing else on the line to test — must advance the
    /// boundary past B1 to that marker.
    #[test]
    fn case_one_marker_directly_above_b1_becomes_the_boundary() {
        let (_dir, repo, root) = empty_repo();

        // B1: source's own history names `root` itself as the boundary — as
        // if dest hasn't moved past setup from source's point of view.
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );
        let b1 = root;

        // B2: a self-verified SourceToDest marker on dest's own line whose
        // counterpart (source_tip) is exactly source's current tip.
        let b2_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let b2_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let b2_message = source_marker_message("main", source_tip, &[root], b2_tree, &b2_signature);
        let b2 = commit_with_signature(
            &repo,
            "refs/heads/dest",
            &[root],
            b2_tree,
            &b2_message,
            &b2_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary = dest_to_source_boundary(&repo, source_tip, b2, "main", &key).unwrap();
        assert_eq!(
            boundary, b2,
            "a case-1-qualifying marker directly above B1 ({b1}) must become the boundary ({b2})"
        );
    }

    /// The walk's B1 lower bound, tested directly: a commit that would
    /// otherwise be a perfectly good B2 (self-verified, counterpart is
    /// exactly `source_tip`) sits *below* B1 on dest's first-parent line —
    /// dest_tip is B1 itself, with nothing above it. The walk must stop at
    /// B1 without ever inspecting, let alone resuming from, anything
    /// beneath it (`tests::dest_to_source`'s
    /// `dest_to_source_resumes_from_its_own_import_marker_when_nothing_new_has_landed`
    /// covers the same property end to end, but its own fixture's B1
    /// happens to equal dest_tip trivially before ever reaching this
    /// commit — this test isolates the lower bound itself).
    #[test]
    fn ignores_a_qualifying_b2_shaped_marker_below_b1() {
        let (_dir, repo, root) = empty_repo();

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            "a source commit, no marker yet",
            &source_signature,
        );

        // A commit below B1 whose own counterpart is exactly source_tip —
        // if the walk ever reached it, it would trivially qualify as B2.
        let old_mirror_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let old_mirror_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let old_mirror_message = source_marker_message(
            "main",
            source_tip,
            &[root],
            old_mirror_tree,
            &old_mirror_signature,
        );
        let old_mirror = commit_with_signature(
            &repo,
            "refs/heads/dest",
            &[root],
            old_mirror_tree,
            &old_mirror_message,
            &old_mirror_signature,
        );

        // B1: a plain commit — the typical shape (a customer's own commit,
        // or the setup graft) — directly above the would-be B2, and also
        // dest's current tip: nothing sits above B1 at all.
        let b1_tree = tree_with_file(&repo, Some(old_mirror), "customer.txt", b"customer");
        let b1 = commit(
            &repo,
            "refs/heads/dest",
            &[old_mirror],
            b1_tree,
            "an already-imported commit",
        );

        let source_boundary_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_boundary_tree = tree_with_file(&repo, Some(source_tip), "boundary.txt", b"x");
        let source_boundary_message = dest_marker_message(
            "main",
            b1,
            &[source_tip],
            source_boundary_tree,
            &source_boundary_signature,
        );
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[source_tip],
            source_boundary_tree,
            &source_boundary_message,
            &source_boundary_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary = dest_to_source_boundary(&repo, source_tip, b1, "main", &key).unwrap();
        assert_eq!(
            boundary, b1,
            "the walk must stop at B1 ({b1}) and never resume from a qualifying-looking marker \
             beneath it ({old_mirror})"
        );
    }

    /// The `MAX_MARKER_SCAN_COMMITS` bail on the dest-side walk: more real
    /// commits sit between `dest_tip` and B1 than the (injected, small) scan
    /// limit allows, and none of them carries a qualifying B2 — the walk
    /// must fail closed rather than silently resume from a boundary it never
    /// actually confirmed.
    #[test]
    fn bails_past_the_injected_scan_limit_before_reaching_b1() {
        let (_dir, repo, root) = empty_repo();

        // B1: source's own history names `root` itself as the boundary.
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );
        let b1 = root;

        // Every commit above `root` on dest's line is a plain, unmarked
        // commit — never a B2 match.
        let mut tip = root;
        for index in 0..10 {
            let tree = tree_with_file(&repo, Some(tip), &format!("f{index}.txt"), b"x");
            tip = commit(
                &repo,
                "refs/heads/dest",
                &[tip],
                tree,
                "a plain dest commit",
            );
        }

        let key = marker::load_key().unwrap();
        let error =
            dest_to_source_boundary_bounded(&repo, source_tip, tip, "main", &key, 5).unwrap_err();
        assert!(
            format!("{error:#}").contains("5 commit limit"),
            "expected the scan-limit bail, got: {error:#}"
        );
        // Control: the identical walk succeeds once the limit covers the
        // whole span down to B1 ({b1}), so the bail above is really about
        // the limit, not some other mistake in the fixture.
        let boundary =
            dest_to_source_boundary_bounded(&repo, source_tip, tip, "main", &key, 11).unwrap();
        assert_eq!(boundary, b1);
    }

    /// decisions/0048's own worked example ("why case 1 alone is not
    /// enough"), at the unit level: a dest-native commit (`native`, no
    /// marker at all) sits directly above B1, and a case-1-qualifying
    /// `SourceToDest` marker (`case_one_marker`, whose own counterpart is
    /// trivially `source_tip` itself) sits above *that*. Round 1's bare
    /// ancestor check would accept `case_one_marker` immediately (it's
    /// `dest_tip`) without ever looking beneath it — silently skipping
    /// `native`'s content forever. Since nothing reachable from `source_tip`
    /// names `native`'s own oid (case 2 also fails for it), the walk must
    /// stop at `native` and report B1 as the boundary, not the marker above.
    #[test]
    fn case_one_alone_does_not_represent_a_dest_native_commit_beneath_it() {
        let (_dir, repo, root) = empty_repo();

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );
        let b1 = root;

        // A dest-native commit — no marker at all, and never imported (no
        // DestToSource marker anywhere reachable from source_tip names it) —
        // sits directly above B1.
        let native_tree = tree_with_file(&repo, Some(root), "customer.txt", b"customer");
        let native = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            native_tree,
            "a customer commit, never imported",
        );

        // A commit whose own counterpart is exactly source_tip — case 1
        // passes trivially — sits above the native commit.
        let case_one_tree = tree_with_file(&repo, Some(native), "dest.txt", b"dest");
        let case_one_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let case_one_message = source_marker_message(
            "main",
            source_tip,
            &[native],
            case_one_tree,
            &case_one_signature,
        );
        let case_one_marker = commit_with_signature(
            &repo,
            "refs/heads/dest",
            &[native],
            case_one_tree,
            &case_one_message,
            &case_one_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary =
            dest_to_source_boundary(&repo, source_tip, case_one_marker, "main", &key).unwrap();
        assert_eq!(
            boundary, b1,
            "a case-1-qualifying marker with an unimported dest-native commit beneath it must \
             not be trusted, even though its own counterpart is an ancestor of source_tip"
        );
    }

    /// decisions/0048's scenario 8: the positive counterpart of the test
    /// above. Same shape — a dest-native commit (`native`) followed by a
    /// `SourceToDest` marker (`case_one_marker`) — but this time `native`
    /// really was imported: a `DestToSource` marker reachable from
    /// `source_tip` names its exact oid, satisfying case 2. The walk must
    /// then advance through both `native` and `case_one_marker`, all the way
    /// to `dest_tip`.
    ///
    /// The import marker is deliberately branded "sibling", not "main" (the
    /// branch under test) — case 2 is branch-agnostic
    /// ([`marker::verify_self`]), but branding it "main" would let
    /// [`newest_dest_marker`]'s own branch-scoped scan pick it straight up
    /// as B1, making the boundary trivially equal `dest_tip` without case
    /// 2's logic ever running at all. Branding it "sibling" forces this test
    /// to actually exercise case 2, not B1 absorbing the whole scenario —
    /// confirmed empirically: this test still passes with case 2 disabled
    /// unless the branch names are kept apart like this.
    #[test]
    fn case_two_represents_a_dest_native_commit_that_a_reachable_marker_names() {
        let (_dir, repo, root) = empty_repo();

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip_after_b1 = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );

        let native_tree = tree_with_file(&repo, Some(root), "customer.txt", b"customer");
        let native = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            native_tree,
            "a customer commit, later imported",
        );

        // The import: a DestToSource marker reachable from source_tip that
        // names `native`'s exact oid — case 2's proof. Branded "sibling",
        // not "main" — see the doc comment above.
        let import_tree = tree_with_file(&repo, Some(source_tip_after_b1), "import.txt", b"x");
        let import_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let import_message = dest_marker_message(
            "sibling",
            native,
            &[source_tip_after_b1],
            import_tree,
            &import_signature,
        );
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[source_tip_after_b1],
            import_tree,
            &import_message,
            &import_signature,
        );

        // A case-1-qualifying SourceToDest marker sits above the (now
        // imported) native commit.
        let case_one_tree = tree_with_file(&repo, Some(native), "dest.txt", b"dest");
        let case_one_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let case_one_message = source_marker_message(
            "main",
            source_tip,
            &[native],
            case_one_tree,
            &case_one_signature,
        );
        let case_one_marker = commit_with_signature(
            &repo,
            "refs/heads/dest",
            &[native],
            case_one_tree,
            &case_one_message,
            &case_one_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary =
            dest_to_source_boundary(&repo, source_tip, case_one_marker, "main", &key).unwrap();
        assert_eq!(
            boundary, case_one_marker,
            "an imported dest-native commit, and a case-1 marker above it, must both be \
             represented, advancing the boundary all the way to dest_tip"
        );
    }

    /// decisions/0048's scenario 9: a bare dest-native commit, represented
    /// only via case 2, with nothing above it at all — `dest_tip` itself —
    /// must be a valid boundary in its own right, a shape the pre-0048
    /// B1-or-B2 formulation never named.
    ///
    /// As in [`case_two_represents_a_dest_native_commit_that_a_reachable_marker_names`]
    /// above, the import marker is branded "sibling", not "main": branding
    /// it "main" would let it double as B1 itself, making the boundary
    /// trivially `dest_tip` via the precondition alone rather than via case
    /// 2 — confirmed empirically the same way.
    #[test]
    fn bare_dest_native_commit_is_a_valid_boundary_in_its_own_right() {
        let (_dir, repo, root) = empty_repo();

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip_after_b1 = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );

        let native_tree = tree_with_file(&repo, Some(root), "customer.txt", b"customer");
        let native = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            native_tree,
            "a bare customer commit, imported, nothing above it",
        );

        let import_tree = tree_with_file(&repo, Some(source_tip_after_b1), "import.txt", b"x");
        let import_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let import_message = dest_marker_message(
            "sibling",
            native,
            &[source_tip_after_b1],
            import_tree,
            &import_signature,
        );
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[source_tip_after_b1],
            import_tree,
            &import_message,
            &import_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary = dest_to_source_boundary(&repo, source_tip, native, "main", &key).unwrap();
        assert_eq!(
            boundary, native,
            "an imported dest-native commit must be a valid boundary in its own right when it is \
             dest_tip itself"
        );
    }

    /// decisions/0048's scenario 10: a `SourceToDest` marker whose own
    /// counterpart is real and locally present, but reachable only from a
    /// *different* branch's source tip, not this walk's own `source_tip` —
    /// case 1 fails (not an ancestor of *this* source_tip), and case 2 fails
    /// too (nothing reachable from this `source_tip` names the marker's own
    /// oid). An authenticated, self-verified marker must not be trusted
    /// without branch-specific reachability.
    #[test]
    fn unreachable_cross_branch_marker_is_not_represented() {
        let (_dir, repo, root) = empty_repo();

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );
        let b1 = root;

        // A commit on a different branch's source history — never reachable
        // from `source_tip` above.
        let other_tree = tree_with_file(&repo, Some(root), "other.txt", b"other");
        let other_branch_tip = commit(
            &repo,
            "refs/heads/other",
            &[root],
            other_tree,
            "a commit on a sibling branch's own source history",
        );

        // A real, self-verified SourceToDest marker naming that unreachable
        // commit, sitting directly above B1.
        let d_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let d_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let d_message =
            source_marker_message("main", other_branch_tip, &[root], d_tree, &d_signature);
        let d = commit_with_signature(
            &repo,
            "refs/heads/dest",
            &[root],
            d_tree,
            &d_message,
            &d_signature,
        );

        let key = marker::load_key().unwrap();
        let boundary = dest_to_source_boundary(&repo, source_tip, d, "main", &key).unwrap();
        assert_eq!(
            boundary, b1,
            "a marker naming a commit reachable only from a different branch's source tip must \
             not be trusted as this branch's own boundary"
        );
    }

    /// decisions/0048: B1 reachable from `dest_tip` only via a non-first-parent
    /// (a real merge) must fail closed with a clear, explicit message once
    /// the walk runs off dest's first-parent line without ever reaching it
    /// — not a raw parent-lookup error (decisions/0019's first-parent
    /// limitation). The precondition above uses full-ancestry
    /// `graph_descendant_of` and so still passes; only the first-parent-only
    /// walk itself can never reach B1.
    #[test]
    fn refuses_clearly_when_b1_is_reachable_only_via_a_non_first_parent_merge() {
        let (_dir, repo, root) = empty_repo();

        let b1_tree = tree_with_file(&repo, Some(root), "b1.txt", b"b1");
        let b1 = commit(
            &repo,
            "refs/heads/b1",
            &[root],
            b1_tree,
            "an already-imported commit",
        );

        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", b1, &[root], source_tree, &source_signature);
        let source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );

        // dest's first-parent line never reaches b1: it's merged in only as
        // the merge's second parent.
        let plain_tree = tree_with_file(&repo, Some(root), "plain.txt", b"plain");
        let plain = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            plain_tree,
            "a plain dest commit",
        );
        let merge_tree = tree_with_file(&repo, Some(plain), "merge.txt", b"merge");
        let merge = commit(
            &repo,
            "refs/heads/dest",
            &[plain, b1],
            merge_tree,
            "a merge that carries b1 as its second parent",
        );

        let key = marker::load_key().unwrap();
        let error = dest_to_source_boundary(&repo, source_tip, merge, "main", &key).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("is not on dest's first-parent line"),
            "expected an explicit first-parent-line refusal, got: {message}"
        );
    }

    /// The `MAX_MARKER_SCAN_COMMITS` bail on case 2's *inner* scan of
    /// source's first-parent history (`dest_to_source_marker_targets`),
    /// distinct from [`bails_past_the_injected_scan_limit_before_reaching_b1`]
    /// above, which exercises the outer dest-side walk's own limit: a
    /// dest-native commit above B1 has no marker of its own, so it forces
    /// the walk to build case 2's target set, and source's own first-parent
    /// line (12 commits deep) exceeds the (injected, small) scan limit
    /// before that set can be finished — the walk must fail closed on that
    /// scan too, not silently treat the candidate as unrepresented on an
    /// incomplete answer.
    #[test]
    fn case_two_inner_scan_bails_past_the_injected_scan_limit() {
        let (_dir, repo, root) = empty_repo();

        // B1: source's own history names `root` itself as the boundary.
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_message =
            dest_marker_message("main", root, &[root], source_tree, &source_signature);
        let mut source_tip = commit_with_signature(
            &repo,
            "refs/heads/source",
            &[root],
            source_tree,
            &source_message,
            &source_signature,
        );
        let b1 = root;

        // Extend source's first-parent line with plain commits, unrelated
        // to case 2, until the whole line (root, the B1 marker, and these)
        // is 12 commits deep — deep enough that an injected limit of 5
        // exceeds it well before source's history is exhausted.
        for index in 0..10 {
            let tree = tree_with_file(&repo, Some(source_tip), &format!("s{index}.txt"), b"x");
            source_tip = commit(
                &repo,
                "refs/heads/source",
                &[source_tip],
                tree,
                "a plain source commit",
            );
        }

        // A dest-native commit (no marker at all) sits directly above B1 —
        // case 1 fails immediately, forcing the walk to build case 2's
        // target set, which is what actually runs into the injected limit.
        let native_tree = tree_with_file(&repo, Some(root), "customer.txt", b"customer");
        let native = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            native_tree,
            "a customer commit needing case 2",
        );

        let key = marker::load_key().unwrap();
        let error = dest_to_source_boundary_bounded(&repo, source_tip, native, "main", &key, 5)
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("case-2 source scan exceeds the 5 commit limit"),
            "expected case 2's own scan-limit bail, got: {error:#}"
        );
        // Control: the identical walk succeeds once the limit covers all of
        // source's history, so the bail above is really about the limit,
        // not some other mistake in the fixture.
        let boundary =
            dest_to_source_boundary_bounded(&repo, source_tip, native, "main", &key, 12).unwrap();
        assert_eq!(
            boundary, b1,
            "with the limit lifted, case 2 should complete its scan and correctly find no \
             matching marker for `native`, leaving B1 as the boundary"
        );
    }
}
