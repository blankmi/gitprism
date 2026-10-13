//! decisions/0052 Part 1: a branch whose pending source commits another
//! branch has already projected reuses that projection, but only when a
//! bounded, authenticated lineage proof shows the reuse is exact.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use git2::{Commit, Oid, Repository};

use crate::limits;
use crate::marker::{self, Direction as MarkerDirection};

use super::loop_prevented;
use super::mapping_index::{MappingIndex, MappingLookup};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReusePlan {
    /// No observed shared mapping: today's replay.
    Replay,
    /// An observed shared mapping that can't be resolved or proven.
    Halt(String),
    /// `dest_tip` is the shared base: build on `projection` directly.
    FastForward { shared_source: Oid, projection: Oid },
    /// Build a bridge with parents `(dest_tip, projection)`.
    Bridge { shared_source: Oid, projection: Oid },
}

/// Step 6: one aggregate allowance for every commit Steps 3–5 load. A
/// commit already loaded during this proof is reused, not counted again.
struct Budget<'repo> {
    repo: &'repo Repository,
    remaining: usize,
    loaded: HashMap<Oid, Commit<'repo>>,
}

/// The budget ran out before the proof finished.
struct Horizon;

impl<'repo> Budget<'repo> {
    fn load(&mut self, oid: Oid) -> Result<std::result::Result<Commit<'repo>, Horizon>> {
        if let Some(commit) = self.loaded.get(&oid) {
            return Ok(Ok(commit.clone()));
        }
        if self.remaining == 0 {
            return Ok(Err(Horizon));
        }
        self.remaining -= 1;
        let commit = self
            .repo
            .find_commit(oid)
            .with_context(|| format!("resolving {oid} while proving a projection reuse"))?;
        self.loaded.insert(oid, commit.clone());
        Ok(Ok(commit))
    }

    fn parent(
        &mut self,
        commit: &Commit<'repo>,
        index: usize,
    ) -> Result<std::result::Result<Commit<'repo>, Horizon>> {
        self.load(commit.parent_id(index)?)
    }
}

/// Unwraps a budgeted load, or returns the horizon halt from the caller.
macro_rules! within_budget {
    ($load:expr, $halt:expr) => {
        match $load? {
            Ok(commit) => commit,
            Err(Horizon) => return $halt,
        }
    };
}

/// `pending` is `pending_commits(boundary, source_tip)`, oldest first.
pub(super) fn plan_projection_reuse(
    repo: &Repository,
    index: &MappingIndex,
    pending: &[Oid],
    boundary: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<ReusePlan> {
    plan_projection_reuse_bounded(
        repo,
        index,
        pending,
        boundary,
        dest_tip,
        branch,
        key,
        limits::MAX_MARKER_SCAN_COMMITS,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_projection_reuse_bounded(
    repo: &Repository,
    index: &MappingIndex,
    pending: &[Oid],
    boundary: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
    scan_limit: usize,
) -> Result<ReusePlan> {
    // Step 1. Only commits replay would project can trigger reuse; `pending`
    // itself stays intact for Steps 2 and 5.
    let mut shared = None;
    for (position, source) in pending.iter().enumerate().rev() {
        let commit = repo
            .find_commit(*source)
            .context("resolving a pending source commit")?;
        if loop_prevented(&commit, key) {
            continue;
        }
        match classify_shared_mapping(repo, index, *source, dest_tip)? {
            SharedMapping::None | SharedMapping::Contained => {}
            SharedMapping::Contradictory(diagnostic) => {
                return Ok(ReusePlan::Halt(diagnostic));
            }
            SharedMapping::NotContained(projection) => {
                shared = Some((position, *source, projection));
                break;
            }
        }
    }
    let Some((k, shared_source, projection)) = shared else {
        return Ok(ReusePlan::Replay);
    };

    let halt = |reason: String| {
        Ok(ReusePlan::Halt(format!(
            "source commit {shared_source} is already projected as dest commit {projection}, \
             but reusing that projection for {branch:?} can't be proven safe ({reason}); \
             replaying would publish a second, incomparable copy of it"
        )))
    };
    let horizon = || {
        halt(format!(
            "the proof exceeds the {scan_limit} commit scan limit"
        ))
    };

    // Step 2.
    let first = repo
        .find_commit(pending[0])
        .context("resolving the oldest pending source commit")?;
    if first.parent_count() == 0 || first.parent_id(0)? != boundary {
        return halt(format!(
            "its resume boundary {boundary} isn't on source's first-parent line below that commit"
        ));
    }

    let mut budget = Budget {
        repo,
        remaining: scan_limit,
        loaded: HashMap::new(),
    };

    // Step 3.
    let Some(accounting) = accounting_set(&mut budget, dest_tip, branch, key)? else {
        return horizon();
    };

    // Step 4.
    let mut suffix = Vec::new();
    let mut current = projection;
    let shared_base = loop {
        if accounting.contains(&current) {
            break current;
        }
        let commit = within_budget!(budget.load(current), horizon());
        suffix.push(commit.clone());
        if commit.parent_count() == 0 {
            return halt(format!(
                "{branch:?}'s dest history isn't only gitprism accounting on top of a commit \
                 {projection} builds on"
            ));
        }
        current = commit.parent_id(0)?;
    };
    let base_tree = within_budget!(budget.load(shared_base), horizon()).tree_id();
    let tip_tree = within_budget!(budget.load(dest_tip), horizon()).tree_id();
    if base_tree != tip_tree {
        return halt(format!(
            "{branch:?}'s dest tip {dest_tip} has content beyond the shared base {shared_base}"
        ));
    }

    // Step 5. Position 0 is `boundary`; `pending[i]` is position `i + 1`.
    let in_range = &pending[..=k];
    let positions: HashMap<Oid, usize> = std::iter::once((boundary, 0))
        .chain(in_range.iter().enumerate().map(|(i, oid)| (*oid, i + 1)))
        .collect();
    let mut imported: HashMap<Oid, usize> = HashMap::new();
    for (i, oid) in in_range.iter().enumerate() {
        let commit = within_budget!(budget.load(*oid), horizon());
        if let Some(parsed) =
            marker::verify_self(&commit, &[MarkerDirection::DestToSource], None, key)
        {
            imported.insert(parsed.counterpart, i + 1);
        }
    }

    let mut cursor = 0usize;
    for commit in suffix.iter().rev() {
        let oid = commit.id();
        if commit.parent_count() != 1 {
            return halt(format!("{oid} has more than one parent"));
        }
        if let Some(parsed) =
            marker::verify_self(commit, &[MarkerDirection::SourceToDest], None, key)
        {
            let Some(&position) = positions.get(&parsed.counterpart) else {
                return halt(format!(
                    "{oid} projects {}, outside {branch:?}'s pending range",
                    parsed.counterpart
                ));
            };
            let parent_tree = within_budget!(budget.parent(commit, 0), horizon()).tree_id();
            let ordered = if commit.tree_id() == parent_tree {
                position >= cursor
            } else {
                position > cursor
            };
            if !ordered {
                return halt(format!(
                    "{oid} projects {} out of first-parent order",
                    parsed.counterpart
                ));
            }
            cursor = position;
        } else if let Some(&position) = imported.get(&oid) {
            // decisions/0048 case 2, restricted to the pending range.
            if position <= cursor {
                return halt(format!("{oid} is imported out of first-parent order"));
            }
            cursor = position;
        } else {
            return halt(format!(
                "{oid} is dest content not represented in {branch:?}'s pending range"
            ));
        }
    }

    Ok(if shared_base == dest_tip {
        ReusePlan::FastForward {
            shared_source,
            projection,
        }
    } else {
        ReusePlan::Bridge {
            shared_source,
            projection,
        }
    })
}

/// How one source commit's observed mappings relate to a dest commit
/// `base` (Step 1, and `resolve`'s Step 9 checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedMapping {
    /// No observed entry.
    None,
    /// Resolved to `base` or an ancestor of it.
    Contained,
    /// 0046 resolution refused, with its diagnostic.
    Contradictory(String),
    /// Resolved to a dest commit `base` doesn't contain.
    NotContained(Oid),
}

pub(crate) fn classify_shared_mapping(
    repo: &Repository,
    index: &MappingIndex,
    source: Oid,
    base: Oid,
) -> Result<SharedMapping> {
    if !index.has_entry(source) {
        return Ok(SharedMapping::None);
    }
    Ok(match index.resolve_exact(repo, source)? {
        MappingLookup::None => SharedMapping::None,
        MappingLookup::Contradictory(diagnostic) => SharedMapping::Contradictory(diagnostic),
        MappingLookup::Resolved(mapping) => {
            let dest = mapping.dest;
            let contained = dest == base
                || repo
                    .graph_descendant_of(base, dest)
                    .with_context(|| format!("checking whether {base} already contains {dest}"))?;
            if contained {
                SharedMapping::Contained
            } else {
                SharedMapping::NotContained(dest)
            }
        }
    })
}

/// Step 3: `dest_tip`, every alias and trusted bridge on its first-parent
/// line, each bridge's adopted parent, and the first other commit. `None`
/// at the budget horizon.
///
/// Writer contract: an authenticated two-parent `SourceToDest` commit is
/// written only by `build_bridge_commit` after this module's proof passed,
/// so a bridge's shape and HMAC stand in for re-running its earlier proof.
fn accounting_set(
    budget: &mut Budget<'_>,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<HashSet<Oid>>> {
    let mut set = HashSet::new();
    let mut current = dest_tip;
    loop {
        set.insert(current);
        let Ok(commit) = budget.load(current)? else {
            return Ok(None);
        };
        let tree_parent = match commit.parent_count() {
            1 => 0,
            2 => 1,
            _ => return Ok(Some(set)),
        };
        if marker::verify(&commit, branch, &[MarkerDirection::SourceToDest], None, key).is_none() {
            return Ok(Some(set));
        }
        let Ok(parent) = budget.parent(&commit, tree_parent)? else {
            return Ok(None);
        };
        if parent.tree_id() != commit.tree_id() {
            return Ok(Some(set));
        }
        if tree_parent == 1 {
            set.insert(parent.id());
        }
        current = commit.parent_id(0)?;
    }
}
