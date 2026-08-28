//! Preflight plus compare-and-swap advancement of dest→source's own local
//! source branch, once its push to source's remote has been accepted — see
//! [`preflight_local_source_branch`] and [`advance_local_source_branch`]'s
//! own doc comments for the two safety properties a blind, forced ref write
//! would lose.

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::git;
use crate::limits;

/// Keeps this checkout's own local `branch` in step with what dest→source
/// just pushed to source's remote — nothing else updates it (a plain `git
/// push` never moves the pusher's own branch either), but later code in this
/// same run (source→dest for this pair, right after) reads this branch
/// locally and needs to see it.
///
/// Two safety properties a blind, forced ref write would lose:
///
/// * Verifies `new_tip` is actually a fast-forward of `branch`'s *current*
///   local value before moving anything. A retry rebuilds `new_tip` against
///   a freshly-fetched *remote* tip (decisions/0009's refetch-and-recompute
///   principle, applied here too), not necessarily this checkout's own local
///   view — so if the local branch has diverged in the meantime (e.g. a
///   concurrent local commit), forcing it forward would silently discard
///   that divergent work rather than surface it.
/// * If `branch` is the one currently checked out, updates the working
///   tree/index together via a safe (non-forced) checkout instead of moving
///   only the ref — otherwise the checkout is left looking dirty (e.g. a
///   newly cherry-picked file shows as staged-for-deletion) relative to a
///   ref that just silently moved out from under it, and a genuine local
///   modification gets silently discarded instead of surfaced as a conflict.
fn local_source_branch_tip(repo: &Repository, branch: &str) -> Result<Oid> {
    let refname = format!("refs/heads/{branch}");
    repo.find_reference(&refname)
        .with_context(|| format!("resolving local source branch {branch:?}"))?
        .peel_to_commit()
        .with_context(|| format!("resolving local source branch {branch:?} to a commit"))
        .map(|commit| commit.id())
}

fn head_points_to_branch(repo: &Repository, refname: &str) -> Result<bool> {
    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => return Ok(false),
    };
    let name_bytes = head.name_bytes();
    if name_bytes.is_empty() {
        return Ok(false);
    }
    let name = std::str::from_utf8(name_bytes).with_context(|| {
        format!(
            "symbolic HEAD name {} is not valid UTF-8",
            git::escape_bytes(name_bytes)
        )
    })?;
    Ok(name == refname)
}

/// Performs all checks that can be made without changing the local checkout.
/// The returned OID is the exact value later used by the compare-and-swap
/// update, so a concurrent ref move cannot be mistaken for the state checked
/// here.
pub(super) fn preflight_local_source_branch(
    repo: &Repository,
    branch: &str,
    new_tip: Oid,
) -> Result<Oid> {
    let previous_tip = local_source_branch_tip(repo, branch)?;

    if previous_tip != new_tip
        && !repo.graph_descendant_of(new_tip, previous_tip).with_context(|| {
            format!(
                "checking whether {new_tip} is a fast-forward of local branch {branch:?}'s current tip {previous_tip}"
            )
        })?
    {
        anyhow::bail!(
            "gitprism sync: local branch {branch:?} (currently {previous_tip}) has diverged from what dest→source would push to source's remote ({new_tip}) — refusing to force it forward and silently discard that local work; reconcile it manually before syncing again"
        );
    }

    let refname = format!("refs/heads/{branch}");
    let head_points_here = head_points_to_branch(repo, &refname)?;

    if head_points_here {
        // A symbolic HEAD can temporarily name a branch whose ref was moved
        // by another local Git operation; in that case Git's status is still
        // relative to the old HEAD commit and reports the expected ref move
        // as staged changes. Only reject tracked dirt when HEAD is the exact
        // branch tip we just observed.
        let head_tip = repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok())
            .map(|commit| commit.id());
        if head_tip == Some(previous_tip) {
            let mut status_options = git2::StatusOptions::new();
            status_options
                .include_untracked(false)
                .include_ignored(false);
            let statuses = repo.statuses(Some(&mut status_options))?;
            if !statuses.is_empty() {
                anyhow::bail!(
                    "gitprism sync: checked-out source branch {branch:?} has local working-tree or index changes; refusing to push before a safe local advancement"
                );
            }
        }
        reject_colliding_untracked_paths(repo, branch, new_tip)?;
        let new_commit = repo
            .find_commit(new_tip)
            .context("resolving the newly pushed local commit")?;
        // Dry-run the same safe checkout used after the push. A dirty or
        // conflicting checkout must stop before the remote is changed.
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.dry_run();
        repo.checkout_tree(new_commit.as_object(), Some(&mut checkout))
            .context("preflighting the checkout of what dest→source would push")?;
    }

    Ok(previous_tip)
}

/// A safe libgit2 checkout does not report every untracked-file collision in
/// dry-run mode. Compare the current HEAD tree with the target and reject an
/// untracked or ignored path only when the target would write beneath it;
/// unrelated local files remain untouched and are allowed.
fn reject_colliding_untracked_paths(repo: &Repository, branch: &str, new_tip: Oid) -> Result<()> {
    let head_tree = repo
        .head()
        .context("resolving HEAD while checking untracked checkout collisions")?
        .peel_to_tree()
        .context("resolving HEAD tree while checking untracked checkout collisions")?;
    let target_tree = repo
        .find_commit(new_tip)
        .context("resolving target tree while checking untracked checkout collisions")?
        .tree()
        .context("reading target tree while checking untracked checkout collisions")?;
    let diff = repo
        .diff_tree_to_tree(Some(&head_tree), Some(&target_tree), None)
        .context("comparing local and target trees while checking checkout collisions")?;
    let mut budget = limits::TraversalBudget::default();
    let mut target_paths = Vec::new();
    for delta in diff.deltas() {
        budget.visit("checkout collision scanning")?;
        if let Some(path) = delta.new_file().path_bytes() {
            if target_paths.len() >= limits::MAX_COLLISION_PATHS {
                anyhow::bail!(
                    "checkout collision scanning exceeds the {} path limit",
                    limits::MAX_COLLISION_PATHS
                );
            }
            target_paths.push(path);
        }
    }
    if target_paths.is_empty() {
        return Ok(());
    }

    let mut status_options = git2::StatusOptions::new();
    status_options
        .include_untracked(true)
        .include_ignored(true)
        .recurse_untracked_dirs(true)
        .recurse_ignored_dirs(true);
    let statuses = repo
        .statuses(Some(&mut status_options))
        .context("checking untracked checkout collisions")?;
    let mut status_paths = 0;
    for entry in statuses.iter() {
        budget.visit("checkout collision scanning")?;
        status_paths += 1;
        if status_paths > limits::MAX_COLLISION_PATHS {
            anyhow::bail!(
                "checkout collision scanning exceeds the {} path limit",
                limits::MAX_COLLISION_PATHS
            );
        }
        if !(entry.status().is_wt_new() || entry.status().is_ignored()) {
            continue;
        }
        let path = entry.path_bytes();
        if target_paths
            .iter()
            .any(|target| git_paths_conflict(target, path))
        {
            anyhow::bail!(
                "gitprism sync: checked-out source branch {branch:?} has an untracked or ignored path that the pushed tree would overwrite; refusing to push before a safe local advancement"
            );
        }
    }
    Ok(())
}

pub(super) fn git_paths_conflict(left: &[u8], right: &[u8]) -> bool {
    left == right || path_component_prefix(left, right) || path_component_prefix(right, left)
}

fn path_component_prefix(prefix: &[u8], path: &[u8]) -> bool {
    path.len() > prefix.len() && path.starts_with(prefix) && path[prefix.len()] == b'/'
}

/// Materializes the pushed commit and advances the local ref with a
/// compare-and-swap against the exact OID returned by preflight. The checkout
/// intentionally remains before the ref update: libgit2 needs the current
/// HEAD/tree relationship to materialize the new tree safely. If the CAS
/// loses to an external Git process, no force/reset is attempted.
pub(super) fn advance_local_source_branch(
    repo: &Repository,
    branch: &str,
    new_tip: Oid,
    expected_tip: Oid,
) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let current_tip = local_source_branch_tip(repo, branch)?;
    if current_tip != expected_tip {
        anyhow::bail!(
            "local source branch {branch:?} moved from expected {expected_tip} to {current_tip} after the remote push; refusing to overwrite it"
        );
    }

    let head_points_here = head_points_to_branch(repo, &refname)?;

    if head_points_here {
        let new_commit = repo
            .find_commit(new_tip)
            .context("resolving the newly pushed local commit")?;
        // Deliberately not forced (`None` options default to a safe checkout,
        // same as `setup`'s own `checkout_head(None)`) — a real local
        // modification must surface as a conflict, not be silently
        // overwritten just because dest→source advanced the branch.
        repo.checkout_tree(new_commit.as_object(), None)
            .context("checking out what dest→source just pushed into the working tree")?;
        crate::policy::restore_control_files_exact(repo, &new_commit.tree()?)?;
    }

    repo.reference_matching(
        &refname,
        new_tip,
        true,
        expected_tip,
        "gitprism sync: dest -> source",
    )
    .map_err(|error| {
        if error.code() == git2::ErrorCode::Modified {
            anyhow::anyhow!(
                "local source branch {branch:?} moved while the pushed tree was being checked out; refusing to overwrite the concurrent ref"
            )
        } else {
            anyhow::Error::new(error)
        }
    })
    .with_context(|| format!("advancing local branch {branch:?} after the remote push"))?;

    Ok(())
}
