use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::marker::{self, Direction as MarkerDirection, StateKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MappingProvenance {
    pub(crate) branch: String,
    pub(crate) marker: Oid,
    pub(crate) raw_dest: Oid,
    pub(crate) canonical_dest: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedMapping {
    pub(crate) source: Oid,
    pub(crate) dest: Oid,
    pub(crate) provenance: Vec<MappingProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MappingLookup {
    None,
    Resolved(ResolvedMapping),
    Contradictory(String),
}

/// decisions/0046 addendum: reconstruction's per-scan bound
/// (`MAX_MARKER_SCAN_COMMITS`) and the aggregate mapping-entry bound
/// (`MAX_MAPPING_ENTRIES`) are both now a *horizon*, not a whole-run
/// failure — hitting either during one head's scan keeps everything found
/// so far, stops that one walk, and is recorded here so a later lookup
/// that would otherwise silently use an incomplete mapping set can instead
/// refuse. Never populated by anything but [`MappingIndex::reconstruct`]'s
/// own scans; a mapping this run's own push adds is already fully known
/// and bounded elsewhere (pending-history limits), so it can never
/// truncate.
#[derive(Debug)]
pub(crate) struct MappingIndex {
    entries: HashMap<Oid, Vec<MappingProvenance>>,
    entry_count: usize,
    limit: usize,
    scan_limit: usize,
    truncated_scans: Vec<String>,
    scanned_commits: usize,
}

impl Default for MappingIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl MappingIndex {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            entry_count: 0,
            limit: crate::limits::MAX_MAPPING_ENTRIES,
            scan_limit: crate::limits::MAX_MARKER_SCAN_COMMITS,
            truncated_scans: Vec::new(),
            scanned_commits: 0,
        }
    }

    #[cfg(test)]
    fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::new()
        }
    }

    #[cfg(test)]
    fn with_scan_limit(scan_limit: usize) -> Self {
        Self {
            scan_limit,
            ..Self::new()
        }
    }

    pub(crate) fn reconstruct(
        repo: &Repository,
        source_heads: &[(String, Oid)],
        dest_heads: &[(String, Oid)],
        key: &StateKey,
    ) -> Result<Self> {
        let mut index = Self::new();
        index.populate(repo, source_heads, dest_heads, key)?;
        Ok(index)
    }

    /// Test-only entry point for exercising truncation without needing a
    /// repository with `MAX_MARKER_SCAN_COMMITS` real commits in it.
    #[cfg(test)]
    pub(crate) fn reconstruct_with_scan_limit(
        repo: &Repository,
        source_heads: &[(String, Oid)],
        dest_heads: &[(String, Oid)],
        key: &StateKey,
        scan_limit: usize,
    ) -> Result<Self> {
        let mut index = Self::with_scan_limit(scan_limit);
        index.populate(repo, source_heads, dest_heads, key)?;
        Ok(index)
    }

    /// decisions/0046 addendum, Finding A: `source_heads` and `dest_heads`
    /// routinely share long stretches of first-parent history (every
    /// discovered branch descends from the same graft, round-tripped
    /// branches share a common trunk, ...). A run-wide visited set per side
    /// means a commit already scanned by an earlier head's walk is never
    /// re-loaded or re-parsed by a later one — reconstruction work becomes
    /// O(unique first-parent commits), not O(heads × history). Kept
    /// separate per side (rather than one combined set) because a source
    /// commit and a dest commit can legitimately share the same OID (the
    /// `setup` graft is real shared ancestry) while requiring different
    /// marker directions to be checked against it.
    fn populate(
        &mut self,
        repo: &Repository,
        source_heads: &[(String, Oid)],
        dest_heads: &[(String, Oid)],
        key: &StateKey,
    ) -> Result<()> {
        let mut visited_source = HashSet::new();
        for (branch, head) in source_heads {
            self.scan_source_history(repo, branch, *head, key, &mut visited_source)?;
        }
        let mut visited_dest = HashSet::new();
        for (branch, head) in dest_heads {
            self.scan_dest_history(repo, branch, *head, key, &mut visited_dest)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn resolve(
        &self,
        repo: &Repository,
        source: Oid,
    ) -> Result<Option<ResolvedMapping>> {
        match self.resolve_for_anchor(repo, source, None)? {
            MappingLookup::None => Ok(None),
            MappingLookup::Resolved(mapping) => Ok(Some(mapping)),
            MappingLookup::Contradictory(diagnostic) => anyhow::bail!(diagnostic),
        }
    }

    /// Whether any reconstruction scan stopped early against a horizon
    /// (`MAX_MARKER_SCAN_COMMITS` or `MAX_MAPPING_ENTRIES`) rather than
    /// running to the true root of every scanned head.
    fn is_truncated(&self) -> bool {
        !self.truncated_scans.is_empty()
    }

    /// Names every truncated scan for an operator-facing refusal. A
    /// truncated index cannot safely answer either "no mapping" or select
    /// one visible mapping (decisions/0046 addendum).
    fn truncation_message(&self) -> String {
        format!(
            "the mapping index could not be fully reconstructed this run, so this mapping lookup \
             cannot be trusted: {}",
            self.truncated_scans.join("; ")
        )
    }

    #[cfg(test)]
    pub(crate) fn scanned_commit_count(&self) -> usize {
        self.scanned_commits
    }

    /// Taints every exact lookup for an incompleteness observed *outside*
    /// this reconstruction's own history scans — decisions/0046 Addendum 2,
    /// Finding F, currently a dest branch listing capped by
    /// `MAX_SOURCE_BRANCHES` — the same way an over-horizon
    /// `scan_source_history`/`scan_dest_history` scan already taints them
    /// via `truncated_scans`: an unlisted dest branch may carry an
    /// incomparable mapping for a source commit this run did observe.
    pub(crate) fn note_incomplete_reconstruction(&mut self, description: String) {
        self.truncated_scans.push(description);
    }

    fn resolve_for_anchor(
        &self,
        repo: &Repository,
        source: Oid,
        exclude_branch: Option<&str>,
    ) -> Result<MappingLookup> {
        let Some(records) = self.entries.get(&source) else {
            return Ok(MappingLookup::None);
        };

        // A horizon means the set of mappings for this source commit is not
        // known to be complete.  Even an already-indexed mapping cannot be
        // selected safely: a later, unscanned marker may be incomparable and
        // choosing the first one would hide that contradiction.  Refuse this
        // lookup per branch. The refusal repeats while history remains beyond
        // the static horizon; decisions/0046 records why avoiding that requires
        // a different durable-state or work-bound decision.
        if self.is_truncated() {
            return Ok(MappingLookup::Contradictory(self.truncation_message()));
        }

        let mut by_dest: HashMap<Oid, Vec<MappingProvenance>> = HashMap::new();
        for record in records {
            by_dest
                .entry(record.canonical_dest)
                .or_default()
                .push(record.clone());
        }

        // decisions/0046 Addendum 3, Finding S: a destination whose only
        // provenance is the branch currently being resolved is preferred
        // against in favor of a comparable/incomparable alternative some
        // OTHER branch already projected for the identical source commit —
        // that alternative is the one the rewrite should build on, since
        // `exclude_branch`'s own chain is exactly what the rewrite
        // discards. But if every mapped destination for this source commit
        // is `exclude_branch`'s own, there is no alternative to prefer:
        // it's this branch's own untouched ancestor content (e.g. an
        // earlier, still-valid part of its own chain an amend didn't
        // touch), not something being discarded, and stays a normal, usable
        // mapping.
        if let Some(exclude_branch) = exclude_branch {
            let any_other_provenance = by_dest
                .values()
                .any(|records| records.iter().any(|record| record.branch != exclude_branch));
            if any_other_provenance {
                by_dest.retain(|_, records| {
                    records.iter().any(|record| record.branch != exclude_branch)
                });
            }
        }

        let mut destinations: Vec<Oid> = by_dest.keys().copied().collect();
        destinations.sort_by_key(ToString::to_string);

        // Finding R: a mapped dest commit this clone never fetched (its
        // dest ref deleted after merge, or discarded by a force rewind)
        // can't be built on, and `graph_descendant_of` has no clean "not
        // found" of its own to compare it against another destination —
        // checked here, before any ancestry comparison touches it, the same
        // way `dest_resume_point_for_branch` already guards this exact
        // hazard.
        let (present, missing): (Vec<Oid>, Vec<Oid>) = destinations
            .iter()
            .copied()
            .partition(|dest| repo.find_commit(*dest).is_ok());
        if present.is_empty() {
            // An exact mapping exists, but its canonical destination is not
            // in this clone.  Walking past it is unsafe: a DestToSource
            // marker may have imported content that replay will deliberately
            // loop-prevent, so an older anchor can silently lose that
            // content.  Halt this branch instead of treating the mapping as
            // if it did not exist.
            return Ok(MappingLookup::Contradictory(format!(
                "source commit {source} maps only to dest commit(s) not present in this clone ({}) — fetch the missing commit or resync from a clone that has it",
                missing
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            )));
        }
        if !missing.is_empty() {
            // Whether a present destination truly dominates one this clone
            // can't inspect isn't decidable safely — a per-branch refusal,
            // never a silent choice among only the mappings that happen to
            // be fetched, and never a raw lookup error that would abort the
            // whole run.
            return Ok(MappingLookup::Contradictory(format!(
                "source commit {source} maps to a dest commit not present in this clone \
                 ({}) alongside {} present canonical mapping(s) — fetch the missing commit \
                 or resync from a clone that has it",
                missing
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                present.len(),
            )));
        }
        let destinations = present;

        let mut common = None;
        for candidate in &destinations {
            let mut is_common_ancestor = true;
            for other in &destinations {
                if *other != *candidate
                    && !repo
                        .graph_descendant_of(*other, *candidate)
                        .with_context(|| {
                            format!(
                                "comparing mapped dest commits {other} and {candidate} for source {source}"
                            )
                        })?
                {
                    is_common_ancestor = false;
                    break;
                }
            }
            if is_common_ancestor {
                common = Some(*candidate);
                break;
            }
        }

        let mut provenance = records.clone();
        provenance.sort_by(|left, right| {
            left.branch
                .cmp(&right.branch)
                .then_with(|| left.marker.to_string().cmp(&right.marker.to_string()))
        });
        let Some(dest) = common else {
            let details = destinations
                .iter()
                .map(|destination| {
                    let branches = by_dest
                        .get(destination)
                        .into_iter()
                        .flatten()
                        .map(|record| format!("{:?} (marker {})", record.branch, record.marker))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{destination} from branches {branches}")
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(MappingLookup::Contradictory(format!(
                "source commit {source} has contradictory dest mappings: {details}"
            )));
        };

        Ok(MappingLookup::Resolved(ResolvedMapping {
            source,
            dest,
            provenance,
        }))
    }

    /// `exclude_branch` (decisions/0046 Addendum 3, Finding S) is the
    /// branch whose own rewrite this anchor is being resolved for: mappings
    /// whose only provenance is that branch itself are excluded from
    /// canonicalization, since that branch's own chain is exactly what the
    /// rewrite discards. `None` for every other caller (scheduling's own
    /// distance metric, which only affects processing order, never an
    /// actual anchor).
    pub(crate) fn nearest_first_parent_mapping(
        &self,
        repo: &Repository,
        tip: Oid,
        exclude_branch: Option<&str>,
    ) -> Result<MappingLookup> {
        let Some((_, lookup)) =
            self.nearest_first_parent_mapping_with_distance(repo, tip, exclude_branch)?
        else {
            return Ok(MappingLookup::None);
        };
        Ok(lookup)
    }

    /// decisions/0046 addendum, Finding A: hitting this walk's own scan
    /// horizon, or consulting an index whose reconstruction was truncated,
    /// must not silently answer either "no mapping" or select a visible
    /// mapping that could conceal an unscanned contradiction. Both degrade
    /// to `Contradictory`, the same per-branch-halt shape a genuine
    /// incomparable-mapping contradiction already gets. A run with no
    /// truncation at all (the overwhelming common case) is completely
    /// unaffected: `truncated_scans` stays empty and this never fires.
    pub(crate) fn nearest_first_parent_mapping_with_distance(
        &self,
        repo: &Repository,
        tip: Oid,
        exclude_branch: Option<&str>,
    ) -> Result<Option<(usize, MappingLookup)>> {
        let mut revwalk = first_parent_walk(repo, tip, "source")?;
        let mut scanned = 0usize;
        // `enumerate()` doesn't fit here: `scanned`'s final value is also
        // read after the loop for the truncation-taint case below.
        #[allow(clippy::explicit_counter_loop)]
        for oid in &mut revwalk {
            if scanned >= self.scan_limit {
                return Ok(Some((
                    scanned,
                    MappingLookup::Contradictory(format!(
                        "this branch's first-parent history exceeds the {} commit anchor-scan \
                         limit without finding an exact authenticated mapping",
                        self.scan_limit
                    )),
                )));
            }
            let oid = oid.context("walking source history for an exact mapping")?;
            match self.resolve_for_anchor(repo, oid, exclude_branch)? {
                MappingLookup::None => {}
                mapping => return Ok(Some((scanned, mapping))),
            }
            scanned += 1;
        }
        if self.is_truncated() {
            return Ok(Some((
                scanned,
                MappingLookup::Contradictory(self.truncation_message()),
            )));
        }
        Ok(None)
    }

    /// `branch` names the head whose history is being scanned — used only
    /// for diagnostics. Verification is against the marker's own recorded
    /// branch (decisions/0046 Addendum 3, Finding Q), the same
    /// self-verification [`super::loop_prevented`] already applies: once a
    /// marker's owning branch is deleted from source, a descendant's own
    /// first-parent scan is the only remaining path to it, and the HMAC
    /// already authenticates the recorded branch regardless of which head
    /// is doing the scanning.
    ///
    /// Returns whether the commit's mapping (if any) was actually recorded
    /// — `false` only when the aggregate mapping-entry bound was hit
    /// (decisions/0046 addendum), signaling the calling scan to stop rather
    /// than silently skip an entry mid-branch.
    pub(crate) fn add_source_commit(
        &mut self,
        repo: &Repository,
        branch: &str,
        oid: Oid,
        key: &StateKey,
    ) -> Result<bool> {
        let commit = repo.find_commit(oid).with_context(|| {
            format!("resolving a source mapping commit on {branch:?}'s history")
        })?;
        let directions = [MarkerDirection::Setup, MarkerDirection::DestToSource];
        let Some(parsed) = marker::verify_self(&commit, &directions, None, key) else {
            return Ok(true);
        };
        Ok(self.record_bounded(
            oid,
            parsed.counterpart,
            parsed.branch,
            oid,
            parsed.counterpart,
        ))
    }

    /// See [`Self::add_source_commit`]'s doc comment — the same
    /// self-verification, for a dest-side scan: a descendant branch that
    /// correctly anchored onto another branch carries that branch's own
    /// dest commits as real ancestors, still branded with their original
    /// branch regardless of which dest ref is being scanned. Bounded the
    /// same way `add_source_commit` is — used only from a reconstruction
    /// scan; a mapping this run's own push just authored uses
    /// [`Self::record_pushed_dest_commit`] instead, which is exempt from
    /// the aggregate bound (decisions/0046 addendum).
    pub(crate) fn add_dest_commit(
        &mut self,
        repo: &Repository,
        branch: &str,
        oid: Oid,
        key: &StateKey,
    ) -> Result<bool> {
        let Some((source, parsed_branch, canonical_dest)) =
            dest_commit_mapping(repo, branch, oid, key)?
        else {
            return Ok(true);
        };
        Ok(self.record_bounded(source, oid, parsed_branch, oid, canonical_dest))
    }

    /// A mapping this run's own push just authored, re-read and verified
    /// from `oid` (unlike [`Self::record_built_mapping`], which trusts a
    /// tuple `build_dest_commit` already knows outright without a repo
    /// read) — used for the accepted-push cases that don't come from
    /// `build.generated_mappings` (a brand-new branch's reused dest tip, or
    /// `branch_scoped_dest_tip`'s own possibly-aliased marker commit).
    /// Already bounded by the pending-history limits before dest was ever
    /// mutated, so recording it here never fails an otherwise-successful run
    /// over index bookkeeping (decisions/0046 Addendum 2, Finding H): a
    /// commit this call just authored that cannot be re-read out of the
    /// repository records no mapping, rather than failing the run — the
    /// consequence is a later branch finding no anchor and halting
    /// per-branch, never anchoring wrongly. Exempt from the aggregate
    /// mapping-entry bound, unlike [`Self::add_dest_commit`]'s
    /// reconstruction-time counterpart.
    pub(crate) fn record_pushed_dest_commit(
        &mut self,
        repo: &Repository,
        branch: &str,
        oid: Oid,
        key: &StateKey,
    ) {
        let Ok(Some((source, parsed_branch, canonical_dest))) =
            dest_commit_mapping(repo, branch, oid, key)
        else {
            return;
        };
        self.record_unbounded(source, oid, parsed_branch, oid, canonical_dest);
    }

    /// Walks `head`'s first-parent source history, recording every
    /// authenticated mapping found. `visited` is this whole reconstruction
    /// run's shared source-side dedup set (Finding A): a commit already
    /// scanned by an earlier head's walk is skipped outright rather than
    /// re-loaded and re-parsed, and the walk stops there — everything
    /// older was already either recorded or already noted as truncated by
    /// whichever walk reached it first. Hitting this scan's own horizon —
    /// the per-scan commit limit, or the aggregate mapping-entry limit —
    /// keeps every mapping already found, records that this one walk
    /// stopped early, and returns rather than failing the whole run
    /// (decisions/0046 addendum).
    fn scan_source_history(
        &mut self,
        repo: &Repository,
        branch: &str,
        head: Oid,
        key: &StateKey,
        visited: &mut HashSet<Oid>,
    ) -> Result<()> {
        let mut revwalk = first_parent_walk(repo, head, "source")?;
        for (scanned, oid) in (&mut revwalk).enumerate() {
            let oid = oid.context("walking source history for authenticated mappings")?;
            if visited.contains(&oid) {
                // Shared first-parent history already indexed by an
                // earlier head's walk this reconstruction — everything
                // from here on was already recorded, or already noted as
                // truncated, by whichever walk reached it first.
                break;
            }
            if scanned >= self.scan_limit {
                self.truncated_scans.push(format!(
                    "source branch {branch:?} history scan stopped at the {} commit limit",
                    self.scan_limit
                ));
                break;
            }
            visited.insert(oid);
            self.scanned_commits += 1;
            if !self.add_source_commit(repo, branch, oid, key)? {
                self.truncated_scans.push(format!(
                    "source branch {branch:?} history scan stopped after the mapping index \
                     reached its {} entry limit",
                    self.limit
                ));
                break;
            }
        }
        Ok(())
    }

    /// See [`Self::scan_source_history`] — identical shape for dest-side
    /// heads, with its own dedup set (dest and source commits can share an
    /// OID at the `setup` graft, so the two sides are deduped separately).
    fn scan_dest_history(
        &mut self,
        repo: &Repository,
        branch: &str,
        head: Oid,
        key: &StateKey,
        visited: &mut HashSet<Oid>,
    ) -> Result<()> {
        let mut revwalk = first_parent_walk(repo, head, "dest")?;
        for (scanned, oid) in (&mut revwalk).enumerate() {
            let oid = oid.context("walking dest history for authenticated mappings")?;
            if visited.contains(&oid) {
                break;
            }
            if scanned >= self.scan_limit {
                self.truncated_scans.push(format!(
                    "dest branch {branch:?} history scan stopped at the {} commit limit",
                    self.scan_limit
                ));
                break;
            }
            visited.insert(oid);
            self.scanned_commits += 1;
            if !self.add_dest_commit(repo, branch, oid, key)? {
                self.truncated_scans.push(format!(
                    "dest branch {branch:?} history scan stopped after the mapping index \
                     reached its {} entry limit",
                    self.limit
                ));
                break;
            }
        }
        Ok(())
    }

    /// A `ForceMirrorOnly` push for `branch` just replaced its dest ref with
    /// `new_dest_tip`, built from a graft-derived rebuild base rather than
    /// `branch`'s own prior chain — so anything that chain contributed
    /// to this run's index that is no longer an ancestor of (or equal to)
    /// `new_dest_tip` is now a mapping to a dest commit orphaned by that
    /// rebuild (decisions/0046 Addendum 3, Finding Q). Only records
    /// provenanced to `branch` itself are ever considered: a source commit
    /// also mapped through some other branch's own markers keeps that
    /// mapping regardless of what just happened to `branch`'s chain.
    ///
    /// Called once per accepted rewrite-rebuild push, before that push's own
    /// newly built commits are recorded — a stale entry must not survive to
    /// anchor a branch scheduled later in the same run onto dest history
    /// this run itself just discarded.
    /// decisions/0046 Addendum 2, Finding E: this used to walk
    /// `new_dest_tip`'s entire ancestry with a `Revwalk` and `bail!` past
    /// `MAX_MARKER_SCAN_COMMITS`, so a dest tip whose history crossed that
    /// bound killed the whole run on every mirror-only rewrite with no
    /// operator remedy. Git already has a safe, unbounded primitive for
    /// "is this dest commit still reachable from the replacement tip" —
    /// `graph_descendant_of` — so this instead asks it per distinct
    /// `canonical_dest` among `branch`'s own records, memoized so each
    /// distinct destination is queried at most once regardless of how many
    /// source commits map to it. A destination absent from the local object
    /// database is stale by definition, established with `find_commit`
    /// before any reachability query — the same existence gate
    /// `resolve_for_anchor` already applies — rather than by relying on
    /// `graph_descendant_of` never being asked about a missing OID.
    /// `new_dest_tip` itself always counts as reachable.
    pub(crate) fn plan_replaced_chain_invalidation(
        &self,
        repo: &Repository,
        branch: &str,
        new_dest_tip: Oid,
    ) -> Result<Vec<(Oid, Oid, String)>> {
        let mut reachable_memo: HashMap<Oid, bool> = HashMap::new();
        let mut stale = Vec::new();
        for (source, records) in &self.entries {
            for record in records {
                if record.branch != branch {
                    continue;
                }
                let dest = record.canonical_dest;
                let reachable = match reachable_memo.get(&dest) {
                    Some(reachable) => *reachable,
                    None => {
                        let reachable = repo.find_commit(dest).is_ok()
                            && (dest == new_dest_tip
                                || repo.graph_descendant_of(new_dest_tip, dest).with_context(
                                    || {
                                        format!(
                                            "comparing replaced destination {dest} against \
                                             rebuild tip {new_dest_tip}"
                                        )
                                    },
                                )?);
                        reachable_memo.insert(dest, reachable);
                        reachable
                    }
                };
                if !reachable {
                    stale.push((*source, record.marker, record.branch.clone()));
                }
            }
        }
        Ok(stale)
    }

    /// Apply a plan made before a force-with-lease push.  This is infallible:
    /// once the remote accepted the replacement, cache maintenance cannot
    /// turn that successful mutation into a failed sync.
    pub(crate) fn apply_replaced_chain_invalidation(&mut self, stale: &[(Oid, Oid, String)]) {
        if stale.is_empty() {
            return;
        }
        let stale: HashSet<(Oid, Oid, String)> = stale.iter().cloned().collect();
        let mut emptied = Vec::new();
        for (source, records) in &mut self.entries {
            let before = records.len();
            records
                .retain(|record| !stale.contains(&(*source, record.marker, record.branch.clone())));
            self.entry_count = self.entry_count.saturating_sub(before - records.len());
            if records.is_empty() {
                emptied.push(*source);
            }
        }
        for source in emptied {
            self.entries.remove(&source);
        }
    }

    #[cfg(test)]
    pub(crate) fn invalidate_replaced_chain(
        &mut self,
        repo: &Repository,
        branch: &str,
        new_dest_tip: Oid,
    ) -> Result<()> {
        let stale = self.plan_replaced_chain_invalidation(repo, branch, new_dest_tip)?;
        self.apply_replaced_chain_invalidation(&stale);
        Ok(())
    }

    /// Records a `(source, dest)` mapping this run's own
    /// `build_dest_commit` just authored for `branch` — trusted by
    /// construction (Finding S), so unlike
    /// [`add_dest_commit`](Self::add_dest_commit) this never reads `dest`
    /// back out of a repository or re-verifies its marker: the caller
    /// already knows the exact tuple because it just built the commit. A
    /// freshly authored dest commit is never decisions/0046's content-empty
    /// alias shape either (`build_pending_dest_tip` skips building one at
    /// all when the merged tree doesn't change), so `dest` is always its
    /// own canonical destination here.
    ///
    /// This run's own push already succeeded before this is ever called —
    /// already bounded by the pending-history limits, so recording it never
    /// fails an otherwise-successful run over index bookkeeping
    /// (decisions/0046 Addendum 2, Finding H): exempt from the aggregate
    /// mapping-entry bound, unlike a reconstruction scan's own
    /// [`Self::record_bounded`].
    pub(crate) fn record_built_mapping(&mut self, branch: &str, source: Oid, dest: Oid) {
        self.record_unbounded(source, dest, branch.to_owned(), dest, dest);
    }

    /// Used only by a reconstruction scan (decisions/0046 addendum): hitting
    /// the aggregate entry limit degrades to the same truncation-horizon
    /// semantics `scan_source_history`/`scan_dest_history` already apply to
    /// `MAX_MARKER_SCAN_COMMITS`, rather than failing the whole run. Returns
    /// whether the mapping was actually recorded, so the calling scan can
    /// stop there instead of silently skipping an entry mid-branch.
    fn record_bounded(
        &mut self,
        source: Oid,
        raw_dest: Oid,
        branch: String,
        marker: Oid,
        canonical_dest: Oid,
    ) -> bool {
        let record = MappingProvenance {
            branch,
            marker,
            raw_dest,
            canonical_dest,
        };
        if self
            .entries
            .get(&source)
            .is_some_and(|records| records.contains(&record))
        {
            return true;
        }
        if self.entry_count >= self.limit {
            return false;
        }
        self.entry_count += 1;
        self.entries.entry(source).or_default().push(record);
        true
    }

    /// Used only for a mapping already known safe by construction — this
    /// run's own push (see [`Self::record_built_mapping`] and
    /// [`Self::record_pushed_dest_commit`]) — never for a reconstruction
    /// scan. Never fails and never refuses on the aggregate bound.
    fn record_unbounded(
        &mut self,
        source: Oid,
        raw_dest: Oid,
        branch: String,
        marker: Oid,
        canonical_dest: Oid,
    ) {
        let record = MappingProvenance {
            branch,
            marker,
            raw_dest,
            canonical_dest,
        };
        if self
            .entries
            .get(&source)
            .is_some_and(|records| records.contains(&record))
        {
            return;
        }
        self.entry_count += 1;
        self.entries.entry(source).or_default().push(record);
    }
}

/// Shared by [`MappingIndex::add_dest_commit`] (bounded, reconstruction-time)
/// and [`MappingIndex::record_pushed_dest_commit`] (unbounded, this run's own
/// push) — verifies `oid`'s marker once and resolves decisions/0046's
/// content-empty alias to its parent, without deciding how the result gets
/// recorded.
fn dest_commit_mapping(
    repo: &Repository,
    branch: &str,
    oid: Oid,
    key: &StateKey,
) -> Result<Option<(Oid, String, Oid)>> {
    let commit = repo
        .find_commit(oid)
        .with_context(|| format!("resolving a dest mapping commit on {branch:?}'s history"))?;
    let Some(parsed) = marker::verify_self(&commit, &[MarkerDirection::SourceToDest], None, key)
    else {
        return Ok(None);
    };
    let canonical_dest = if source_to_dest_alias(&commit) {
        commit.parent_id(0).expect("alias has one parent")
    } else {
        oid
    };
    Ok(Some((parsed.counterpart, parsed.branch, canonical_dest)))
}

fn first_parent_walk<'repo>(
    repo: &'repo Repository,
    head: Oid,
    side: &str,
) -> Result<git2::Revwalk<'repo>> {
    let mut revwalk = repo
        .revwalk()
        .with_context(|| format!("starting {side} mapping history scan"))?;
    revwalk
        .push(head)
        .with_context(|| format!("seeding {side} mapping history scan"))?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .with_context(|| format!("ordering {side} mapping history scan"))?;
    revwalk
        .simplify_first_parent()
        .with_context(|| format!("restricting {side} mapping history scan to first-parent"))?;
    Ok(revwalk)
}

fn source_to_dest_alias(commit: &git2::Commit<'_>) -> bool {
    commit.parent_count() == 1
        && commit
            .parent(0)
            .ok()
            .is_some_and(|parent| parent.tree_id() == commit.tree_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::marker;

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
        let signature = if message.contains("Gitprism-State:") {
            git2::Signature::now("gitprism", "gitprism@example.com").unwrap()
        } else {
            git2::Signature::now("test", "test@example.com").unwrap()
        };
        let parent_commits = parents
            .iter()
            .map(|oid| repo.find_commit(*oid).unwrap())
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        repo.commit(
            Some(reference),
            &signature,
            &signature,
            message,
            &repo.find_tree(tree).unwrap(),
            &parent_refs,
        )
        .unwrap()
    }

    fn marker_message(
        _repo: &Repository,
        direction: MarkerDirection,
        branch: &str,
        counterpart: Oid,
        parents: &[Oid],
        tree: Oid,
    ) -> String {
        let signature = git2::Signature::now("gitprism", "gitprism@example.com").unwrap();
        marker::build_message(
            "test mapping",
            direction,
            branch,
            counterpart,
            match direction {
                MarkerDirection::SourceToDest => "Gitprism-Source-Commit",
                _ => "Gitprism-Dest-Commit",
            },
            parents,
            tree,
            &signature,
            &signature,
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

    #[test]
    fn mapping_index_reconstructs_verified_markers_and_normalizes_anchors() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let dest_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let dest_message = marker_message(
            &repo,
            MarkerDirection::SourceToDest,
            "feature",
            source,
            &[root],
            dest_tree,
        );
        let dest = commit(&repo, "refs/heads/dest", &[root], dest_tree, &dest_message);
        let setup_tree = tree_with_file(&repo, Some(root), "setup.txt", b"setup");
        let setup_message = marker_message(
            &repo,
            MarkerDirection::Setup,
            "feature",
            root,
            &[root],
            setup_tree,
        );
        let setup = commit(
            &repo,
            "refs/heads/feature-source",
            &[root],
            setup_tree,
            &setup_message,
        );
        let dest_to_source_tree = tree_with_file(&repo, Some(setup), "back.txt", b"back");
        let dest_to_source_message = marker_message(
            &repo,
            MarkerDirection::DestToSource,
            "feature",
            dest,
            &[setup],
            dest_to_source_tree,
        );
        let dest_to_source = commit(
            &repo,
            "refs/heads/feature-source",
            &[setup],
            dest_to_source_tree,
            &dest_to_source_message,
        );
        let invalid_tree = tree_with_file(&repo, Some(dest), "invalid.txt", b"invalid");
        let mut invalid_message = marker_message(
            &repo,
            MarkerDirection::SourceToDest,
            "feature",
            source,
            &[dest],
            invalid_tree,
        );
        let mac = invalid_message
            .rfind('0')
            .unwrap_or_else(|| invalid_message.len() - 2);
        invalid_message.replace_range(mac..=mac, "1");
        let invalid = commit(
            &repo,
            "refs/heads/dest",
            &[dest],
            invalid_tree,
            &invalid_message,
        );
        assert_eq!(
            marker::verify(
                &repo.find_commit(dest).unwrap(),
                "feature",
                &[MarkerDirection::SourceToDest],
                None,
                &marker::load_key().unwrap(),
            ),
            Some(source)
        );

        let index = MappingIndex::reconstruct(
            &repo,
            &[("feature".to_owned(), dest_to_source)],
            &[("feature".to_owned(), invalid)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let source_mapping = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(source_mapping.dest, dest);
        assert!(
            source_mapping
                .provenance
                .iter()
                .all(|record| record.branch == "feature")
        );
        assert_eq!(index.resolve(&repo, setup).unwrap().unwrap().dest, root);
        assert_eq!(
            index.resolve(&repo, dest_to_source).unwrap().unwrap().dest,
            dest
        );
        assert!(index.resolve(&repo, invalid).unwrap().is_none());
    }

    #[test]
    fn mapping_index_collapses_empty_aliases_and_chooses_a_comparable_ancestor() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let base_tree = tree_with_file(&repo, Some(root), "base.txt", b"base");
        let base = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[root],
                base_tree,
            ),
        );
        let alias_one = commit(
            &repo,
            "refs/heads/dest",
            &[base],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[base],
                base_tree,
            ),
        );
        let alias_two = commit(
            &repo,
            "refs/heads/dest",
            &[alias_one],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[alias_one],
                base_tree,
            ),
        );
        let desc_tree = tree_with_file(&repo, Some(alias_two), "desc.txt", b"desc");
        let desc = commit(
            &repo,
            "refs/heads/dest",
            &[alias_two],
            desc_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[alias_two],
                desc_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[("feature".to_owned(), desc)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let resolved = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(resolved.dest, base);
        assert_eq!(resolved.provenance.len(), 4);
        assert!(
            resolved
                .provenance
                .iter()
                .any(|record| record.raw_dest == alias_one && record.canonical_dest == base)
        );
        assert!(
            resolved
                .provenance
                .iter()
                .any(|record| record.raw_dest == desc && record.canonical_dest == desc)
        );
    }

    #[test]
    fn mapping_index_reports_incomparable_dest_mappings_with_provenance() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let left_tree = tree_with_file(&repo, Some(root), "left.txt", b"left");
        let left = commit(
            &repo,
            "refs/heads/left",
            &[root],
            left_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "left",
                source,
                &[root],
                left_tree,
            ),
        );
        let right_tree = tree_with_file(&repo, Some(root), "right.txt", b"right");
        let right = commit(
            &repo,
            "refs/heads/right",
            &[root],
            right_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "right",
                source,
                &[root],
                right_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[("left".to_owned(), left), ("right".to_owned(), right)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let error = index.resolve(&repo, source).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&source.to_string()));
        assert!(message.contains(&left.to_string()));
        assert!(message.contains(&right.to_string()));
        assert!(message.contains("left"));
        assert!(message.contains("right"));
        assert!(message.contains("contradictory"));
    }

    #[test]
    fn record_built_mapping_adds_an_exact_mapping_without_reading_the_repo() {
        // Finding S: `build_dest_commit` already knows the exact (source,
        // dest, branch) tuple for a commit it just authored — recording it
        // must not need to look the commit back up or re-verify its marker,
        // so `source` here doesn't correspond to any real object in
        // `repo`'s odb at all. `dest` must be a real, locally present
        // commit: Finding R's existence check in `resolve` (a different
        // method, exercised below to prove the mapping was recorded, not by
        // `record_built_mapping` itself) would otherwise correctly treat an
        // ordinary orphaned
        // mapping as unusable.
        let (_dir, repo, root) = empty_repo();
        let source = Oid::from_bytes(&[3; 20]).unwrap();
        let dest = root;
        let mut index = MappingIndex::new();

        index.record_built_mapping("feature", source, dest);

        let mapping = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(mapping.dest, dest);
        assert_eq!(mapping.provenance.len(), 1);
        assert_eq!(mapping.provenance[0].branch, "feature");
        assert_eq!(mapping.provenance[0].raw_dest, dest);
        assert_eq!(mapping.provenance[0].canonical_dest, dest);
    }

    #[test]
    fn record_bounded_refuses_silently_once_the_aggregate_entry_limit_is_reached() {
        // Finding B: hitting the aggregate bound during reconstruction must
        // degrade (no recording, no error) rather than fail the whole run —
        // the calling scan decides what "false" means (a truncation note).
        let mut index = MappingIndex::with_limit(2);
        let source = Oid::from_bytes(&[1; 20]).unwrap();
        for n in 0..2 {
            let mut bytes = [0; 20];
            bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            assert!(index.record_bounded(
                source,
                Oid::from_bytes(&bytes).unwrap(),
                format!("branch-{n}"),
                source,
                Oid::from_bytes(&bytes).unwrap(),
            ));
        }
        assert_eq!(index.entry_count, 2);
        let recorded = index.record_bounded(
            source,
            Oid::from_bytes(&[9; 20]).unwrap(),
            "overflow".to_owned(),
            source,
            Oid::from_bytes(&[9; 20]).unwrap(),
        );
        assert!(
            !recorded,
            "hitting the aggregate limit must refuse silently, not error"
        );
        assert_eq!(index.entry_count, 2);
        assert_eq!(index.entries.get(&source).unwrap().len(), 2);
    }

    #[test]
    fn record_built_mapping_is_never_refused_by_the_aggregate_entry_limit() {
        // Finding B, part 1: this run's own push already succeeded — index
        // bookkeeping for it must never fail the run afterward, even once
        // the aggregate bound reconstruction uses has already been reached.
        let mut index = MappingIndex::with_limit(1);
        index.record_built_mapping(
            "first",
            Oid::from_bytes(&[1; 20]).unwrap(),
            Oid::from_bytes(&[2; 20]).unwrap(),
        );
        assert_eq!(index.entry_count, 1);

        index.record_built_mapping(
            "second",
            Oid::from_bytes(&[3; 20]).unwrap(),
            Oid::from_bytes(&[4; 20]).unwrap(),
        );
        assert_eq!(
            index.entry_count, 2,
            "recording a mapping this run's own push just authored must succeed \
             past the aggregate limit, never error"
        );
    }

    #[test]
    fn record_pushed_dest_commit_records_nothing_for_an_oid_absent_from_the_object_database() {
        // decisions/0046 Addendum 2, Finding H: `record_pushed_dest_commit`
        // is called after the corresponding push already landed, so it must
        // never fail the run — a commit it cannot re-read simply records no
        // mapping. The consequence is a later branch finding no anchor and
        // halting per-branch, never anchoring wrongly.
        let (_dir, repo, _root) = empty_repo();
        let missing = Oid::from_bytes(&[9; 20]).unwrap();
        let mut index = MappingIndex::new();

        index.record_pushed_dest_commit(&repo, "feature", missing, &marker::load_key().unwrap());

        assert_eq!(index.entry_count, 0);
    }

    #[test]
    fn mapping_index_counts_an_inherited_setup_marker_once_across_source_heads() {
        let (_dir, repo, root) = empty_repo();
        let setup_tree = tree_with_file(&repo, Some(root), "setup.txt", b"setup");
        let setup = commit(
            &repo,
            "refs/heads/setup",
            &[root],
            setup_tree,
            &marker_message(
                &repo,
                MarkerDirection::Setup,
                "origin",
                root,
                &[root],
                setup_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[("first".to_owned(), setup), ("second".to_owned(), setup)],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let mapping = index.resolve(&repo, setup).unwrap().unwrap();
        assert_eq!(mapping.dest, root);
        assert_eq!(mapping.provenance.len(), 1);
        assert_eq!(index.entry_count, 1);
    }

    #[test]
    fn nearest_mapping_walks_first_parent_and_ignores_mapped_side_parent() {
        // The ordinary and filtered-only commits have no authenticated
        // mapping; the merge's mapped side parent must not be considered.
        let (_dir, repo, root) = empty_repo();
        let mapped_tree = tree_with_file(&repo, Some(root), "mapped.txt", b"mapped");
        let mapped = commit(
            &repo,
            "refs/heads/main",
            &[root],
            mapped_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "main",
                root,
                &[root],
                mapped_tree,
            ),
        );
        let ordinary_tree = tree_with_file(&repo, Some(mapped), "ordinary.txt", b"ordinary");
        let ordinary = commit(
            &repo,
            "refs/heads/main",
            &[mapped],
            ordinary_tree,
            "ordinary",
        );
        let filtered_tree = tree_with_file(&repo, Some(ordinary), "secret.txt", b"filtered");
        let filtered = commit(
            &repo,
            "refs/heads/main",
            &[ordinary],
            filtered_tree,
            "filtered-only change",
        );
        let side_tree = tree_with_file(&repo, Some(filtered), "side.txt", b"side");
        let side = commit(
            &repo,
            "refs/heads/side",
            &[filtered],
            side_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "side",
                root,
                &[filtered],
                side_tree,
            ),
        );
        let merge = commit(
            &repo,
            "refs/heads/main",
            &[filtered, side],
            filtered_tree,
            "merge side",
        );

        let index = MappingIndex::reconstruct(
            &repo,
            &[("main".to_owned(), merge), ("side".to_owned(), side)],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let side_mapping = index.resolve(&repo, side).unwrap().unwrap();
        assert_eq!(side_mapping.source, side);
        let nearest = match index
            .nearest_first_parent_mapping(&repo, merge, None)
            .unwrap()
        {
            MappingLookup::Resolved(mapping) => mapping,
            other => panic!("expected a resolved mapping, got {other:?}"),
        };
        assert_eq!(nearest.source, mapped);
        assert_eq!(nearest.dest, root);
        assert!(
            nearest
                .provenance
                .iter()
                .all(|record| record.branch == "main")
        );
    }

    #[test]
    fn invalidate_replaced_chain_drops_only_the_named_branchs_unreachable_mappings() {
        // Mirrors Finding Q: "feature" mapped S1 -> D1 and S2 -> D2. A
        // ForceMirrorOnly rebuild replaces feature's chain with D1 -> D3,
        // orphaning D2. "sibling" separately mapped that same S2 to its own
        // D2' — a mapping that must survive since it isn't feature's.
        let (_dir, repo, root) = empty_repo();
        let s1_tree = tree_with_file(&repo, Some(root), "s1.txt", b"s1");
        let s1 = commit(&repo, "refs/heads/s1", &[root], s1_tree, "s1");
        let s2_tree = tree_with_file(&repo, Some(root), "s2.txt", b"s2");
        let s2 = commit(&repo, "refs/heads/s2", &[s1], s2_tree, "s2");

        let d1_tree = tree_with_file(&repo, Some(root), "d1.txt", b"d1");
        let d1 = commit(
            &repo,
            "refs/heads/feature",
            &[root],
            d1_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                s1,
                &[root],
                d1_tree,
            ),
        );
        let d2_tree = tree_with_file(&repo, Some(root), "d2.txt", b"d2");
        let d2 = commit(
            &repo,
            "refs/heads/feature",
            &[d1],
            d2_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                s2,
                &[d1],
                d2_tree,
            ),
        );
        let d2_sibling_tree = tree_with_file(&repo, Some(root), "sibling.txt", b"sibling");
        let d2_sibling = commit(
            &repo,
            "refs/heads/sibling",
            &[root],
            d2_sibling_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "sibling",
                s2,
                &[root],
                d2_sibling_tree,
            ),
        );

        let mut index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[
                ("feature".to_owned(), d2),
                ("sibling".to_owned(), d2_sibling),
            ],
            &marker::load_key().unwrap(),
        )
        .unwrap();

        // The rebuild: a fresh D3 on top of D1, never descending from D2 —
        // built detached (no ref update) since "feature" itself still
        // points at D2 here; only D3's oid is needed for the reachability
        // check below.
        let d3_tree = tree_with_file(&repo, Some(d1), "d3.txt", b"d3");
        let signature = git2::Signature::now("test", "test@example.com").unwrap();
        let d3 = repo
            .commit(
                None,
                &signature,
                &signature,
                "rebuilt d3",
                &repo.find_tree(d3_tree).unwrap(),
                &[&repo.find_commit(d1).unwrap()],
            )
            .unwrap();

        index
            .invalidate_replaced_chain(&repo, "feature", d3)
            .unwrap();

        // A source-side marker can name a destination object this clone no
        // longer has.  Invalidation must classify it as stale without a
        // fallible ancestry query (and therefore remain safe after the
        // corresponding force-with-lease push).
        let missing_dest = Oid::from_bytes(&[7; 20]).unwrap();
        index.record_built_mapping("feature", root, missing_dest);
        index
            .invalidate_replaced_chain(&repo, "feature", d3)
            .unwrap();
        assert!(index.resolve(&repo, root).unwrap().is_none());

        assert_eq!(
            index.resolve(&repo, s1).unwrap().unwrap().dest,
            d1,
            "S1 -> D1 is still reachable from the new tip and must survive"
        );
        assert_eq!(
            index.resolve(&repo, s2).unwrap().unwrap().dest,
            d2_sibling,
            "feature's own orphaned S2 -> D2 must be dropped, leaving sibling's own mapping"
        );
        assert!(
            index
                .resolve(&repo, s2)
                .unwrap()
                .unwrap()
                .provenance
                .iter()
                .all(|record| record.branch == "sibling"),
            "no remaining provenance for S2 may still claim the invalidated feature branch"
        );
    }

    #[test]
    fn plan_replaced_chain_invalidation_handles_a_replacement_chain_and_orphan_count_far_beyond_a_trivial_handful_without_erroring()
     {
        // decisions/0046 Addendum 2, Finding E: the old implementation
        // walked the entire replacement chain with a `Revwalk` and
        // `bail!`ed past `MAX_MARKER_SCAN_COMMITS`. The new implementation
        // queries `graph_descendant_of` once per distinct destination
        // instead, so its cost is independent of the replacement chain's
        // depth. A real `MAX_MARKER_SCAN_COMMITS`-deep repository is
        // impractical to build in a unit test; this proves the same
        // property structurally, at a chain length and orphaned-mapping
        // count clearly beyond this file's other tests (a handful of
        // commits) — nothing in `plan_replaced_chain_invalidation` any
        // longer references that limit.
        let (_dir, repo, root) = empty_repo();

        let chain_len = 300usize;
        let mut tip = root;
        for step in 0..chain_len {
            let tree = tree_with_file(&repo, Some(tip), &format!("d{step}.txt"), b"d");
            tip = commit(
                &repo,
                "refs/heads/feature",
                &[tip],
                tree,
                &format!("d{step}"),
            );
        }
        let new_dest_tip = tip;

        let mut index = MappingIndex::new();
        let orphan_count = 100usize;
        for n in 0..orphan_count {
            let mut dest_bytes = [0u8; 20];
            dest_bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            let mut source_bytes = [0u8; 20];
            source_bytes[8..16].copy_from_slice(&(n as u64).to_be_bytes());
            index.record_built_mapping(
                "feature",
                Oid::from_bytes(&source_bytes).unwrap(),
                Oid::from_bytes(&dest_bytes).unwrap(),
            );
        }
        // A mapping still reachable from the rebuild tip (root is an
        // ancestor of every commit in the chain built above) must survive
        // invalidation, and a mapping provenanced to another branch must
        // never be considered at all.
        let survivor_source = Oid::from_bytes(&[42; 20]).unwrap();
        index.record_built_mapping("feature", survivor_source, root);
        let other_branch_source = Oid::from_bytes(&[43; 20]).unwrap();
        index.record_built_mapping("sibling", other_branch_source, root);

        let stale = index
            .plan_replaced_chain_invalidation(&repo, "feature", new_dest_tip)
            .unwrap();

        assert_eq!(stale.len(), orphan_count);
        assert!(
            !stale
                .iter()
                .any(|(source, _, _)| *source == survivor_source)
        );
        assert!(
            !stale
                .iter()
                .any(|(source, _, _)| *source == other_branch_source)
        );
    }

    #[test]
    fn nearest_mapping_propagates_missing_tip_errors() {
        let (_dir, repo, _root) = empty_repo();
        let index = MappingIndex::new();
        let missing = Oid::from_bytes(&[8; 20]).unwrap();
        let error = index
            .nearest_first_parent_mapping(&repo, missing, None)
            .unwrap_err();
        assert!(!error.to_string().contains("contradictory"));
    }

    #[test]
    fn mapping_index_self_verifies_a_source_marker_against_its_own_recorded_branch() {
        // Finding Q: once "feature" itself is deleted from source, "task"'s
        // own scan is the only remaining path to a DestToSource marker
        // branded "feature" that task inherited as an ordinary ancestor
        // commit — self-verification must accept it even though the
        // scanning head ("task") differs from the marker's own recorded
        // branch.
        let (_dir, repo, root) = empty_repo();
        let marker_tree = tree_with_file(&repo, Some(root), "back.txt", b"back");
        let imported = commit(
            &repo,
            "refs/heads/task",
            &[root],
            marker_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "feature",
                root,
                &[root],
                marker_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[("task".to_owned(), imported)],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let mapping = index.resolve(&repo, imported).unwrap().unwrap();
        assert_eq!(mapping.dest, root);
        assert_eq!(mapping.provenance[0].branch, "feature");
    }

    #[test]
    fn mapping_index_self_verifies_a_dest_marker_against_its_own_recorded_branch() {
        // Same defect on the dest-side scan: "task" correctly anchored onto
        // "feature" carries feature's own SourceToDest marker as a real
        // ancestor commit in task's dest chain — scanned under "task"'s dest
        // ref, but still branded "feature".
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let dest_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let feature_dest = commit(
            &repo,
            "refs/heads/task",
            &[root],
            dest_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[root],
                dest_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[("task".to_owned(), feature_dest)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let mapping = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(mapping.dest, feature_dest);
        assert_eq!(mapping.provenance[0].branch, "feature");
    }

    #[test]
    fn nearest_mapping_halts_on_a_source_commit_whose_only_mapped_dest_is_missing_locally() {
        // Finding R: a verified marker naming a dest commit this clone never
        // fetched (its dest ref deleted after merge, or discarded by a
        // force rewind) is as unusable for anchoring as no mapping at
        // all — the walk must halt rather than continue to an older mapping
        // that could silently omit content imported by the missing marker.
        let (_dir, repo, root) = empty_repo();
        let older_tree = tree_with_file(&repo, Some(root), "older.txt", b"older");
        let older = commit(&repo, "refs/heads/main", &[root], older_tree, "older");
        let newer_tree = tree_with_file(&repo, Some(older), "newer.txt", b"newer");
        let newer = commit(&repo, "refs/heads/main", &[older], newer_tree, "newer");

        let missing_dest = Oid::from_bytes(&[9; 20]).unwrap();
        let mut index = MappingIndex::new();
        index.record_built_mapping("feature", newer, missing_dest);
        index.record_built_mapping("feature", older, root);

        let nearest = match index
            .nearest_first_parent_mapping(&repo, newer, None)
            .unwrap()
        {
            MappingLookup::Contradictory(diagnostic) => diagnostic,
            other => panic!("expected a safe per-branch halt, got {other:?}"),
        };
        assert!(nearest.contains("not present in this clone"));
    }

    #[test]
    fn nearest_mapping_refuses_rather_than_erroring_when_one_of_several_mappings_is_missing_locally()
     {
        // Finding R: canonicalizing among several mappings for the same exact
        // source commit compares their dest oids by ancestry —
        // `graph_descendant_of` has no clean answer for an oid this clone
        // never fetched, so whether the missing one would have dominated
        // can't be decided safely. That must degrade to a per-branch
        // refusal (the existing halt precedent), never a raw lookup error
        // that aborts the whole run.
        let (_dir, repo, root) = empty_repo();
        let present_tree = tree_with_file(&repo, Some(root), "present.txt", b"present");
        let present_dest = commit(&repo, "refs/heads/main", &[root], present_tree, "present");
        let missing_dest = Oid::from_bytes(&[9; 20]).unwrap();

        let mut index = MappingIndex::new();
        index.record_built_mapping("left", root, present_dest);
        index.record_built_mapping("right", root, missing_dest);

        let lookup = index
            .nearest_first_parent_mapping(&repo, root, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "an ancestry comparison against a missing dest object must degrade to a \
             per-branch refusal, not propagate a raw lookup error and abort the whole run, \
             got {lookup:?}"
        );
    }

    #[test]
    fn resolve_for_anchor_excludes_the_current_branchs_own_sole_projection_of_a_shared_ancestor() {
        // Finding S: legacy wrong-order state a pre-decisions/0046 run could
        // produce: task's own now-discarded chain projected F as D_task_F
        // while feature's separate chain projects the identical F as
        // D_feat_F — parallel, incomparable canonical dests for the same
        // exact source commit. Resolving F's anchor for task's OWN rewrite
        // must exclude task's own stale projection, since task's own chain
        // is exactly what the rewrite discards; without exclusion this is
        // a genuine, permanent contradiction.
        let (_dir, repo, root) = empty_repo();
        let f_tree = tree_with_file(&repo, Some(root), "f.txt", b"f");
        let f = commit(&repo, "refs/heads/feature", &[root], f_tree, "F");

        let d_task_f = commit(
            &repo,
            "refs/heads/task",
            &[root],
            f_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "task",
                f,
                &[root],
                f_tree,
            ),
        );
        let d_feat_f = commit(
            &repo,
            "refs/heads/other-dest",
            &[root],
            f_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                f,
                &[root],
                f_tree,
            ),
        );

        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[
                ("task".to_owned(), d_task_f),
                ("feature".to_owned(), d_feat_f),
            ],
            &marker::load_key().unwrap(),
        )
        .unwrap();

        // Without exclusion: genuinely contradictory — both are real,
        // incomparable canonical dests for the identical source commit.
        let error = index.resolve(&repo, f).unwrap_err();
        assert!(error.to_string().contains("contradictory"));

        // Resolving F's anchor for task's own rewrite excludes task's own
        // sole projection, leaving feature's as the unambiguous anchor.
        let resolved = match index.resolve_for_anchor(&repo, f, Some("task")).unwrap() {
            MappingLookup::Resolved(mapping) => mapping,
            other => panic!("expected task's own stale projection to be excluded, got {other:?}"),
        };
        assert_eq!(resolved.dest, d_feat_f);

        // Resolving the same commit for feature's own rewrite must not
        // exclude feature's projection and find nothing left instead.
        let resolved_for_feature = match index
            .resolve_for_anchor(&repo, f, Some("feature"))
            .unwrap()
        {
            MappingLookup::Resolved(mapping) => mapping,
            other => panic!("expected feature's own projection to remain usable, got {other:?}"),
        };
        assert_eq!(resolved_for_feature.dest, d_task_f);
    }

    #[test]
    fn resolve_for_anchor_keeps_the_current_branchs_own_mapping_when_no_alternative_exists() {
        // Finding S: self-exclusion only prefers a comparable/incomparable
        // alternative some OTHER branch already projected for the identical
        // source commit. When nothing else maps it at all, the branch's own
        // mapping is untouched ancestor content an amend or rebase didn't
        // reach — not part of the chain being discarded — and remains a
        // normal, usable anchor. (Regression: `run_does_not_resurrect_an_
        // orphaned_dest_commit_after_a_same_run_amend` needs exactly this —
        // an amended branch's still-valid earlier S1 -> D1 mapping, branded
        // only to itself, must still resolve during its own rewrite.)
        let (_dir, repo, root) = empty_repo();
        let mut index = MappingIndex::new();
        index.record_built_mapping("feature", root, root);

        let resolved = match index
            .resolve_for_anchor(&repo, root, Some("feature"))
            .unwrap()
        {
            MappingLookup::Resolved(mapping) => mapping,
            other => {
                panic!("expected the branch's own sole mapping to remain usable, got {other:?}")
            }
        };
        assert_eq!(resolved.dest, root);
    }

    /// Builds a first-parent chain of `count` plain (markerless) commits on
    /// top of `parent`, returning the tip. Used by the Finding A tests below,
    /// where only chain length/shape matters, not marker content.
    fn commit_chain(repo: &Repository, branch: &str, parent: Oid, count: usize) -> Oid {
        let mut tip = parent;
        for n in 0..count {
            let tree = tree_with_file(
                repo,
                Some(tip),
                &format!("{branch}-{n}.txt"),
                format!("{branch}-{n}").as_bytes(),
            );
            tip = commit(repo, &format!("refs/heads/{branch}"), &[tip], tree, "plain");
        }
        tip
    }

    #[test]
    fn reconstruction_does_not_rescan_first_parent_history_shared_between_branches() {
        // Finding A, part 1: two branches sharing a long common base must
        // not each pay for scanning that shared base — a commit already
        // scanned by an earlier branch's walk is never re-loaded/re-parsed.
        let (_dir, repo, root) = empty_repo();
        // Shared base: root -> shared (2 plain commits).
        let shared = commit_chain(&repo, "shared", root, 2);
        repo.branch("branch-a", &repo.find_commit(shared).unwrap(), false)
            .unwrap();
        repo.branch("branch-b", &repo.find_commit(shared).unwrap(), false)
            .unwrap();
        let tip_a = commit_chain(&repo, "branch-a", shared, 2);
        let tip_b = commit_chain(&repo, "branch-b", shared, 2);

        let index = MappingIndex::reconstruct(
            &repo,
            &[
                ("branch-a".to_owned(), tip_a),
                ("branch-b".to_owned(), tip_b),
            ],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();

        // Unique commits: root, the 2 shared commits, 2 on branch-a, 2 on
        // branch-b = 7. Without dedup, each branch's own full walk (5
        // commits: its own 2 plus the 2 shared plus root) would total 10.
        assert_eq!(
            index.scanned_commit_count(),
            7,
            "shared first-parent history must be scanned once, not once per branch"
        );
    }

    #[test]
    fn reconstruction_truncates_a_scan_that_exceeds_its_horizon_instead_of_erroring() {
        // Finding A, part 2: hitting the per-scan commit limit keeps every
        // mapping already found and stops that one walk — it must not fail
        // the whole run.
        let (_dir, repo, root) = empty_repo();
        let setup_tree = tree_with_file(&repo, Some(root), "setup.txt", b"setup");
        let setup = commit(
            &repo,
            "refs/heads/main",
            &[root],
            setup_tree,
            &marker_message(
                &repo,
                MarkerDirection::Setup,
                "main",
                root,
                &[root],
                setup_tree,
            ),
        );
        // 3 plain commits between the marker and the tip; a scan limit of 2
        // reaches the tip and one more commit, then must stop before ever
        // reaching `setup`.
        let tip = commit_chain(&repo, "main", setup, 3);

        let index = MappingIndex::reconstruct_with_scan_limit(
            &repo,
            &[("main".to_owned(), tip)],
            &[],
            &marker::load_key().unwrap(),
            2,
        )
        .expect("hitting the scan horizon must not error the whole reconstruction");
        assert!(
            index.is_truncated(),
            "the scan must record that it stopped early"
        );
        // The marker itself was never reached by the truncated scan, so an
        // exact lookup for it correctly finds nothing recorded — truncation
        // does not fabricate a mapping that was never actually scanned.
        assert!(index.resolve(&repo, root).unwrap().is_none());
    }

    #[test]
    fn a_branch_whose_own_walk_exceeds_the_scan_horizon_halts_per_branch_not_the_whole_run() {
        // Finding A / C: a branch whose own first-parent chain is longer
        // than the scan horizon and carries no mapping within it must
        // refuse (naming the truncation), not error out of the run and not
        // silently fall back to "no shared history".
        let (_dir, repo, root) = empty_repo();
        let tip = commit_chain(&repo, "main", root, 5);
        let index = MappingIndex::with_scan_limit(2);
        // No reconstruction scan is even needed to observe this: the same
        // horizon applies to the branch's own anchor-resolution walk.
        let lookup = index
            .nearest_first_parent_mapping(&repo, tip, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "a branch chain longer than the scan horizon, with no mapping found within it, \
             must refuse rather than report no mapping or error the run, got {lookup:?}"
        );
    }

    #[test]
    fn a_truncated_reconstruction_taints_an_unrelated_branchs_no_mapping_result() {
        // Finding A: a totally unrelated branch's own walk completes fully
        // (no mapping anywhere in its own short history, which would
        // ordinarily mean decisions/0024's benign no-shared-history
        // classification) — but because some OTHER branch's reconstruction
        // scan was truncated this run, "no mapping" can no longer be
        // trusted, so this must refuse instead of silently classifying the
        // branch as having no shared history.
        let (_dir, repo, long_root) = empty_repo();
        let long_tip = commit_chain(&repo, "long", long_root, 5);

        let short_tree = tree_with_file(&repo, None, "short-root.txt", b"short-root");
        let short_root = commit(&repo, "refs/heads/short", &[], short_tree, "short root");
        let short_tip = commit_chain(&repo, "short", short_root, 1);

        let index = MappingIndex::reconstruct_with_scan_limit(
            &repo,
            &[
                ("long".to_owned(), long_tip),
                ("short".to_owned(), short_tip),
            ],
            &[],
            &marker::load_key().unwrap(),
            2,
        )
        .unwrap();
        assert!(index.is_truncated());

        // "short"'s own chain (2 commits) is fully within the scan horizon
        // and carries no marker anywhere — walking it alone would exhaust
        // the revwalk and legitimately find nothing.
        let lookup = index
            .nearest_first_parent_mapping(&repo, short_tip, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "an unrelated branch's own complete, mapping-free walk must still refuse \
             once any reconstruction scan was truncated this run, got {lookup:?}"
        );
    }

    #[test]
    fn truncation_elsewhere_halts_even_a_branch_that_has_a_mapping() {
        // A real mapping cannot be trusted while another reconstruction scan
        // stopped before its full history was inspected: an incomparable
        // mapping may be hidden beyond that horizon.
        let (_dir, repo, long_root) = empty_repo();
        let long_tip = commit_chain(&repo, "long", long_root, 5);

        let setup_tree = tree_with_file(&repo, Some(long_root), "setup.txt", b"setup");
        let setup = commit(
            &repo,
            "refs/heads/short",
            &[long_root],
            setup_tree,
            &marker_message(
                &repo,
                MarkerDirection::Setup,
                "short",
                long_root,
                &[long_root],
                setup_tree,
            ),
        );
        let short_tip = commit_chain(&repo, "short", setup, 1);

        let index = MappingIndex::reconstruct_with_scan_limit(
            &repo,
            &[
                ("long".to_owned(), long_tip),
                ("short".to_owned(), short_tip),
            ],
            &[],
            &marker::load_key().unwrap(),
            2,
        )
        .unwrap();
        assert!(index.is_truncated());

        let lookup = index
            .nearest_first_parent_mapping(&repo, short_tip, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "got {lookup:?}"
        );
    }

    #[test]
    fn truncated_index_never_hides_an_incomparable_mapping_for_an_exact_source() {
        // If one reconstruction horizon stopped after recording S -> D1,
        // another unscanned marker may still record S -> D2.  The visible
        // mapping must not be selected while the index is incomplete.
        let (_dir, repo, root) = empty_repo();
        let left_tree = tree_with_file(&repo, Some(root), "left.txt", b"left");
        let left = commit(&repo, "refs/heads/left", &[root], left_tree, "left");
        let right_tree = tree_with_file(&repo, Some(root), "right.txt", b"right");
        let right = commit(&repo, "refs/heads/right", &[root], right_tree, "right");
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");

        let mut index = MappingIndex::new();
        index.record_built_mapping("left", source, left);
        index.record_built_mapping("right", source, right);
        index
            .truncated_scans
            .push("dest branch \"right\" history scan stopped".to_owned());

        let lookup = index
            .nearest_first_parent_mapping(&repo, source, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "an incomplete index must not select a visible mapping, got {lookup:?}"
        );
    }

    #[test]
    fn an_index_noted_incomplete_from_outside_the_history_scans_refuses_an_exact_lookup_that_would_otherwise_resolve()
     {
        // decisions/0046 Addendum 2, Finding F: a truncated dest branch
        // listing is an incompleteness observed outside
        // `scan_source_history`/`scan_dest_history` themselves, but it must
        // taint exact lookups exactly the same way a truncated internal
        // scan already does — an unlisted dest branch could carry an
        // incomparable mapping for a source commit this run did observe.
        let (_dir, repo, root) = empty_repo();
        let dest_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let dest = commit(&repo, "refs/heads/dest", &[root], dest_tree, "dest");

        let mut index = MappingIndex::new();
        index.record_built_mapping("feature", root, dest);
        assert!(index.resolve(&repo, root).unwrap().is_some());

        index.note_incomplete_reconstruction(
            "dest branch listing truncated at the 4096 branch limit".to_owned(),
        );

        let lookup = index
            .nearest_first_parent_mapping(&repo, root, None)
            .unwrap();
        assert!(
            matches!(lookup, MappingLookup::Contradictory(_)),
            "an index noted incomplete outside its own scans must refuse an otherwise-resolvable \
             lookup, got {lookup:?}"
        );
    }

    #[test]
    fn reconstruction_scan_stops_without_erroring_once_the_aggregate_entry_limit_is_hit() {
        // Finding B, part 2: hitting the aggregate mapping-entry limit
        // during a reconstruction scan degrades the same way exceeding the
        // per-scan commit limit does — keep what was found, note the
        // truncation, stop that walk. It must never fail the whole run.
        let (_dir, repo, root) = empty_repo();
        let first_tree = tree_with_file(&repo, Some(root), "first.txt", b"first");
        let first = commit(
            &repo,
            "refs/heads/main",
            &[root],
            first_tree,
            &marker_message(
                &repo,
                MarkerDirection::Setup,
                "main",
                root,
                &[root],
                first_tree,
            ),
        );
        let second_tree = tree_with_file(&repo, Some(first), "second.txt", b"second");
        let second = commit(
            &repo,
            "refs/heads/main",
            &[first],
            second_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "main",
                first,
                &[first],
                second_tree,
            ),
        );

        let mut index = MappingIndex::with_limit(1);
        let mut visited = HashSet::new();
        index
            .scan_source_history(
                &repo,
                "main",
                second,
                &marker::load_key().unwrap(),
                &mut visited,
            )
            .expect("hitting the aggregate entry limit must not error the scan");

        assert_eq!(
            index.entry_count, 1,
            "only the first mapping fit within the limit"
        );
        assert!(
            index.is_truncated(),
            "the scan must record that the entry limit stopped it early"
        );
    }
}
