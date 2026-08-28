//! Tree filtering (decisions/0004, 0011) — see `filter_tree`'s own doc
//! comment for why filtering is gitprism's own job, not git's.

use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::exclude::ExcludeList;
use crate::git;
use crate::limits;

/// Builds a filtered copy of `tree` with every excluded path removed, in
/// `repo`'s object database. Filtering stays gitprism's own job, not git's
/// (decisions/0004, 0011) — it has to happen *before* a pending commit ever
/// reaches [`git::merge_tree`], not after, since an excluded path's own
/// history (e.g. `.gitprismignore` being edited repeatedly) would otherwise
/// present as a modify/delete conflict against a dest that never had that
/// path at all (decisions/0014, 0016).
///
/// Excluded entries are skipped without recursing into them — pruning a
/// `secrets/` tree of 10k files costs one `is_excluded` call, not 10k.
/// Surviving subtrees are rebuilt recursively and omitted entirely if
/// filtering leaves them empty, since git doesn't track empty directories.
/// Non-tree entries are re-inserted with their original `filemode()`
/// preserved — what keeps executable bits, symlinks, and gitlinks intact,
/// and stops a submodule being recursed into as if it were an ordinary tree.
pub(crate) fn filter_tree(
    repo: &Repository,
    tree: &git2::Tree,
    prefix: &Path,
    exclude_list: &ExcludeList,
) -> Result<Oid> {
    let mut budget = limits::TraversalBudget::default();
    filter_tree_with_budget(repo, tree, prefix, exclude_list, &mut budget, 0)
}

fn filter_tree_with_budget(
    repo: &Repository,
    tree: &git2::Tree,
    prefix: &Path,
    exclude_list: &ExcludeList,
    budget: &mut limits::TraversalBudget,
    depth: usize,
) -> Result<Oid> {
    limits::TraversalBudget::check_depth(depth, "Git tree filtering")?;
    let mut builder = repo
        .treebuilder(None)
        .context("starting a filtered tree builder")?;

    for entry in tree.iter() {
        budget.visit("Git tree filtering")?;
        let name_bytes = entry.name_bytes();
        let name = std::str::from_utf8(name_bytes).with_context(|| {
            format!(
                "a tree entry has a non-UTF-8 name gitprism can't filter by: {}",
                git::escape_bytes(name_bytes)
            )
        })?;
        let rel_path = prefix.join(name);
        let is_tree = entry.kind() == Some(git2::ObjectType::Tree);
        // A submodule gitlink counts as a directory for ignore-pattern
        // matching, matching `git check-ignore`'s own DT_DIR treatment of
        // gitlinks, even though it is never recursed into like a tree
        // (decisions/0011 addendum).
        let is_gitlink = entry.filemode() == i32::from(git2::FileMode::Commit);
        let is_dir = is_tree || is_gitlink;

        if exclude_list.is_excluded(&rel_path, is_dir) {
            continue;
        }

        if is_tree {
            let subtree = repo
                .find_tree(entry.id())
                .with_context(|| format!("reading subtree {}", rel_path.display()))?;
            let filtered_oid = filter_tree_with_budget(
                repo,
                &subtree,
                &rel_path,
                exclude_list,
                budget,
                depth + 1,
            )?;
            // A directory that excluding left with nothing in it must not
            // appear at all — git doesn't track directories independently
            // of their contents.
            if repo
                .find_tree(filtered_oid)
                .context("reading a just-filtered subtree back")?
                .iter()
                .next()
                .is_none()
            {
                continue;
            }
            builder
                .insert(name, filtered_oid, git2::FileMode::Tree.into())
                .with_context(|| format!("inserting filtered subtree {}", rel_path.display()))?;
        } else {
            builder
                .insert(name, entry.id(), entry.filemode())
                .with_context(|| format!("inserting {}", rel_path.display()))?;
        }
    }

    builder.write().context("writing a filtered tree")
}

/// The empty tree, written into `repo`'s odb so a `git` subprocess can name
/// it as `--merge-base` for a commit with no parent. Defensive only: every
/// commit `pending_commits` yields strictly descends from `boundary`, so it
/// always has a parent.
pub(super) fn empty_tree(repo: &Repository) -> Result<Oid> {
    repo.treebuilder(None)
        .context("starting an empty tree builder")?
        .write()
        .context("writing the empty tree")
}
