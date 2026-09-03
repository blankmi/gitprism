//! The first-parent marker revwalks shared by [`super`]'s main loop and
//! [`super::anchor`]'s mapping-index reconstruction and lookup — see
//! decisions/0019 for why every scan here is restricted to first-parent
//! history.

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

/// [`scan_for_dest_marker`], bailing when nothing is found — used only by
/// [`super::anchor::dest_tip_accounted_for`] and [`super::pending_dest_commits`],
/// whose branch always comes from `config.branches`, where `setup`
/// (decisions/0006, decisions/0023) guarantees the trailer exists. Finding
/// none really does mean "has `gitprism setup` been run for this pair?"
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

// TODO(plan `docs/plans/2026-09-02/CODE-001-dest-to-source-boundary.md`,
// step 4): once `dest_to_source_boundary` exists here, add unit tests
// covering the `MAX_MARKER_SCAN_COMMITS` bail on the dest-side walk and
// scenario 6 (B1 missing from the local odb; dest force-rewound behind an
// already-imported B1 whose older tip would otherwise satisfy B2).
