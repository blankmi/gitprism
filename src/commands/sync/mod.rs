//! `gitprism sync` — see design/playbooks/0001-gitlab-pipeline-triggers.md,
//! design/decisions/0003-mapping-state-in-commit-trailers.md,
//! design/decisions/0006-setup-uses-real-shared-history.md,
//! design/decisions/0007-conflict-policy-hard-stop.md,
//! design/decisions/0009-push-race-refetch-and-recompute.md,
//! design/decisions/0013-repo-urls-optional-fall-back-to-env-vars.md,
//! design/decisions/0016-both-directions-merge-via-real-git-merge-tree.md, and
//! design/decisions/0017-source-to-dest-mirrors-every-branch.md.
//!
//! The two directions no longer share one configured list of branches
//! (decisions/0017 supersedes decisions/0005 for source→dest's scope): every
//! run does dest→source first for every branch named in `config.branches`,
//! then discovers every branch that actually exists on source and does
//! source→dest for each one — a brand-new branch needs no config entry to
//! start mirroring. Grouping by phase rather than by branch is safe because
//! branches are otherwise independent, and it still guarantees dest→source
//! for a given branch completes before source→dest reads that branch's
//! (possibly just-advanced) local tip. Neither direction does its own merge
//! or patch work any more: each builds a `(base, ours, theirs)` tree triple
//! for a pending commit and hands it to one real `git merge-tree
//! --write-tree` subprocess (decisions/0016), which computes the resulting
//! tree exactly the way a human running `git cherry-pick`/`git merge` would
//! see it — idempotent, rename-aware, and unable to disagree between
//! directions about what counts as a conflict.
//!
//! **source→dest**: for every branch discovered on source, fetch dest's
//! current tip for the same-named branch, find every source commit not yet
//! reflected there, and for each one merge dest's current chain-tip tree
//! (`ours`) against the source commit's own tree (`theirs`), both filtered
//! against the *current* exclude-list (decisions/0004, 0011 — not a
//! historical reconstruction of what it looked like at that commit) before
//! the merge ever sees them — an excluded path never reaching the merge is
//! what stops its own history looking like a modify/delete conflict on every
//! sync. The merge base is the source commit's first-parent tree, filtered
//! the same way. Pushes the result — fast-forward only, never forced
//! (requirements/0001). Refuses to sync a branch at all if dest's tip carries
//! any commit gitprism didn't put there since its own last push, rather than
//! fast-forwarding a snapshot that would silently drop dest's independent
//! content — that content is exactly what the dest→source phase above
//! already brought back, for the branches configured to round-trip.
//!
//! **dest→source**: for every branch named in `config.branches`, find every
//! dest commit not yet reflected into source by scanning *source's* history
//! for the most recent `Gitprism-Dest-Commit` trailer (decisions/0003) —
//! setup's own graft commit (decisions/0006) always carries one, so this
//! never needs a special-cased first run — then for each pending dest commit
//! merge source's current chain-tip tree (`ours`) against the dest commit's
//! own tree (`theirs`), unfiltered (dest never holds source-only content),
//! and push the result to source's own remote. A real content conflict
//! hard-stops that branch (decisions/0007): whatever merged cleanly before
//! the conflict is still pushed, and the conflicting commit is left for a
//! human to resolve (decisions/0008), retried automatically on the next run
//! once it is. Branches not in `config.branches` (e.g. a transient feature
//! branch) never round-trip this way — decisions/0017's deliberate
//! asymmetry — and no branch is ever deleted on either side.
//!
//! Same discovery convention as `setup` (decisions/0012): `cwd` is a
//! starting point for git-style upward discovery, and a relative `--config`
//! resolves against the discovered root, not the invoking subdirectory. But
//! unlike `setup`, `sync` runs against a repo that already has history — its
//! `.gitprism.toml`/`.gitprismignore` are read from the working tree, which
//! mirrors the committed tree on an ordinary checkout.
//!
//! Split (2026-08-28, see docs/2026-08-27_REPOSITORY_REVIEW.md §10/§11) into
//! this orchestration module (`run`, the main per-branch loop, branch
//! listing, and the build/push helpers) plus [`anchor`] (decisions/0044/0046's
//! dest anchor search), [`marker_scan`] (the shared first-parent marker
//! revwalks), [`local_advance`] (dest→source's local ref preflight and
//! compare-and-swap), [`filter`] (tree filtering), and [`policy_check`]
//! (decisions/0037's per-commit control-file check) — a behaviour-preserving
//! move, not a redesign.

pub(crate) mod anchor;
pub(crate) mod filter;
mod local_advance;
mod mapping_index;
mod marker_scan;
pub(crate) mod policy_check;

use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository, Signature};

use crate::config::Config;
use crate::exclude::{self, ExcludeList};
use crate::git::{self, PushMode};
use crate::limits;
use crate::marker::{self, Direction as MarkerDirection};
use crate::policy;
use crate::progress::{Direction, Outcome, Reporter};

use anchor::{
    DestAnchor, RunCache, dest_anchor_for_branch, dest_ref_exists_cached,
    dest_resume_point_for_branch, mapping_distance_for_branch, mirror_only_rewrite_detected,
};
use filter::{empty_tree, filter_tree};
use local_advance::{advance_local_source_branch, preflight_local_source_branch};
use marker_scan::newest_dest_marker;
use policy_check::{find_control_file_policy_mismatch, policy_mismatch_message};

/// A lost fast-forward race (decisions/0009) is refetched and recomputed
/// from scratch this many times before sync gives up and fails loudly. Exact
/// bound is an implementation detail, not a design fork.
const MAX_RACE_RETRIES: u32 = 3;

/// decisions/0038's operator-facing replacement for the old "kept losing a
/// fast-forward race after N retries" — that message stated the symptom and
/// gave no next step. This names the branch, says the two sides diverged,
/// and hands reconciliation to the operator using ordinary git, deliberately
/// never mentioning merge, rebase, or cherry-pick: choosing among those is
/// the human decision gitprism must not automate (AGENTS.md). `ff_target` is
/// whichever side this push direction was trying to fast-forward ("dest" for
/// source→dest, "source" for dest→source).
fn divergence_after_exhausted_retries_message(branch: &str, ff_target: &str) -> String {
    format!(
        "gitprism sync: dest branch {branch:?} and source have diverged — {MAX_RACE_RETRIES} refetch-and-recompute attempts still couldn't fast-forward {ff_target} onto it; reconcile the two histories with ordinary git, then rerun gitprism sync"
    )
}

/// decisions/0046 addendum, Finding I: memoizes only a branch's `None`
/// distance across scheduling rounds. A branch that could gain a mapping
/// from another branch's successful push already has that push's
/// prerequisite anchor in its own ancestry, and would therefore already
/// have reported a distance — a `None` distance can never become `Some`
/// mid-run, so it is safe to skip recomputing that branch's full-history
/// walk for the rest of this run. A `Some` distance is never cached here:
/// an earlier push in the same run can shorten it before a later round asks
/// again.
fn mapping_distance_with_none_memo(
    branch: &str,
    known_to_have_no_mapping: &mut std::collections::HashSet<String>,
    mut distance_for: impl FnMut(&str) -> Result<Option<usize>>,
) -> Result<Option<usize>> {
    if known_to_have_no_mapping.contains(branch) {
        return Ok(None);
    }
    let distance = distance_for(branch)?;
    if distance.is_none() {
        known_to_have_no_mapping.insert(branch.to_owned());
    }
    Ok(distance)
}

/// One round of decisions/0046's distance-based scheduling: computes each
/// remaining branch's mapping distance exactly once this round (never once
/// per pairwise comparison, as the previous ~2·B² revwalks per run did —
/// see decisions/0046 addendum, Finding C) and selects the minimum by
/// `(has-no-mapping, distance, name)`. This helper still recomputes every
/// remaining branch every round; the caller memoizes `None` results across
/// rounds itself (Finding I) with [`mapping_distance_with_none_memo`],
/// since a `Some` distance can shorten mid-run and must not be cached. A
/// branch whose distance computation itself fails
/// (e.g. Finding A's own-scan-horizon refusal, or any other unexpected git
/// error) is never selected and never aborts the round for every other
/// branch — it's returned instead, for the caller to report as its own
/// per-branch halt (decisions/0045's shape) before trying the next round
/// with it removed. Recomputing every round (rather than caching across
/// rounds) is deliberate: a push earlier in this same run can add a
/// mapping a later branch's distance depends on (decisions/0046).
///
/// Returns `(halted, None)` when at least one branch's distance errored
/// this round — nothing is selected from a round that saw an error, so the
/// caller retries with the halted branches removed. Otherwise returns
/// `(empty, Some(branch))`, or `(empty, None)` only when `remaining` itself
/// is empty.
fn select_next_branch_by_mapping_distance(
    remaining: &[String],
    mut distance_for: impl FnMut(&str) -> Result<Option<usize>>,
) -> (Vec<(String, String)>, Option<String>) {
    let mut keys: Vec<(bool, usize, String)> = Vec::with_capacity(remaining.len());
    let mut halted = Vec::new();
    for branch in remaining {
        match distance_for(branch) {
            Ok(distance) => keys.push((
                distance.is_none(),
                distance.unwrap_or(usize::MAX),
                branch.clone(),
            )),
            Err(error) => halted.push((
                branch.clone(),
                format!("{branch:?} halted — could not determine its mapping distance: {error:#}"),
            )),
        }
    }
    if !halted.is_empty() {
        return (halted, None);
    }
    let selected = keys.into_iter().min().map(|(_, _, branch)| branch);
    (halted, selected)
}

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
    // Repository discovery first, so a wrong cwd reports that, not a
    // missing/malformed state key (F-14) — the pair secret is still
    // validated before fetching or constructing any commits either way.
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism sync must be run inside an existing git repository (none found at or above {}) — has `gitprism setup` been run?",
            cwd.display()
        )
    })?;
    let state_key = marker::load_key()?;
    let source_root = repo
        .workdir()
        .context("gitprism sync requires a repo with a working tree, not a bare repo")?
        .to_path_buf();

    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        source_root.join(config_path)
    };
    let verified_policy = load_run_policy(&config_path, &source_root)?;
    let config = verified_policy.config;
    let exclude_list = verified_policy.exclude_list;
    // Threaded into the source→dest loop below for decisions/0037's
    // per-commit `.gitprismignore` consistency check — the same
    // digest-verified bytes `exclude_list` was already built from, not a
    // re-read. `.gitprism.toml` has no equivalent per-commit check
    // (decisions/0037's amendment): it's verified once, globally, right
    // above by `load_run_policy`, and never re-read per branch or per commit.
    let ignore_raw = verified_policy.ignore_raw;
    let _operation_lock = crate::lock::OperationLock::acquire(&repo)?;

    // Checked once per run, not once per merge (decisions/0016) — an
    // operator on a too-old git gets one clear version message up front
    // instead of a confusing failure the first time some pending commit
    // needs merging.
    git::ensure_merge_tree_supported()?;

    // Listed here — before either phase runs — purely to size the progress
    // bar's total (decisions/0020, point 3): `n = config.branches.len() +
    // source_branches.len()` has to be known from the very first line, not
    // discovered mid-run the way this listing used to happen (immediately
    // before the source→dest loop below, and only there). The listing itself
    // is unchanged, just moved earlier and reused below rather than repeated.
    let (source_branches, skipped_source_branches) = list_source_branches(&repo)?;
    let reporter = Reporter::new(
        config.branches.len() + source_branches.len() + skipped_source_branches.len(),
        config
            .branches
            .iter()
            .map(String::as_str)
            .chain(source_branches.iter().map(String::as_str))
            .chain(
                skipped_source_branches
                    .iter()
                    .map(|skipped| skipped.display_name.as_str()),
            ),
    );

    // Reported up front, before either phase runs, so a per-branch listing
    // problem never gets buried under every branch's own completed line
    // (decisions/0024: a warning, not a whole-run abort — `run()` continues
    // with every branch it *could* list).
    for skipped in &skipped_source_branches {
        reporter.complete(
            Outcome::Warning,
            &skipped.display_name,
            Direction::SourceToDest,
            false,
            Some(&skipped.reason),
        );
    }

    // dest→source first, for every explicitly configured branch: any content
    // dest carries that gitprism didn't itself put there (e.g. a merged PR)
    // must be reflected into source before source→dest's own refusal check
    // below evaluates dest's tip — that check refuses to build on dest
    // content it doesn't recognize, and reflecting it into source is exactly
    // what makes it recognized (see `dest_resume_point`'s third case).
    // Grouping by phase rather than by branch still guarantees this ordering
    // per branch, since discovery above only runs once, before either phase
    // (decisions/0017, decisions/0020).
    let mut run_cache = RunCache::default();
    for branch in &config.branches {
        sync_pair_from_dest_with_key(&repo, &source_root, &config, branch, &reporter, &state_key)
            .with_context(|| format!("syncing {branch:?} dest -> source"))?;
    }

    // source→dest discovers every branch that exists on source at run time
    // (decisions/0017) rather than reading `config.branches` — a brand-new
    // branch needs no config entry to start mirroring. `source_branches` was
    // already listed (and sorted) above, before the dest→source loop, so
    // there's nothing left to (re-)discover here.
    // decisions/0037: a branch whose replay hits a control-file policy
    // mismatch halts (nothing is pushed for it) but doesn't stop the loop —
    // every other branch still gets its turn, matching decisions/0024's
    // precedent for not letting one branch's problem cost every other
    // branch its sync. decisions/0046 extends the same halt-and-continue
    // signal to a discovered branch whose exact mappings are contradictory.
    // Either failure is accumulated instead and turned into a non-zero exit only
    // once every branch has been processed, so CI can't mistake a halted
    // branch for a clean run.
    let mut any_branch_halted = false;
    // decisions/0046: reconstruct exact authenticated mappings after
    // dest→source has advanced local source histories. Destination heads are
    // fetched once and retained in the same per-run cache used below.
    let dest_url = config.dest_url()?;
    let mapping_index = anchor::reconstruct_mapping_index(
        &repo,
        &source_root,
        &dest_url,
        &source_branches,
        &state_key,
        &mut run_cache,
    )?;
    run_cache.mapping_index = mapping_index;

    // One cache for this whole `run()` invocation — destination ref
    // existence, fetched destination tips, and the authenticated mapping
    // index — is shared across every branch below. Select the next branch
    // from the nearest exact mapping so a parent projection is available to
    // its child later in this run (decisions/0046).
    let mut remaining_branches = source_branches.clone();
    let mut branches_with_no_mapping = std::collections::HashSet::new();
    while !remaining_branches.is_empty() {
        let (halted_by_distance_error, selected) =
            select_next_branch_by_mapping_distance(&remaining_branches, |branch| {
                mapping_distance_with_none_memo(branch, &mut branches_with_no_mapping, |branch| {
                    mapping_distance_for_branch(&repo, branch, &run_cache)
                })
            });
        if !halted_by_distance_error.is_empty() {
            // decisions/0045's per-branch halt shape, applied to a distance
            // computation failure too (e.g. Finding A's own-scan-horizon
            // refusal): one branch's problem must not starve every other
            // branch's turn this run.
            for (branch, message) in &halted_by_distance_error {
                let round_tripped = config.branches.iter().any(|b| b == branch);
                reporter.complete(
                    Outcome::Error,
                    branch,
                    Direction::SourceToDest,
                    round_tripped,
                    Some(message),
                );
            }
            any_branch_halted = true;
            remaining_branches.retain(|branch| {
                !halted_by_distance_error
                    .iter()
                    .any(|(halted, _)| halted == branch)
            });
            continue;
        }
        let branch = selected.expect(
            "select_next_branch_by_mapping_distance returns a branch whenever nothing halted \
             and the candidate list is non-empty",
        );
        remaining_branches.retain(|candidate| candidate != &branch);
        let halted = sync_pair_to_dest_with_key(
            &repo,
            &source_root,
            &config,
            &branch,
            &reporter,
            &state_key,
            &exclude_list,
            &ignore_raw,
            &mut run_cache,
        )
        .with_context(|| format!("syncing {branch:?} source -> dest"))?;
        any_branch_halted |= halted;
    }

    // Every branch is accounted for — clear the pinned bar rather than
    // leaving it frozen on whatever its last step message happened to be
    // (decisions/0020's own Context cites clearing transient UI once a step
    // is done). The completed lines already printed above it are the
    // permanent record of the run, not the bar itself. Done before the
    // halted-branch bail below too, for the same reason the conflict
    // hard-stop already clears it before its own `anyhow::bail!`.
    reporter.finish();

    if any_branch_halted {
        anyhow::bail!(
            "gitprism sync: one or more branches halted — a replayed commit's control file disagreed with the pinned policy, a discovered branch had contradictory exact dest mappings, or a discovered branch's existing dest ref wasn't safe to build on — see the branch lines above for the affected commit(s)/branch(es) and remedy"
        );
    }

    Ok(())
}

fn load_run_policy(config_path: &Path, source_root: &Path) -> Result<policy::VerifiedPolicy> {
    policy::load(config_path, &source_root.join(exclude::FILENAME))
}

/// A local branch [`list_source_branches`] could not read as a mirror
/// candidate — its escaped display name (never valid UTF-8, or
/// `validate_branch_name` would have accepted it) plus why it's being
/// skipped, for [`run`] to report as a per-branch [`Outcome::Warning`]
/// (decisions/0024's precedent: an operator-controlled oddity on a
/// discovered branch is a warning that keeps reappearing until fixed, not a
/// whole-run abort).
pub(crate) struct SkippedSourceBranch {
    pub(crate) display_name: String,
    pub(crate) reason: String,
}

/// Every local branch that exists on source right now, sorted for
/// deterministic run order (git2's branch iteration order isn't guaranteed).
/// Read once by [`run`] — before either sync phase starts, purely to size
/// [`Reporter`]'s upfront total (decisions/0020, point 3) — and reused for
/// the source→dest loop rather than listed again. A local, read-only `git
/// branch` enumeration; no fetch involved. A branch whose name isn't valid
/// UTF-8 is reported back via the second element rather than failing the
/// whole listing — decisions/0024's precedent for a branch gitprism
/// discovered but can't act on.
pub(super) fn list_source_branches(
    repo: &Repository,
) -> Result<(Vec<String>, Vec<SkippedSourceBranch>)> {
    let mut source_branches = Vec::new();
    let mut skipped = Vec::new();
    for entry in repo
        .branches(Some(git2::BranchType::Local))
        .context("listing source's local branches")?
    {
        if source_branches.len() >= limits::MAX_SOURCE_BRANCHES {
            anyhow::bail!(
                "source branch enumeration exceeds the {} branch limit",
                limits::MAX_SOURCE_BRANCHES
            );
        }
        let (branch, _) = entry.context("reading a local branch")?;
        let name_bytes = branch
            .name_bytes()
            .context("reading a local branch's name")?;
        let name = match std::str::from_utf8(name_bytes) {
            Ok(name) => name,
            Err(_) => {
                let display_name = git::escape_bytes(name_bytes);
                skipped.push(SkippedSourceBranch {
                    reason: format!(
                        "local branch {display_name} has a non-UTF-8 name gitprism can't mirror — skipped this run"
                    ),
                    display_name,
                });
                continue;
            }
        };
        crate::git::validate_branch_name(name)
            .with_context(|| format!("validating local branch {name:?}"))?;
        source_branches.push(name.to_string());
    }
    source_branches.sort();
    Ok((source_branches, skipped))
}

/// Returns `Ok(true)` if this branch halted with nothing pushed — because a
/// replayed commit's control file disagreed with the pinned policy
/// (decisions/0037), because a discovered branch's exact mappings were
/// contradictory (decisions/0046), or because a discovered branch's existing
/// dest ref isn't a point gitprism recognizes as safe to build on
/// (decisions/0045) —
/// `Ok(false)` for every other outcome (done, skipped, or decisions/0024's
/// warning). The equivalent refusal for a *round-tripped* (`config.branches`)
/// branch stays a fatal `anyhow::bail!`, unaffected by decisions/0045: that
/// branch class really can have independent dest content decisions/0017
/// promises to reflect back, so an operator must resolve the divergence
/// before every other branch's sync is attempted.
#[allow(clippy::too_many_arguments)]
fn sync_pair_to_dest_with_key(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    reporter: &Reporter,
    state_key: &marker::StateKey,
    exclude_list: &ExcludeList,
    ignore_raw: &str,
    run_cache: &mut RunCache,
) -> Result<bool> {
    git::validate_branch_name(branch)
        .with_context(|| format!("validating source branch {branch:?}"))?;
    // decisions/0020: cyan in a completed line iff round-tripped (named in
    // `config.branches`) — computed once here and reused for every
    // `reporter` call this function makes, rather than re-derived at each
    // one.
    let round_tripped = config.branches.iter().any(|b| b == branch);

    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {branch:?}"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
        .id();

    let dest_url = config.dest_url()?;

    let mut attempt = 0;
    loop {
        // decisions/0017: `branch` was discovered on source, not read from
        // config, so unlike every branch `setup` has grafted, it may have no
        // same-named counterpart on dest at all yet (a brand-new feature
        // branch, say) — checked explicitly rather than attempting a fetch
        // and treating "no such ref" as the same failure it would be for a
        // branch that's supposed to already exist. Read through the same
        // per-run cache decisions/0046's anchor lookup shares (see
        // `dest_ref_exists_cached`).
        let dest_ref_exists = dest_ref_exists_cached(source_root, &dest_url, branch, run_cache)?;
        // decisions/0038, decisions/0039: force is requested only once this
        // very run has established both that `branch` is mirror-only and
        // that it positively identified a source-side rewrite below — the
        // one narrow authority this project's operator-intervention default
        // (AGENTS.md) permits gitprism to override on its own. Every other
        // path through this loop leaves it at the fast-forward-only default.
        let mut push_mode = PushMode::FastForwardOnly;

        let (dest_tip, boundary) = if dest_ref_exists {
            if round_tripped {
                reporter.step(
                    branch,
                    Direction::SourceToDest,
                    "fetching dest (finding resume point before merging from source)",
                );
            } else {
                reporter.step(
                    branch,
                    Direction::SourceToDest,
                    "fetching dest (mirror-only branch, not round-tripped)",
                );
            }
            // Deliberately its own fetch, not read from a cache: decisions/0040
            // requires a `ForceMirrorOnly` lease built from the dest tip as
            // actually fetched by *this* attempt, and the race-retry loop
            // below must refetch after a `RejectedRefMoved` rejection —
            // reusing a run-wide cached tip here would build a lease against
            // a tip that may already be stale (decisions/0046 Addendum 2,
            // Finding K).
            git::fetch(source_root, &dest_url, branch).with_context(|| {
                format!("fetching dest branch {branch:?} from configured remote")
            })?;
            // decisions/0040: this is the OID a `ForceMirrorOnly` lease must
            // be built from — the dest tip as actually fetched this run, not
            // the graft-derived rebuild base the rewrite arm below binds
            // `dest_tip` to after this `match`. Named distinctly so that
            // substitution can't happen by accident.
            let fetched_dest_tip = repo
                .find_reference("FETCH_HEAD")
                .context("reading FETCH_HEAD after fetch")?
                .peel_to_commit()
                .context("resolving fetched dest branch to a commit")?
                .id();

            // There is no safe way to build a new commit straight from
            // source's filtered snapshot and fast-forward dest onto it
            // unless this clone's source_tip is known to be caught up with
            // whatever dest last synced from — either because dest carries
            // independent content gitprism hasn't reflected into source yet
            // (dest→source, run just above in `run`, normally handles this
            // before we ever get here), or because this clone's own source
            // branch is behind or diverged from the source commit dest was
            // actually last synced from (e.g. another clone already pushed
            // for this branch). Either way, proceeding could silently drop
            // content some other commit already contributed, even though the
            // ref update itself would be a legitimate fast-forward.
            match dest_resume_point_for_branch(
                repo,
                source_tip,
                fetched_dest_tip,
                branch,
                state_key,
            )? {
                Some(boundary) => (fetched_dest_tip, boundary),
                // decisions/0039: `round_tripped` is the authority invariant
                // itself — a mirror-only branch's dest ref is a projection
                // nothing ever imports back (decisions/0017), so nothing on
                // it is dest's own independent contribution gitprism is
                // obligated to preserve. That is the *only* thing that
                // licenses discarding it here; a round-tripped branch never
                // reaches this arm regardless of how the four conditions
                // otherwise line up, and falls through to the ordinary
                // refusal below instead.
                None if !round_tripped
                    && mirror_only_rewrite_detected(
                        repo,
                        source_tip,
                        fetched_dest_tip,
                        branch,
                        state_key,
                    )? =>
                {
                    reporter.step(
                        branch,
                        Direction::SourceToDest,
                        "source branch was rewritten (mirror-only) — rebuilding dest's projection from the shared graft",
                    );
                    // Reuses the same graft-derived base the `!dest_ref_exists`
                    // arm below already computes this way — a detected
                    // rewrite means dest's own ref can no longer answer "what
                    // does source's ancestry say the boundary is," which is
                    // exactly the condition that arm was already built for.
                    let (boundary, rebuild_dest_tip) = match dest_anchor_for_branch(
                        repo, source_tip, branch, run_cache,
                    )? {
                        DestAnchor::Resolved(boundary, dest_tip) => (boundary, dest_tip),
                        DestAnchor::Contradictory(diagnostic) => {
                            reporter.complete(
                                Outcome::Error,
                                branch,
                                Direction::SourceToDest,
                                round_tripped,
                                Some(&format!("{branch:?} halted — {diagnostic}")),
                            );
                            return Ok(true);
                        }
                        DestAnchor::None => anyhow::bail!(
                            "gitprism sync: detected a rewritten mirror-only branch {branch:?} but found no Gitprism-Dest-Commit trailer to rebuild from — has `gitprism setup` been run for this pair?"
                        ),
                    };
                    // The lease is a compare-and-swap against dest's actual
                    // fetched tip, never against `rebuild_dest_tip` — a
                    // concurrent advance past `fetched_dest_tip` is a race
                    // this run hasn't seen yet, not part of the authorized
                    // rewrite (decisions/0040).
                    push_mode = PushMode::ForceMirrorOnly {
                        expected_dest: fetched_dest_tip,
                    };
                    (rebuild_dest_tip, boundary)
                }
                None if round_tripped => anyhow::bail!(
                    "gitprism sync: dest branch {branch:?} isn't at a point this clone can safely build on — either dest→source hasn't reflected its content into source yet, or this clone's {branch:?} is behind or diverged from what dest was last synced from (fetch/pull the latest source history first)"
                ),
                // decisions/0045: per-branch halt for a discovered branch.
                None => {
                    reporter.complete(
                        Outcome::Error,
                        branch,
                        Direction::SourceToDest,
                        round_tripped,
                        Some(&unsafe_to_build_on_message(branch)),
                    );
                    return Ok(true);
                }
            }
        } else {
            // No dest ref to be unsafe about yet, so no safety check applies
            // either — this branch's own ancestry already carries dest
            // content, inherited from whichever branch it was created from
            // (typically a branch `setup` grafted), so the nearest
            // authenticated mapping reachable from `source_tip` names both
            // the dest-space tree to build the new chain onto and the
            // source-space boundary `pending_commits` should resume from —
            // the same graft-derived ancestry decisions/0006 established,
            // just read directly from the per-run mapping index instead of
            // from a dest ref that doesn't exist. But unlike a
            // `config.branches` entry, nothing guarantees this discovered
            // branch (decisions/0017) shares any ancestry with dest at all —
            // handled below (decisions/0024) the same way
            // `already_merged_into_a_landing_branch`'s skip further below
            // handles decisions/0018's Case 2, not decisions/0023's setup-time
            // hard-fail for the superficially similar "no merge-base"
            // shape: an operator never asked gitprism to manage a branch
            // decisions/0017 merely discovered, so one such branch warns and
            // the run moves on rather than stopping every other branch too.
            let (boundary, dest_tip) = match dest_anchor_for_branch(
                repo, source_tip, branch, run_cache,
            )? {
                DestAnchor::Resolved(boundary, dest_tip) => (boundary, dest_tip),
                DestAnchor::Contradictory(diagnostic) => {
                    reporter.complete(
                        Outcome::Error,
                        branch,
                        Direction::SourceToDest,
                        round_tripped,
                        Some(&format!("{branch:?} halted — {diagnostic}")),
                    );
                    return Ok(true);
                }
                // decisions/0024: a genuinely pre-existing, unrelated local
                // branch (e.g. decisions/0021's `ai-setup` example) has no
                // `Gitprism-Dest-Commit` trailer anywhere in its first-parent
                // history — gitprism still won't guess at joining unrelated
                // histories (decisions/0007), but reports it as a warning and
                // continues rather than aborting the whole run. Distinct from
                // the `Outcome::Skipped` below: this branch will keep
                // reappearing every run until an operator acts on it, unlike
                // that genuinely benign, one-time no-op. Note this check now
                // precedes that skip (decisions/0037 moved the already-merged
                // classification after boundary resolution so the policy
                // pre-pass could use the same boundary), so a branch with no
                // marker at all reports this warning even where the older
                // ordering would have recognized it as already merged.
                DestAnchor::None => {
                    reporter.complete(
                        Outcome::Warning,
                        branch,
                        Direction::SourceToDest,
                        round_tripped,
                        Some(&format!(
                            "{branch:?} has no shared history with anything `gitprism setup` or a prior sync ever produced — combining unrelated histories is a manual `git merge --allow-unrelated-histories` job, not something gitprism will do"
                        )),
                    );
                    return Ok(false);
                }
            };
            (dest_tip, boundary)
        };

        // decisions/0037: before building or pushing anything for this
        // branch, every commit the replay would actually apply — same
        // boundary/tip `build_pending_dest_tip` below is about to use — is
        // checked for a `.gitprismignore` that's present but disagrees with
        // the pinned policy (its amendment excludes `.gitprism.toml` from
        // this per-commit check; see `find_control_file_policy_mismatch`).
        // Absence isn't a mismatch (the trusted policy already governs the
        // commit regardless of its own tree), and this runs *before*
        // decisions/0018's "already merged" classification just below so a
        // control-file-only branch that would otherwise filter to a no-op
        // halts loudly instead of being silently read as
        // already-merged-and-cleaned-up.
        let pending_for_policy_check = pending_commits(repo, boundary, source_tip)?;
        if let Some(mismatch) = find_control_file_policy_mismatch(
            repo,
            &pending_for_policy_check,
            state_key,
            ignore_raw,
        )? {
            reporter.complete(
                Outcome::Error,
                branch,
                Direction::SourceToDest,
                round_tripped,
                Some(&policy_mismatch_message(branch, &mismatch)),
            );
            // Per-branch halt, not a run-wide one (decisions/0037 contrasts
            // itself with decisions/0007's conflict hard-stop): nothing for
            // this branch is pushed, but `run` still processes every other
            // branch and only fails the overall invocation once all of them
            // are done.
            return Ok(true);
        }

        // decisions/0018, Case 2: a mirror-only branch with no dest ref may
        // never have been synced yet, or it may have been synced, merged into
        // a round-tripped branch via an ordinary PR, and had its now-merged
        // mirror deleted on dest as routine cleanup — indistinguishable from
        // "never synced" by ref/ancestry alone. Checked content-first, with no
        // persisted state, before ever rebuilding anything: if `branch`'s
        // content is already fully present in one of `config.branches`'s
        // current tips, its absence on dest is expected, not something to
        // resurrect (GitLab's own push-mirror does the same for its mirrors).
        if !dest_ref_exists
            && !round_tripped
            && let Some(landing) = already_merged_into_a_landing_branch(
                repo,
                config,
                source_tip,
                source_root,
                exclude_list,
            )?
        {
            reporter.complete(
                Outcome::Skipped,
                branch,
                Direction::SourceToDest,
                round_tripped,
                // "(expected for a mirror-only branch)" used to be spelled
                // out here, but the completed line's own color already
                // conveys round-trip vs. mirror-only (decisions/0020) — the
                // note is for the reason, not a restatement of what the
                // color already showed.
                Some(&mirror_only_skip_note(&landing)),
            );
            return Ok(false);
        }

        let build = build_pending_dest_tip(
            repo,
            config,
            exclude_list,
            boundary,
            dest_tip,
            source_tip,
            source_root,
            branch,
            state_key,
        )?;

        // `build.new_tip` is `None` in three cases: the branch has nothing
        // new to merge onto an existing dest ref (a genuine no-op — no push
        // at all, the `None` arm below); it's a brand-new branch with no
        // commits of its own beyond whatever graft/marker point it shares
        // with dest (decisions/0017: still has to be created on dest); or a
        // detected rewrite's replacement commits built nothing at all —
        // either `pending_commits` was empty (source was reset straight back
        // to a commit already carrying a marker) or every pending commit
        // filtered to no change against the rebuild base. The other two
        // still need something pushed — for a fresh branch `dest_tip` is
        // already the right content, just missing a ref name, and for a
        // detected rewrite `dest_tip` here is `rebuild_dest_tip`, the
        // graft-derived base dest must be rewound to (decisions/0039's
        // addendum: a rewrite that rebuilds to the base is still a rewrite,
        // not a no-op, even when the base and the built chain happen to
        // coincide). `branch_scoped_dest_tip` (decisions/0044) decides
        // whether `dest_tip` can be used as-is or needs this branch's own
        // marker commit on top.
        let force_rebuild = matches!(push_mode, PushMode::ForceMirrorOnly { .. });
        let new_dest_tip = match build.new_tip {
            Some(tip) => Some(tip),
            None if force_rebuild || !dest_ref_exists => Some(branch_scoped_dest_tip(
                repo, config, dest_tip, source_tip, branch, state_key,
            )?),
            None => None,
        };

        if let Some(new_dest_tip) = new_dest_tip {
            // All fallible ancestry work needed to discard the replaced
            // branch's stale mappings happens before the remote mutation.
            // Applying the resulting plan after an accepted push is
            // deliberately infallible, so cache maintenance can never turn
            // a successful force-with-lease into a failed sync.
            let invalidation_plan = if force_rebuild {
                Some(run_cache.mapping_index.plan_replaced_chain_invalidation(
                    repo,
                    branch,
                    new_dest_tip,
                )?)
            } else {
                None
            };
            match git::push(source_root, &dest_url, new_dest_tip, branch, push_mode)? {
                git::PushOutcome::Accepted => {
                    // The destination ref is visible to later branch anchor
                    // lookups this run, with no re-query.
                    run_cache.dest_ref_exists.insert(branch.to_string(), true);
                    // decisions/0046, F-A: a `ForceMirrorOnly` push just
                    // replaced `branch`'s own dest chain wholesale — any
                    // mapping this run recorded from that replaced chain
                    // whose dest commit didn't survive into `new_dest_tip`'s
                    // own ancestry is now a mapping to an orphan. Left alone,
                    // a branch scheduled later in this same run could anchor
                    // on it and push straight back onto history this run
                    // itself just discarded. A mapping for the same source
                    // commit recorded via some other branch's own markers is
                    // untouched — this only clears `branch`'s own stale
                    // entries. Done before recording this push's own new
                    // mappings below, though the order doesn't matter: they
                    // are, by construction, already reachable from
                    // `new_dest_tip`.
                    if force_rebuild {
                        run_cache.mapping_index.apply_replaced_chain_invalidation(
                            invalidation_plan
                                .as_deref()
                                .expect("force rebuild always planned invalidation"),
                        );
                    }
                    // F-C: `build_dest_commit` already returned the exact
                    // (source, dest) pair for each of these — recorded
                    // straight into the index, with no re-read or
                    // re-HMAC-verify of a commit this very call just
                    // authored.
                    for &(source_oid, generated_dest_oid) in &build.generated_mappings {
                        run_cache.mapping_index.record_built_mapping(
                            branch,
                            source_oid,
                            generated_dest_oid,
                        );
                    }
                    // When `build.new_tip` is `Some`, `new_dest_tip` is
                    // exactly the last mapping just recorded above — adding
                    // it again would only be rediscovered and discarded by
                    // `record`'s own dedup. The remaining cases (a brand-new
                    // branch or a rebuild reusing `dest_tip` unchanged, or
                    // `branch_scoped_dest_tip`'s own possibly-aliased marker
                    // commit) didn't come from this call's build loop at
                    // all, so they still go through the verifying,
                    // alias-canonicalizing path.
                    if build.new_tip.is_none() {
                        run_cache.mapping_index.record_pushed_dest_commit(
                            repo,
                            branch,
                            new_dest_tip,
                            state_key,
                        );
                    }
                }
                git::PushOutcome::RejectedRefMoved if attempt < MAX_RACE_RETRIES => {
                    // dest's tip moved between fetch and push — refetch and
                    // recompute against its new state rather than rebasing
                    // what was already built (decisions/0009). A
                    // `ForceMirrorOnly` push routinely lands here too: its
                    // `--force-with-lease` is a compare-and-swap against the
                    // dest tip this run actually fetched (decisions/0040), so
                    // another writer advancing dest in the fetch-to-push
                    // window reports the identical rejection, not a silently
                    // overwritten ref. Either way, a rewritten mirror-only
                    // branch is re-detected fresh next iteration rather than
                    // assumed from this rejection — force is never escalated
                    // to from a retry count.
                    //
                    // A rejection proves dest's ref for `branch` moved, which
                    // for the `!dest_ref_exists` case can only mean another
                    // writer just created it — the top-of-loop cache read
                    // that produced this attempt's stale `false` must not
                    // survive into the next iteration, or it would keep
                    // retrying the brand-new-branch path against a branch
                    // that now has a dest ref.
                    run_cache.dest_ref_exists.remove(branch);
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedRefMoved => {
                    anyhow::bail!(divergence_after_exhausted_retries_message(branch, "dest"))
                }
            }
        }

        if let Some(conflict) = build.conflict {
            reporter.complete(
                Outcome::Error,
                branch,
                Direction::SourceToDest,
                round_tripped,
                Some(&format!(
                    "hit a real conflict at source commit {} in {:?} — resolve it with `gitprism resolve <branch> --direction source-to-dest`; commits before it were still pushed to dest's {branch:?} branch",
                    conflict.commit, conflict.paths
                )),
            );
            // Decisions/0007's hard-stop ends this run right here — clear
            // the pinned bar rather than leaving it frozen mid-step above
            // the error text `anyhow::bail!` is about to print, which would
            // otherwise read as "the run hung," not "the run errored."
            reporter.finish();
            anyhow::bail!(
                "gitprism sync: {branch:?} <- {branch:?} hit a real conflict at source commit {} in {:?} — resolve it with `gitprism resolve <branch> --direction source-to-dest`; commits before it were still pushed to dest's {branch:?} branch",
                conflict.commit,
                conflict.paths
            );
        }

        reporter.complete(
            if new_dest_tip.is_some() {
                Outcome::Done
            } else {
                Outcome::Skipped
            },
            branch,
            Direction::SourceToDest,
            round_tripped,
            if new_dest_tip.is_none() {
                Some("up to date, nothing to sync")
            } else if force_rebuild && build.new_tip.is_none() && new_dest_tip == Some(dest_tip) {
                // Honest about what actually happened: dest's ref moved, but
                // to the shared rebuild base itself, not to a newly built
                // commit — must not read as "N new commits pushed" when none
                // were.
                Some("rebuilt from the shared graft; no new commits were needed")
            } else if force_rebuild && build.new_tip.is_none() {
                Some(
                    "rebuilt from the shared graft with a branch-scoped marker commit; no source content changed",
                )
            } else {
                None
            },
        );
        return Ok(false);
    }
}

/// decisions/0045: refusal text for a *discovered* branch. Unlike the
/// round-tripped message, it cannot blame dest→source, which never runs for
/// a branch outside `config.branches`.
fn unsafe_to_build_on_message(branch: &str) -> String {
    format!(
        "{branch:?} isn't at a point this clone can safely build on — its dest history and \
         this clone's source history for {branch:?} share no ancestry gitprism recognizes \
         (a dest-native branch of the same name with genuinely unrelated history, or this \
         clone is behind); fetch/pull the latest source history first, or reconcile the \
         branches manually if their histories are genuinely unrelated"
    )
}

/// decisions/0018 Case 2's skip note: `already_merged_into_a_landing_branch`
/// only ever tells us the branch's filtered content is already fully present
/// in `landing` — it cannot tell a branch genuinely merged via a PR and
/// cleaned up on dest apart from one that never had a dest ref at all
/// (decisions/0018's own "indistinguishable by ref/ancestry alone"). The note
/// must therefore state only that observation, not assert deletion or prior
/// existence on dest.
fn mirror_only_skip_note(landing: &str) -> String {
    format!("its filtered content is already fully present in {landing:?}")
}

/// The first commit in a direction's pending list that couldn't be merged
/// cleanly, plus the paths git reported as conflicted — one shape for both
/// directions, since both now go through the same `git merge-tree` primitive
/// and therefore cannot disagree about what a conflict is (decisions/0007,
/// decisions/0016).
struct Conflict {
    commit: Oid,
    paths: Vec<String>,
}

/// The result of [`build_pending_dest_tip`]: `new_tip` is the chain's tip if
/// anything applied cleanly (`None` if nothing was pending, or every pending
/// commit was either loop-prevented or merged to a no-op), and `conflict`
/// names the first source commit that couldn't be merged cleanly onto dest,
/// if any (decisions/0007, decisions/0016) — processing always stops there
/// (decisions/0007's "Consequences": later commits may depend on it).
struct PendingDestBuild {
    new_tip: Option<Oid>,
    /// `(source oid, dest oid)` for every commit actually built this call —
    /// `build_dest_commit`'s own trusted output, recorded into the mapping
    /// index directly on push acceptance rather than re-read and
    /// re-verified from the repo (decisions/0046, F-C).
    generated_mappings: Vec<(Oid, Oid)>,
    conflict: Option<Conflict>,
}

/// Builds, in `repo`'s object database, a chain of new commits reflecting
/// every source commit between `boundary` and `source_tip`, each merged onto
/// dest's current chain tip via a real `git merge-tree` subprocess
/// (decisions/0016) — not a full-tree snapshot replace, which would silently
/// regress any independent dest content a not-yet-processed dest→source
/// cherry-pick already landed further up source's history.
///
/// The merge base is always `source_commit.parent(0)`'s tree — the mainline
/// parent for a merge commit — filtered the same way `theirs` is, per
/// decisions/0016's table; no `parent_count` special-casing is needed at all,
/// so octopus merges fall out of the same rule for free. This replaces
/// decisions/0014's source-space cursor entirely: that cursor existed only to
/// work around `apply_to_tree` patch application not being idempotent (a
/// repeated add duplicated instead of no-op'ing), which is exactly what
/// solving the problem with a real 3-way merge makes unnecessary — one
/// mechanism per property, instead of two mechanisms for the same one, is how
/// the duplication and mid-chain-stranding bugs that motivated decisions/0016
/// stop being possible. Stops at the first commit that doesn't merge cleanly
/// (decisions/0007). `dest_tip` seeds the chain's first parent.
/// Loop prevention (decisions/0003): whether `commit` already exists on
/// dest, so replaying it would loop — a `Setup` graft (exempt from the
/// branch check by construction) or any `DestToSource` marker, verified
/// against its own recorded branch rather than the branch currently being
/// synced.
///
/// Widened by decisions/0043's addendum (F-04), retained by decisions/0046:
/// a `DestToSource` marker is written once, scoped to whichever branch's
/// dest→source sync imported it, but once that commit is an ancestor of a
/// *different* branch's tip (e.g. a branch forked after the import), the
/// dest-native commit it names is already on dest regardless of which
/// branch originally imported it — hence the self-verification via
/// `marker::verify_self` (decisions/0046 addendum, Finding L), which never
/// takes a branch to check against in the first place.
pub(super) fn loop_prevented(commit: &git2::Commit, key: &marker::StateKey) -> bool {
    // `Setup` is exempt from the branch check by construction, and a
    // `DestToSource` marker inherited onto some other branch's history
    // verifies against its own recorded branch, not the branch being
    // scanned (decisions/0046 addendum, Finding L) — exactly what
    // `verify_self` already does, so no `branch` argument is needed here.
    marker::verify_self(
        commit,
        &[MarkerDirection::Setup, MarkerDirection::DestToSource],
        None,
        key,
    )
    .is_some()
}

#[allow(clippy::too_many_arguments)]
fn build_pending_dest_tip(
    repo: &Repository,
    config: &Config,
    exclude_list: &ExcludeList,
    boundary: Oid,
    dest_tip: Oid,
    source_tip: Oid,
    source_root: &Path,
    branch: &str,
    key: &marker::StateKey,
) -> Result<PendingDestBuild> {
    let pending = pending_commits(repo, boundary, source_tip)?;

    let mut parent = dest_tip;
    let mut built_any = false;
    let mut generated_mappings = Vec::new();
    for source_oid in pending {
        let source_commit = repo
            .find_commit(source_oid)
            .context("resolving a pending source commit")?;

        // Loop prevention (decisions/0003, widened by decisions/0043's
        // addendum) — see `loop_prevented`. First thing in the loop now that
        // there's no cursor left to advance before it.
        if loop_prevented(&source_commit, key) {
            continue;
        }

        let parent_commit = repo
            .find_commit(parent)
            .context("resolving the in-progress dest chain's parent")?;

        let base_tree = match source_commit.parent(0) {
            Ok(base_commit) => {
                filter_tree(repo, &base_commit.tree()?, Path::new(""), exclude_list)?
            }
            Err(_) => empty_tree(repo)?,
        };
        let theirs_tree = filter_tree(
            repo,
            &source_commit
                .tree()
                .context("reading a pending source commit's tree")?,
            Path::new(""),
            exclude_list,
        )?;

        match git::merge_tree(source_root, base_tree, parent_commit.tree_id(), theirs_tree)? {
            git::MergeTreeOutcome::Conflict { paths } => {
                return Ok(PendingDestBuild {
                    new_tip: built_any.then_some(parent),
                    generated_mappings,
                    conflict: Some(Conflict {
                        commit: source_oid,
                        paths,
                    }),
                });
            }
            git::MergeTreeOutcome::Clean(merged) => {
                // This commit's merge, once filtered, changed nothing dest-
                // side (e.g. it only touched excluded paths, or dest already
                // independently has the same content) — must not push an
                // empty commit (requirements/0001).
                if merged == parent_commit.tree_id() {
                    continue;
                }

                parent =
                    build_dest_commit(repo, config, parent, &source_commit, merged, branch, key)?;
                generated_mappings.push((source_oid, parent));
                built_any = true;
            }
        }
    }

    Ok(PendingDestBuild {
        new_tip: built_any.then_some(parent),
        generated_mappings,
        conflict: None,
    })
}

/// Whether `branch_tip`'s content is already fully merged into any of
/// `config.branches`'s current local source-side tips (decisions/0018, Case
/// 2) — content-based, via the same `git merge-tree` primitive decisions/0016
/// already uses, not oid ancestry, so a squash merge is recognized just as
/// well as a real merge or a rebase/fast-forward (a squash merge's result has
/// no ordinary ancestor relationship to the branch it came from at all).
///
/// All three trees are filtered through `exclude_list` before comparison
/// (decisions/0018 addendum), the same as every other cross-side content
/// comparison in this file (e.g. [`build_pending_dest_tip`]) — dest only ever
/// sees the filtered subset of source, so the question this function has to
/// ask is "is everything dest would ever see from this branch already in the
/// landing branch," not "is 100% of this branch's raw source content already
/// there." An excluded path touched on `branch` alongside ordinary mirrored
/// changes is routine in a source-is-a-superset repo, not evidence of
/// genuinely unmerged content — comparing raw trees would see that excluded
/// path as content the landing branch never received and wrongly conclude
/// "not merged," resurrecting the branch on dest every run.
///
/// Safe to read each landing branch's *current* tip here because `run`
/// finishes dest→source for every `config.branches` entry before source→dest
/// ever discovers a branch (decisions/0017's phase ordering) — those tips are
/// as fresh as this run makes them, not stale from before this run started.
///
/// Returns the name of the first landing branch `branch_tip` is already fully
/// merged into, if any. No state is written or read anywhere for this — every
/// answer comes from the current object graph, recomputed from scratch each
/// call, the same shape git-trim's own "merged vs. stray" classification
/// uses.
fn already_merged_into_a_landing_branch(
    repo: &Repository,
    config: &Config,
    branch_tip: Oid,
    source_root: &Path,
    exclude_list: &ExcludeList,
) -> Result<Option<String>> {
    for landing in &config.branches {
        // A landing branch named in config that doesn't (yet) exist on source
        // isn't something to compare against — nothing for `branch_tip` to
        // have been merged into.
        let Ok(landing_ref) = repo.find_branch(landing, git2::BranchType::Local) else {
            continue;
        };
        let landing_tip = landing_ref
            .get()
            .peel_to_commit()
            .with_context(|| format!("resolving landing branch {landing:?} to a commit"))?
            .id();

        // No shared history at all between this branch and the landing
        // branch — nothing a 3-way merge can evaluate, so this landing branch
        // has nothing to say about whether `branch_tip` is merged.
        let merge_base = match repo.merge_base(branch_tip, landing_tip) {
            Ok(oid) => oid,
            Err(error) if error.code() == git2::ErrorCode::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::from(error).context(format!(
                    "finding a merge base between this branch and landing branch {landing:?}"
                )));
            }
        };

        // `branch_tip` has no commits of its own beyond where it diverged
        // from `landing` at all (e.g. a branch just created off it, decisions
        // /0017's "no commits of its own yet" case) — trivially identical in
        // content to `landing`, but that's "hasn't diverged yet," not
        // "already merged and cleaned up." Without this guard a brand-new,
        // never-synced branch would be wrongly treated as already merged
        // (caught by the existing
        // `run_mirrors_an_ad_hoc_branch_with_no_commits_of_its_own`
        // regression test).
        if branch_tip == merge_base {
            continue;
        }

        let base_tree = filter_tree(
            repo,
            &repo
                .find_commit(merge_base)
                .context("resolving a landing branch's merge-base commit")?
                .tree()
                .context("reading a landing branch's merge-base tree")?,
            Path::new(""),
            exclude_list,
        )?;
        let landing_tree = filter_tree(
            repo,
            &repo
                .find_commit(landing_tip)
                .context("resolving a landing branch's tip commit")?
                .tree()
                .context("reading a landing branch's tip tree")?,
            Path::new(""),
            exclude_list,
        )?;
        let branch_tree = filter_tree(
            repo,
            &repo
                .find_commit(branch_tip)
                .context("resolving a mirror-only branch's tip commit")?
                .tree()
                .context("reading a mirror-only branch's tip tree")?,
            Path::new(""),
            exclude_list,
        )?;

        if let git::MergeTreeOutcome::Clean(merged) =
            git::merge_tree(source_root, base_tree, landing_tree, branch_tree)?
            && merged == landing_tree
        {
            return Ok(Some(landing.clone()));
        }
    }

    Ok(None)
}

/// Every commit strictly after `boundary` up to and including `tip`, oldest
/// first — the order later commits may depend on must be preserved
/// (decisions/0007's "Consequences"). Used for both directions: `tip` is
/// source's tip when walking what's pending for dest, or dest's tip when
/// walking what's pending for source — the walk itself doesn't care which
/// repo-side branch it's scoped to (decisions/0005: the ref you scan already
/// supplies that context).
pub(crate) fn pending_commits(repo: &Repository, boundary: Oid, tip: Oid) -> Result<Vec<Oid>> {
    let mut revwalk = repo.revwalk().context("starting a pending-commit walk")?;
    revwalk
        .push(tip)
        .context("seeding the pending-commit walk")?;
    revwalk
        .hide(boundary)
        .context("excluding already-synced history")?;
    revwalk
        .simplify_first_parent()
        .context("restricting the pending-commit walk to first-parent history (decisions/0035)")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .context("ordering pending commits oldest-first")?;

    let mut pending = Vec::new();
    for oid in revwalk {
        if pending.len() >= limits::MAX_PENDING_COMMITS {
            anyhow::bail!(
                "pending-commit history exceeds the {} commit limit",
                limits::MAX_PENDING_COMMITS
            );
        }
        pending.push(oid.context("walking pending commits")?);
    }
    Ok(pending)
}

fn validated_commit_message<'a, 'b>(commit: &'a git2::Commit<'b>) -> Result<&'a str> {
    if commit.message_bytes().len() > limits::MAX_COMMIT_MESSAGE_BYTES {
        anyhow::bail!(
            "commit {} message exceeds the {} byte limit",
            commit.id(),
            limits::MAX_COMMIT_MESSAGE_BYTES
        );
    }
    std::str::from_utf8(commit.message_bytes()).with_context(|| {
        format!(
            "commit {} has a non-UTF-8 message; gitprism refuses to replace it with a lossy or empty message",
            commit.id()
        )
    })
}

/// Builds one new dest-bound commit in `repo`'s object database — object
/// only, no ref update, since the chain is pushed by oid once it's complete.
/// Preserves the original author, stamps gitprism's own committer identity
/// (decisions/0010), and carries the `Gitprism-Source-Commit` trailer
/// (decisions/0003) that lets a future sync resume from here.
pub(crate) fn build_dest_commit(
    repo: &Repository,
    config: &Config,
    parent: Oid,
    source_commit: &git2::Commit,
    filtered_tree_oid: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Oid> {
    let parent_commit = repo
        .find_commit(parent)
        .context("resolving the dest chain's parent commit")?;
    let tree = repo
        .find_tree(filtered_tree_oid)
        .context("reading the filtered tree")?;
    let committer = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = marker::build_message(
        validated_commit_message(source_commit)?,
        MarkerDirection::SourceToDest,
        branch,
        source_commit.id(),
        "Gitprism-Source-Commit",
        &[parent],
        tree.id(),
        &source_commit.author(),
        &committer,
        key,
    );

    repo.commit(
        None,
        &source_commit.author(),
        &committer,
        &message,
        &tree,
        &[&parent_commit],
    )
    .with_context(|| {
        format!(
            "building dest commit for source commit {}",
            source_commit.id()
        )
    })
}

/// decisions/0044: the tip to create/rebuild a dest ref at when nothing was
/// built — `dest_tip` itself if [`anchor::dest_tip_is_accounted_for`]
/// recognizes it for *this* branch, otherwise a content-empty marker commit
/// on top of it. An inherited anchor carries another branch's marker, so the
/// branch gets its own content-empty marker when branch accounting requires
/// it.
fn branch_scoped_dest_tip(
    repo: &Repository,
    config: &Config,
    dest_tip: Oid,
    source_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Oid> {
    if anchor::dest_tip_is_accounted_for(repo, source_tip, dest_tip, branch, key)? {
        return Ok(dest_tip);
    }
    let source_commit = repo
        .find_commit(source_tip)
        .context("resolving source's tip for a branch-scoped marker commit")?;
    let tree_id = repo
        .find_commit(dest_tip)
        .context("resolving dest's anchor commit for a branch-scoped marker commit")?
        .tree_id();
    build_dest_commit(repo, config, dest_tip, &source_commit, tree_id, branch, key)
}

/// Cherry-picks dest's pending commits on `branch` onto source's same-named
/// branch and pushes the result to source's own remote (decisions/0013), one
/// branch at a time — `branch` always comes from `config.branches`
/// (decisions/0017), the explicit, small set of branches dest content is
/// ported back for. A real conflict hard-stops this branch (decisions/0007):
/// whatever applied cleanly before it is still pushed, and the conflict is
/// reported with enough detail for `gitprism resolve` (decisions/0008) to act
/// on later — no trailer is written for the unresolved commit, so the next
/// run's resume-scan naturally retries it once it's resolved.
fn sync_pair_from_dest_with_key(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    reporter: &Reporter,
    state_key: &marker::StateKey,
) -> Result<()> {
    git::validate_branch_name(branch)
        .with_context(|| format!("validating configured branch {branch:?}"))?;
    let dest_url = config.dest_url()?;

    let mut attempt = 0;
    loop {
        // `branch` is always named in `config.branches`, so unlike a
        // discovered mirror-only branch's first sync (decisions/0017,
        // `sync_pair_to_dest`'s own `remote_ref_exists` check), there is no
        // legitimate reason for it to have no ref on dest at all — `gitprism
        // setup` (decisions/0006) always grafts every round-tripped branch.
        // Checked before fetching so a deleted dest ref fails with a clear,
        // gitprism-authored message (decisions/0018) instead of git's own raw
        // "couldn't find remote ref" subprocess error aborting the run.
        if !git::remote_ref_exists(source_root, &dest_url, branch)? {
            anyhow::bail!(
                "gitprism sync: round-tripped branch {branch:?} has no ref on dest anymore — source and dest are out of sync (a round-tripped branch's dest ref should never be deleted); investigate before syncing again"
            );
        }

        reporter.step(
            branch,
            Direction::DestToSource,
            "fetching dest (checking for independent content to reflect into source)",
        );
        git::fetch(source_root, &dest_url, branch)
            .with_context(|| format!("fetching dest branch {branch:?} from configured remote"))?;
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();

        // On the first attempt, source's own tip is normally just this
        // checkout's local branch — no fetch needed. But `branch` is a
        // `config.branches` entry, meant to round-trip on every run
        // regardless of which branch actually triggered this run
        // (decisions/0005, decisions/0017), so it can't be assumed present:
        // a CI job whose default git strategy only fetches the ref that
        // triggered the pipeline (e.g. GitLab CI) leaves every other
        // configured branch absent from this checkout entirely
        // (decisions/0041). When that happens, fetch it from source's own
        // remote and create the local branch from the fetched tip — the
        // same fetch already used below for a lost push-race retry, just
        // reused at the point it's actually needed, and left as a real
        // local ref so `preflight_local_source_branch`/
        // `advance_local_source_branch` need no special-casing.
        //
        // A retry (attempt > 0) means the push below lost a fast-forward
        // race against source's *remote* instead, which the local branch
        // (now guaranteed to exist) can't see by itself — so from then on,
        // refetch source's own branch too and recompute against its actual
        // current tip, the same refetch-and-recompute principle
        // decisions/0009 already established for the source→dest direction.
        let source_tip = if attempt == 0 {
            match repo.find_branch(branch, git2::BranchType::Local) {
                Ok(local_branch) => local_branch
                    .get()
                    .peel_to_commit()
                    .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
                    .id(),
                Err(error) if error.code() == git2::ErrorCode::NotFound => {
                    let source_url = config.source_url()?;
                    reporter.step(
                        branch,
                        Direction::DestToSource,
                        "fetching source (branch not present in this checkout)",
                    );
                    git::fetch(source_root, &source_url, branch).with_context(|| {
                        format!("fetching source branch {branch:?} from configured remote")
                    })?;
                    let fetched_tip = repo
                        .find_reference("FETCH_HEAD")
                        .context("reading FETCH_HEAD after fetch")?
                        .peel_to_commit()
                        .context("resolving fetched source branch to a commit")?
                        .id();
                    repo.reference(
                        &format!("refs/heads/{branch}"),
                        fetched_tip,
                        false,
                        "gitprism sync: creating local branch from source's own remote",
                    )
                    .with_context(|| {
                        format!(
                            "creating local branch {branch:?} from source's own remote at {fetched_tip}"
                        )
                    })?;
                    fetched_tip
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("resolving source branch {branch:?}"));
                }
            }
        } else {
            // Only resolved once actually needed — a config that omits
            // [source].url/GITPRISM_SOURCE_URL entirely (decisions/0013) is
            // valid as long as this branch never actually needs to push
            // anything to source, e.g. a branch that never receives
            // independent dest-side commits.
            let source_url = config.source_url()?;
            reporter.step(
                branch,
                Direction::DestToSource,
                "refetching source (lost a push race, recomputing)",
            );
            git::fetch(source_root, &source_url, branch).with_context(|| {
                format!("fetching source branch {branch:?} from configured remote")
            })?;
            repo.find_reference("FETCH_HEAD")
                .context("reading FETCH_HEAD after fetch")?
                .peel_to_commit()
                .context("resolving fetched source branch to a commit")?
                .id()
        };

        let pending = pending_dest_commits(repo, source_tip, dest_tip, branch, state_key)
            .with_context(|| {
                format!("has dest branch {branch:?}'s history been rewritten outside gitprism?")
            })?;
        let build = build_pending_source_tip(
            repo,
            config,
            pending,
            source_tip,
            source_root,
            branch,
            state_key,
        )?;

        if let Some(new_source_tip) = build.new_tip {
            let expected_local_tip = preflight_local_source_branch(repo, branch, new_source_tip)
                .with_context(|| {
                    format!(
                        "checking whether local source branch {branch:?} can be advanced before pushing"
                    )
                })?;
            let source_url = config.source_url()?;
            // Always round-tripped — `branch` here always comes from
            // `config.branches` (see the doc comment above) — so
            // fast-forward-only unconditionally, per decisions/0038.
            match git::push(
                source_root,
                &source_url,
                new_source_tip,
                branch,
                PushMode::FastForwardOnly,
            )? {
                git::PushOutcome::Accepted => {
                    advance_local_source_branch(
                        repo,
                        branch,
                        new_source_tip,
                        expected_local_tip,
                    )
                    .with_context(|| {
                        format!(
                                "source remote accepted {branch:?}, but the local source branch could not be advanced safely; the remote push succeeded. Preserve local changes, fetch the configured source remote, and fast-forward or reconcile the local branch with ordinary Git before rerunning"
                        )
                    })?;
                }
                git::PushOutcome::RejectedRefMoved if attempt < MAX_RACE_RETRIES => {
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedRefMoved => {
                    anyhow::bail!(divergence_after_exhausted_retries_message(branch, "source"))
                }
            }
        }

        if let Some(conflict) = build.conflict {
            // Always round-tripped (`true`): `branch` here is always named in
            // `config.branches` (decisions/0017's own doc comment above), so
            // there's no mirror-only case for this direction's completed line
            // to distinguish.
            reporter.complete(
                Outcome::Error,
                branch,
                Direction::DestToSource,
                true,
                Some(&format!(
                    "hit a real conflict at dest commit {} in {:?} on branch {branch:?} — resolve it with `gitprism resolve <branch>`; commits before it were still pushed to source's branch",
                    conflict.commit, conflict.paths
                )),
            );
            // Decisions/0007's hard-stop ends this run right here — clear
            // the pinned bar rather than leaving it frozen mid-step above
            // the error text `anyhow::bail!` is about to print, which would
            // otherwise read as "the run hung," not "the run errored."
            reporter.finish();
            anyhow::bail!(
                "gitprism sync: {branch:?} <- {branch:?} hit a real conflict at dest commit {} in {:?} — resolve it with `gitprism resolve <branch>`; commits before it were still pushed to source's branch",
                conflict.commit,
                conflict.paths
            );
        }

        reporter.complete(
            if build.new_tip.is_some() {
                Outcome::Done
            } else {
                Outcome::Skipped
            },
            branch,
            Direction::DestToSource,
            true,
            build
                .new_tip
                .is_none()
                .then_some("nothing new to reflect back into source"),
        );
        return Ok(());
    }
}

/// Every dest commit still pending reconciliation onto source, oldest first,
/// with loop-prevention already applied (decisions/0003) — exactly what
/// [`sync_pair_from_dest`] would attempt to build next. Pulled out as its own
/// function so `gitprism resolve` (decisions/0008, 0015) can compute the
/// identical list — resolve and sync must never disagree about which dest
/// commit is next.
///
/// `boundary` names a dest-space commit (via [`marker_scan::newest_dest_marker`]);
/// it's verified to actually be an ancestor of (or equal to) `dest_tip`
/// before trusting it to scope the walk — dest is fast-forward-only in
/// normal operation (requirements/0001), so this should always hold, but a
/// missing object or a genuine non-ancestor both mean something is wrong
/// enough to fail loudly rather than silently mis-walk.
pub(crate) fn pending_dest_commits(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Vec<Oid>> {
    let (_, boundary) = newest_dest_marker(repo, source_tip, branch, key)?;
    if boundary != dest_tip {
        let is_ancestor = repo.find_commit(boundary).is_ok()
            && repo
                .graph_descendant_of(dest_tip, boundary)
                .with_context(|| format!("checking whether {dest_tip} descends from {boundary}"))?;
        if !is_ancestor {
            anyhow::bail!(
                "gitprism sync: source's last-synced dest commit ({boundary}) isn't an ancestor of dest's current tip ({dest_tip})"
            );
        }
    }

    let pending = pending_commits(repo, boundary, dest_tip)?;
    let mut result = Vec::with_capacity(pending.len());
    for oid in pending {
        let commit = repo
            .find_commit(oid)
            .context("resolving a pending dest commit")?;
        // Loop prevention (decisions/0003): a dest commit that itself came
        // from source (source→dest sync) already exists on source — cherry-
        // picking it back would loop.
        if marker::verify(&commit, branch, &[MarkerDirection::SourceToDest], None, key).is_none() {
            result.push(oid);
        }
    }
    Ok(result)
}

/// The result of [`build_pending_source_tip`]: `new_tip` is the chain's tip
/// if anything was built (`None` only if every pending commit was loop-
/// prevented, or nothing was pending at all — a clean merge always gets its
/// own marker commit even when it changes nothing, see
/// `build_pending_source_tip`'s doc comment), and `conflict` names the first
/// dest commit that couldn't be merged cleanly, if any — processing always
/// stops there (decisions/0007's "Consequences": later commits may depend on
/// it).
struct PendingSourceBuild {
    new_tip: Option<Oid>,
    conflict: Option<Conflict>,
}

/// Builds, in `repo`'s object database, a chain of new commits reflecting
/// every commit in `pending` (already loop-prevention-filtered, see
/// [`pending_dest_commits`]) that merges cleanly onto source via a real `git
/// merge-tree` subprocess (decisions/0016) — stopping at the first one that
/// doesn't (decisions/0007). `source_tip` seeds the chain's first parent.
///
/// The merge base is `dest_commit.parent(0)`'s tree, unfiltered — dest never
/// holds source-only content, so there is nothing here for gitprism's own
/// filtering to do. `sync` and `gitprism resolve` (decisions/0015) now agree
/// on both the merge base and the merge engine (`git cherry-pick` and `git
/// merge-tree` are both backed by merge-ort), so the two can no longer
/// disagree about whether a given commit conflicts.
fn build_pending_source_tip(
    repo: &Repository,
    config: &Config,
    pending: Vec<Oid>,
    source_tip: Oid,
    source_root: &Path,
    branch: &str,
    key: &marker::StateKey,
) -> Result<PendingSourceBuild> {
    let mut parent = source_tip;
    let mut built_any = false;
    for dest_oid in pending {
        let dest_commit = repo
            .find_commit(dest_oid)
            .context("resolving a pending dest commit")?;

        let parent_commit = repo
            .find_commit(parent)
            .context("resolving the in-progress source chain's parent")?;

        let base_tree = match dest_commit.parent(0) {
            Ok(base_commit) => base_commit.tree_id(),
            Err(_) => empty_tree(repo)?,
        };

        match git::merge_tree(
            source_root,
            base_tree,
            parent_commit.tree_id(),
            dest_commit.tree_id(),
        )? {
            git::MergeTreeOutcome::Conflict { paths } => {
                return Ok(PendingSourceBuild {
                    new_tip: built_any.then_some(parent),
                    conflict: Some(Conflict {
                        commit: dest_oid,
                        paths,
                    }),
                });
            }
            git::MergeTreeOutcome::Clean(tree_oid) => {
                // Unlike source→dest's "don't push an empty commit" rule
                // (requirements/0001, scoped to that direction only), a dest
                // commit that merges to no change (e.g. dest's edit was
                // already present on source) still needs its own marker
                // commit here, even though its tree is identical to its
                // parent's — the resume boundary *is* the newest
                // Gitprism-Dest-Commit trailer on source's history
                // (decisions/0003), so skipping it would leave that trailer
                // pointing at an older dest oid forever, permanently blocking
                // source→dest from ever recognizing this dest commit (and
                // anything after it) as accounted for (`dest_resume_point`'s
                // case 2). This asymmetry with source→dest's skip rule is
                // deliberate, not an inconsistency to unify away.
                parent =
                    build_source_commit(repo, config, parent, &dest_commit, tree_oid, branch, key)?;
                built_any = true;
            }
        }
    }

    Ok(PendingSourceBuild {
        new_tip: built_any.then_some(parent),
        conflict: None,
    })
}

/// Builds one new source-bound commit in `repo`'s object database — object
/// only, no ref update, since the chain is pushed by oid once it's complete.
/// Preserves the original author, stamps gitprism's own committer identity
/// (decisions/0010), and carries the `Gitprism-Dest-Commit` trailer
/// (decisions/0003) that lets a future sync resume from here. `pub(crate)`
/// so `gitprism resolve` (decisions/0015) builds its own commit in exactly
/// the same shape once a human finishes resolving a conflict by hand.
pub(crate) fn build_source_commit(
    repo: &Repository,
    config: &Config,
    parent: Oid,
    dest_commit: &git2::Commit,
    tree_oid: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Oid> {
    let parent_commit = repo
        .find_commit(parent)
        .context("resolving the source chain's parent commit")?;
    let tree = repo
        .find_tree(tree_oid)
        .context("reading the cherry-picked tree")?;
    let committer = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = marker::build_message(
        validated_commit_message(dest_commit)?,
        MarkerDirection::DestToSource,
        branch,
        dest_commit.id(),
        "Gitprism-Dest-Commit",
        &[parent],
        tree.id(),
        &dest_commit.author(),
        &committer,
        key,
    );

    repo.commit(
        None,
        &dest_commit.author(),
        &committer,
        &message,
        &tree,
        &[&parent_commit],
    )
    .with_context(|| {
        format!(
            "building source commit for dest commit {}",
            dest_commit.id()
        )
    })
}

#[cfg(test)]
mod tests;
