//! decisions/0043/0044's dest anchor search: which dest-space commit a
//! discovered or rewritten mirror-only branch should build its chain onto,
//! and whether dest's current tip is a state gitprism recognizes as safe to
//! build on at all.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::git;
use crate::marker::{self, Direction as MarkerDirection};

use super::marker_scan::{newest_dest_marker, newest_source_marker, scan_for_dest_marker};

/// `setup`'s own real graft between source and dest (decisions/0006) — the
/// one commit both sides actually share ancestry from. dest→source's
/// cherry-picks give dest content a *marker* commit on source (see
/// [`newest_dest_marker`]), but never change source's real ancestry with
/// dest, so this never moves once `setup` has run for the pair.
///
/// `Ok(None)`: no merge base at all. Callers decide what that means for
/// their branch class (decisions/0045).
fn graft_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Option<Oid>> {
    Ok(repo.merge_base(source_tip, dest_tip).ok())
}

/// Whether `dest_tip` is a point gitprism already accounts for — a *safety*
/// question only, entirely tip/marker-based and independent of where
/// [`dest_resume_point`]'s actual revwalk boundary is. Three cases, checked
/// in order:
///
/// 1. It's the tip of gitprism's own last source→dest push (carries a
///    `Gitprism-Source-Commit` trailer directly).
/// 2. dest hasn't advanced at all since `setup`'s graft (decisions/0006), i.e.
///    no sync has landed yet and nothing independent has landed either.
/// 3. dest_tip has moved past the graft, but dest→source has already
///    reflected it into source this same run (source's history carries a
///    `Gitprism-Dest-Commit` trailer naming `dest_tip` exactly, checked via
///    [`newest_dest_marker`]).
///
/// Case 1 returning `true` on trailer presence alone isn't a loosening:
/// dest's tip is necessarily the newest marker [`newest_source_marker`]
/// would find scanning forward from it, so [`dest_resume_point`] still
/// applies the identical two ancestry guards to the identical oid and
/// refuses in exactly the same situations as today.
///
/// decisions/0039's rewrite detection needs to tell Case 1/3 ("a prior
/// gitprism sync genuinely landed here") apart from Case 2 ("dest hasn't
/// moved past the graft, so no sync has happened to be rewritten yet") —
/// a distinction the plain `bool` [`dest_tip_is_accounted_for`] collapses,
/// so the three cases are returned as their own variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestTipAccountedFor {
    No,
    ViaPriorGitprismSync,
    AtGraftOnly,
}

fn dest_tip_accounted_for(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<DestTipAccountedFor> {
    let dest_commit = repo
        .find_commit(dest_tip)
        .context("resolving dest's tip commit")?;

    // Case 1.
    if marker::verify(
        &dest_commit,
        branch,
        &[MarkerDirection::SourceToDest],
        None,
        key,
    )
    .is_some()
    {
        return Ok(DestTipAccountedFor::ViaPriorGitprismSync);
    }

    // Case 2.
    if graft_point(repo, source_tip, dest_tip)? == Some(dest_tip) {
        return Ok(DestTipAccountedFor::AtGraftOnly);
    }

    // Case 3: dest_tip has moved past the graft with nothing gitprism wrote
    // there directly (case 1 would've caught that) — only safe if
    // dest→source has already reflected dest_tip into source, i.e. source's
    // own history carries a Gitprism-Dest-Commit trailer naming it exactly
    // (this same run, since it's ordered first — see `run`'s doc comment).
    let (_, marker_names) = newest_dest_marker(repo, source_tip, branch, key)?;
    Ok(if marker_names == dest_tip {
        DestTipAccountedFor::ViaPriorGitprismSync
    } else {
        DestTipAccountedFor::No
    })
}

pub(super) fn dest_tip_is_accounted_for(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<bool> {
    Ok(dest_tip_accounted_for(repo, source_tip, dest_tip, branch, key)? != DestTipAccountedFor::No)
}

/// Where source's pending-commit walk ([`super::pending_commits`], feeding
/// [`super::build_pending_dest_tip`]) resumes from — `None` if dest carries
/// history gitprism doesn't recognize as safe to build on at all.
///
/// Two independent questions, computed separately:
///
/// * **Safety** — is dest's tip a state gitprism can safely build on at all?
///   [`dest_tip_is_accounted_for`], tip/marker-only.
/// * **Boundary** — which source commits does dest already have? A real scan
///   of dest's own history for the newest `Gitprism-Source-Commit` trailer
///   ([`newest_source_marker`]) — *not* the graft point. The graft point is
///   where source and dest's ancestry was joined once, at `setup` time, and
///   never moves again; using it as the boundary here would make
///   [`super::pending_commits`] re-yield every source commit gitprism
///   already pushed to dest as soon as dest gained any independent content
///   of its own (a merged PR, say) — silently duplicating already-synced
///   content on dest, or hard-stopping every later sync on a bogus
///   "conflict" if the duplicate re-apply doesn't happen to apply cleanly.
///   That was this function's actual bug before this split.
///
/// When the newest marker isn't usable — missing from this clone's odb
/// entirely, or found but not actually an ancestor of `source_tip` — the
/// answer is to refuse (`Ok(None)`) rather than fall back to an older marker
/// or to the graft: an older boundary would make [`super::pending_commits`]
/// re-yield everything between the two markers, the same bug with a wider
/// blast radius.
pub(crate) fn dest_resume_point_for_branch(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<Oid>> {
    if !dest_tip_is_accounted_for(repo, source_tip, dest_tip, branch, key)? {
        return Ok(None);
    }

    let Some(boundary) = newest_source_marker(repo, dest_tip, branch, key)? else {
        // dest legitimately has no gitprism-written commit anywhere in its
        // history (first sync ever for this pair) — the only boundary that
        // can mean is the original graft point. Computing it again here
        // (rather than threading it through from `dest_tip_is_accounted_for`'s
        // own case-2 check) is deliberate: it's cheap, and it preserves
        // today's failure ordering — no enum/Option plumbing needed just to
        // avoid one extra `merge_base` call. `dest_tip_is_accounted_for`
        // already proved shared history exists; a missing graft here is a
        // broken invariant, not a per-branch condition.
        return Ok(Some(graft_point(repo, source_tip, dest_tip)?.context(
            "no shared history between source and dest for this pair — has `gitprism setup` been run?",
        )?));
    };

    if boundary == source_tip {
        return Ok(Some(boundary));
    }

    // The trailer might name a commit this clone doesn't even have — a
    // sibling clone's own source commit is never transmitted to dest, only
    // the filtered commit it produced is, so an unrelated or behind clone has
    // no way to have fetched it. That's just as unsafe to build on as a
    // confirmed non-ancestor, so it's checked (and rejected) before asking
    // libgit2 to compare ancestry, whose own error surface for a missing
    // object isn't a clean `NotFound` here.
    if repo.find_commit(boundary).is_err() {
        return Ok(None);
    }

    let descends = repo.graph_descendant_of(source_tip, boundary).with_context(|| {
        format!(
            "checking whether {source_tip} descends from the Gitprism-Source-Commit trailer {boundary}"
        )
    })?;
    Ok(descends.then_some(boundary))
}

#[cfg(test)]
pub(super) fn dest_resume_point(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
) -> Result<Option<Oid>> {
    let key = marker::load_key()?;
    dest_resume_point_for_branch(repo, source_tip, dest_tip, "main", &key)
}

/// decisions/0039: positively identifies a rewritten mirror-only source
/// branch — a state checked directly by four conditions, not inferred from
/// exhausted retries. Callable only once [`dest_resume_point_for_branch`]
/// has already refused (returned `Ok(None)`) for this `(source_tip,
/// dest_tip)` pair; the caller is also responsible for condition 1
/// (mirror-only — absent from `config.branches`) and condition 2
/// (`dest_ref_exists`), since both are already known at the one call site
/// this is used from. This function checks the remaining two:
///
/// 3. a previous gitprism marker is found on dest's own history — Case 1 or
///    Case 3 of [`dest_tip_accounted_for`] ("a prior sync genuinely
///    happened"), explicitly excluding Case 2 (dest sitting exactly at the
///    graft, nothing synced yet — not a rewrite, just a first sync still
///    pending);
/// 4. `source_tip` no longer descends from that marker's boundary — the
///    exact `graph_descendant_of` check [`dest_resume_point_for_branch`]
///    already computed on its way to refusing, inspected here on its
///    `false` result instead of discarded.
///
/// A missing boundary object (the trailer names a commit this clone never
/// fetched) is genuinely ambiguous, shallow clone or not — rewritten, or
/// this clone simply hasn't fetched source far enough back yet — so it
/// never counts as a detected rewrite; [`dest_resume_point_for_branch`]'s
/// ordinary refusal stands for that case (decisions/0039's addendum-to-the-
/// addendum, 2026-08-27: a non-shallow clone's own history is *stale*, not
/// *complete* — see the amendment for why `Repository::is_shallow` doesn't
/// prove otherwise). Any other `find_commit` failure is a real error and
/// propagates as `Err`, never guessed either way.
pub(super) fn mirror_only_rewrite_detected(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<bool> {
    if dest_tip_accounted_for(repo, source_tip, dest_tip, branch, key)?
        != DestTipAccountedFor::ViaPriorGitprismSync
    {
        return Ok(false);
    }

    let Some(boundary) = newest_source_marker(repo, dest_tip, branch, key)? else {
        return Ok(false);
    };
    if boundary == source_tip {
        return Ok(false);
    }
    match repo.find_commit(boundary) {
        Ok(_) => {}
        Err(error) if error.code() == git2::ErrorCode::NotFound => {
            // decisions/0039 addendum (2026-08-27): a stale non-shallow
            // clone looks exactly like this; not positive rewrite evidence.
            return Ok(false);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("looking up the Gitprism-Source-Commit trailer {boundary}")
            });
        }
    }

    let descends = repo.graph_descendant_of(source_tip, boundary).with_context(|| {
        format!(
            "checking whether {source_tip} descends from the Gitprism-Source-Commit trailer {boundary}"
        )
    })?;
    Ok(!descends)
}

/// decisions/0043: `branch`'s dest-space anchor — [`scan_for_dest_marker`]'s
/// own baseline scan, refined by a search for a more specific anchor among
/// sibling branches discovered this run. A branch forked from another
/// already-mirrored branch (round-tripped or mirror-only) anchors its dest
/// chain on that branch's own mirror at their real merge-base, instead of
/// always falling back to the nearest round-tripped marker the baseline
/// scan finds by walking straight past every unmarked commit in between.
///
/// Shared by both of [`super::sync_pair_to_dest_with_key`]'s call sites — a
/// brand-new branch's own first mirror, and decisions/0039's rewrite-rebuild
/// arm — so both benefit with no second implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DestAnchor {
    /// The baseline scan itself found nothing — decisions/0024's existing
    /// warn-and-continue path, untouched.
    None,
    /// The `(boundary, dest_tip)` pair to build the pending chain onto —
    /// either the baseline, untouched, or a sibling branch's more specific
    /// one.
    Resolved(Oid, Oid),
    /// Two or more sibling candidates are equally specific mirrored
    /// ancestors and neither is an ancestor of the other — gitprism won't
    /// guess which to anchor onto. Carries every such candidate's branch
    /// name and merge-base oid, for the caller to name in a hard-fail
    /// message.
    Ambiguous(Vec<(String, Oid)>),
    /// Two or more sibling candidates share the exact same merge-base
    /// (source-side fork point), but their own dest histories — never
    /// merged with each other — each carry a *different* qualifying
    /// `Gitprism-Source-Commit` marker for it, so step 5 genuinely can't
    /// tell which dest-space projection of that shared fork point is the
    /// right one to anchor onto. Carries every disagreeing candidate's
    /// branch name and its own resolved dest-space anchor oid.
    AmbiguousResolution(Vec<(String, Oid)>),
}

/// decisions/0043 steps 2 and 5: every real subprocess this search would
/// otherwise repeat per candidate per branch, resolved through one cache per
/// `run()` invocation instead — dest-ref existence (`git ls-remote`) and a
/// fetched candidate's dest tip (`git fetch`) alike. This search runs once
/// per newly-discovered-or-rewritten branch, and step 5 (trying every member
/// of an equal-merge-base group, not just one — see Decision step 5) can
/// revisit the same sibling from more than one branch's own search, so
/// caching only one of the two subprocesses would just trade one quadratic
/// cost for the other. A cache hit skips the subprocess entirely; a miss
/// queries or fetches once and remembers the result for every later lookup
/// this run, including a later branch's own candidate search landing on the
/// same sibling.
#[derive(Default)]
pub(super) struct RunCache {
    pub(super) dest_ref_exists: HashMap<String, bool>,
    pub(super) dest_tip: HashMap<String, Oid>,
}

pub(super) fn dest_ref_exists_cached(
    source_root: &Path,
    dest_url: &str,
    branch: &str,
    cache: &mut RunCache,
) -> Result<bool> {
    if let Some(&exists) = cache.dest_ref_exists.get(branch) {
        return Ok(exists);
    }
    let exists = git::remote_ref_exists(source_root, dest_url, branch)
        .with_context(|| format!("checking whether {branch:?} has a dest ref"))?;
    cache.dest_ref_exists.insert(branch.to_string(), exists);
    Ok(exists)
}

/// A sibling candidate's fetched dest tip, resolved through the same
/// per-run cache as `dest_ref_exists_cached` — see [`RunCache`]. Trying
/// every member of an equal-merge-base group (Decision step 5) means the
/// same sibling can be looked up from more than one branch's own search
/// this run; a cache hit reuses the oid with no subprocess at all.
pub(super) fn fetch_dest_tip_cached(
    repo: &Repository,
    source_root: &Path,
    dest_url: &str,
    branch: &str,
    cache: &mut RunCache,
) -> Result<Oid> {
    if let Some(&tip) = cache.dest_tip.get(branch) {
        return Ok(tip);
    }
    git::fetch(source_root, dest_url, branch)
        .with_context(|| format!("fetching sibling candidate {branch:?} from configured remote"))?;
    let tip = repo
        .find_reference("FETCH_HEAD")
        .context("reading FETCH_HEAD after fetching a sibling candidate")?
        .peel_to_commit()
        .context("resolving a fetched sibling candidate to a commit")?
        .id();
    cache.dest_tip.insert(branch.to_string(), tip);
    Ok(tip)
}

pub(super) fn dest_anchor_for_branch(
    repo: &Repository,
    source_root: &Path,
    dest_url: &str,
    branch: &str,
    source_tip: Oid,
    key: &marker::StateKey,
    run_cache: &mut RunCache,
) -> Result<DestAnchor> {
    let Some((boundary_base, dest_tip_base)) = scan_for_dest_marker(repo, source_tip, branch, key)?
    else {
        return Ok(DestAnchor::None);
    };

    // decisions/0017's existing discovery, not a new listing mechanism —
    // every other local branch on source is a candidate, round-tripped or
    // mirror-only alike (no special-casing: the search only ever resolves
    // to a candidate's real historical merge-base, never its current tip,
    // which is exactly what makes anchoring on a round-tripped candidate
    // here as safe as a mirror-only one).
    let mut survivors: Vec<(String, Oid)> = Vec::new();
    let (candidates, _skipped) = super::list_source_branches(repo)?;
    for candidate in candidates {
        if candidate == branch {
            continue;
        }
        if !dest_ref_exists_cached(source_root, dest_url, &candidate, run_cache)? {
            continue;
        }
        let candidate_tip = repo
            .find_branch(&candidate, git2::BranchType::Local)
            .with_context(|| format!("resolving sibling candidate {candidate:?}"))?
            .get()
            .peel_to_commit()
            .with_context(|| format!("resolving sibling candidate {candidate:?} to a commit"))?
            .id();
        // No shared history at all with this candidate — same guard
        // `already_merged_into_a_landing_branch` already uses.
        let cbase = match repo.merge_base(source_tip, candidate_tip) {
            Ok(oid) => oid,
            Err(error) if error.code() == git2::ErrorCode::NotFound => continue,
            Err(error) => {
                return Err(anyhow::Error::from(error).context(format!(
                    "finding a merge base between this branch and sibling candidate {candidate:?}"
                )));
            }
        };
        // A candidate less specific than what the baseline already found is
        // never an improvement — bounds the search to real refinements only.
        let at_least_as_specific = cbase == boundary_base
            || repo
                .graph_descendant_of(cbase, boundary_base)
                .with_context(|| {
                    format!("checking whether {cbase} descends from {boundary_base}")
                })?;
        if !at_least_as_specific {
            continue;
        }
        survivors.push((candidate, cbase));
    }

    // A survivor whose merge-base merely ties `boundary_base` exactly offers
    // no refinement over the baseline at all — only a survivor strictly
    // beyond it is a real candidate to disambiguate among. Without this,
    // two or more candidates that both happen to share nothing beyond the
    // baseline (e.g. two unrelated siblings forked from the same graft
    // point as `branch` itself) would spuriously read as "ambiguous,"
    // despite neither actually proposing anything more specific than what
    // the baseline already found.
    if !survivors.iter().any(|(_, cbase)| *cbase != boundary_base) {
        return Ok(DestAnchor::Resolved(boundary_base, dest_tip_base));
    }

    // Two or more survivors sharing the exact same `cbase` are not
    // ambiguous *by merge-base* — equal is the opposite of incomparable —
    // but their own dest histories are never merged with each other, so
    // step 5 below can still disagree once each one is actually tried.
    // Group by `cbase` for the domination comparison, keeping every branch
    // name in the group rather than collapsing to one representative: a
    // representative picked before step 5 runs could be the one candidate
    // whose own dest history happens to lack a qualifying marker, silently
    // discarding a sibling that would have found one.
    let mut groups: Vec<(Oid, Vec<String>)> = Vec::new();
    for (name, cbase) in survivors {
        match groups.iter_mut().find(|(c, _)| *c == cbase) {
            Some((_, names)) => names.push(name),
            None => groups.push((cbase, vec![name])),
        }
    }
    for (_, names) in &mut groups {
        names.sort();
    }

    // The unique most-specific group: its `cbase` undominated by any other
    // group's, via `graph_descendant_of` — the same primitive
    // decisions/0039's own condition 4 already uses. A tie strictly at
    // `boundary_base` is always dominated by any real refinement above (per
    // the check just above, at least one exists here), so it never reaches
    // `maximal`. Two or more genuinely incomparable *refinements* are what
    // leaves more than one maximal group, handled below.
    let mut maximal: Vec<(Oid, Vec<String>)> = Vec::new();
    for (index, (cbase, names)) in groups.iter().enumerate() {
        let mut dominated = false;
        for (other_index, (other_cbase, _)) in groups.iter().enumerate() {
            if index == other_index {
                continue;
            }
            if repo
                .graph_descendant_of(*other_cbase, *cbase)
                .with_context(|| {
                    format!("comparing sibling candidate merge-bases {other_cbase} and {cbase}")
                })?
            {
                dominated = true;
                break;
            }
        }
        if !dominated {
            maximal.push((*cbase, names.clone()));
        }
    }

    let (cbase, names) = match maximal.len() {
        // Every group dominated by another is impossible for a nonempty
        // list under git's acyclic ancestry order, but treated as "the
        // search found nothing better" rather than panicking.
        0 => return Ok(DestAnchor::Resolved(boundary_base, dest_tip_base)),
        1 => maximal.into_iter().next().expect("checked len == 1"),
        _ => {
            // Every candidate in every incomparable group, not just one
            // representative per group — the operator needs to see all of
            // them to resolve the ambiguity.
            return Ok(DestAnchor::Ambiguous(
                maximal
                    .into_iter()
                    .flat_map(|(cbase, names)| names.into_iter().map(move |name| (name, cbase)))
                    .collect(),
            ));
        }
    };

    // Every branch sharing the winning `cbase` must actually be tried, not
    // just the first one alphabetically: an equal merge-base only proves
    // the *source-side* fork point is shared, not that every candidate's
    // *dest-side* history recorded it the same way. `fetch_dest_tip_cached`
    // keeps this at one real `git fetch` per sibling per run, even though
    // the same sibling can be revisited by more than one branch's own
    // search this run (decisions/0043's own cost claim would otherwise be
    // regressed right back by this very fix, just via `fetch` instead of
    // `ls-remote`).
    let mut resolutions: Vec<(String, Oid, Oid)> = Vec::new();
    for name in names {
        // decisions/0043's addendum (F-04): a `DestToSource` marker at or
        // before `cbase` on `name`'s own source-side history already records
        // exactly what dest has for `name`. No fetch needed, and it can be
        // strictly more specific than the dest-side scan below, which
        // depends on dest's own history separately carrying a matching
        // trailer — never true for a dest-native commit dest→source only
        // reflected into source's history, not dest's.
        let source_side = newest_dest_to_source_marker_at_or_before(repo, cbase, &name, key)?;

        let candidate_dest_tip =
            fetch_dest_tip_cached(repo, source_root, dest_url, &name, run_cache)?;
        let dest_side =
            newest_source_marker_at_or_before(repo, candidate_dest_tip, &name, cbase, key)?
                .map(|(found_dest_oid, found_source_oid)| (found_source_oid, found_dest_oid));

        let resolved = match (source_side, dest_side) {
            (Some((source_oid, dest_oid)), Some((_, dest_side_source_oid)))
                if source_oid == cbase
                    || repo
                        .graph_descendant_of(source_oid, dest_side_source_oid)
                        .with_context(|| {
                            format!(
                                "checking whether {source_oid} descends from {dest_side_source_oid}"
                            )
                        })? =>
            {
                Some((source_oid, dest_oid))
            }
            (Some(source_side), None) => Some(source_side),
            (_, dest_side) => dest_side,
        };

        if let Some((found_source_oid, found_dest_oid)) = resolved {
            resolutions.push((name, found_source_oid, found_dest_oid));
        }
    }

    match resolutions.as_slice() {
        // None of the equally-specific candidates' own dest histories carry
        // a qualifying marker for the shared merge-base (e.g. every one is
        // a round-tripped candidate that has never itself been synced
        // beyond `setup`'s own graft) — degrades to the existing baseline
        // rather than to a worse or unsafe result (decisions/0043's own
        // "Why": a bug in the new search must never produce something less
        // safe than today).
        [] => Ok(DestAnchor::Resolved(boundary_base, dest_tip_base)),
        [(_, found_source_oid, found_dest_oid), rest @ ..]
            if rest
                .iter()
                .all(|(_, s, d)| s == found_source_oid && d == found_dest_oid) =>
        {
            // Every candidate that resolved at all agrees on the same
            // dest-space anchor — not ambiguous, even if only some of the
            // group's candidates qualified at all.
            Ok(DestAnchor::Resolved(*found_source_oid, *found_dest_oid))
        }
        _ => Ok(DestAnchor::AmbiguousResolution(
            resolutions
                .into_iter()
                .map(|(name, _, found_dest_oid)| (name, found_dest_oid))
                .collect(),
        )),
    }
}

/// decisions/0043 step 5: [`newest_source_marker`]'s own first-parent walk
/// over a *sibling candidate's* dest history, narrowed to only accept a
/// `Gitprism-Source-Commit` trailer naming `cbase` itself or an ancestor of
/// it — not just the newest trailer found at all, since a sibling
/// candidate's dest history can carry later syncs that postdate the real
/// fork point being anchored on. Returns the found commit's own oid
/// (dest-space) alongside the source oid its trailer names.
fn newest_source_marker_at_or_before(
    repo: &Repository,
    dest_tip: Oid,
    branch: &str,
    cbase: Oid,
    key: &marker::StateKey,
) -> Result<Option<(Oid, Oid)>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting a sibling candidate's resume-point scan")?;
    revwalk
        .push(dest_tip)
        .context("seeding a sibling candidate's resume-point scan")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .context("ordering a sibling candidate's resume-point scan newest-first")?;
    revwalk.simplify_first_parent().context(
        "restricting a sibling candidate's resume-point scan to first-parent history (decisions/0019)",
    )?;

    for (scanned, oid) in revwalk.enumerate() {
        if scanned >= crate::limits::MAX_MARKER_SCAN_COMMITS {
            anyhow::bail!(
                "sibling candidate resume-point scan exceeds the {} commit limit",
                crate::limits::MAX_MARKER_SCAN_COMMITS
            );
        }
        let oid = oid.context("walking a sibling candidate's history for a resume point")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in a sibling candidate's history")?;
        let Some(source_oid) =
            marker::verify(&commit, branch, &[MarkerDirection::SourceToDest], None, key)
        else {
            continue;
        };
        let qualifies = source_oid == cbase
            || (repo.find_commit(source_oid).is_ok()
                && repo
                    .graph_descendant_of(cbase, source_oid)
                    .with_context(|| {
                        format!("checking whether {cbase} descends from {source_oid}")
                    })?);
        if qualifies {
            return Ok(Some((oid, source_oid)));
        }
    }

    Ok(None)
}

/// decisions/0043's addendum (F-04): the newest `DestToSource` marker at or
/// before `cbase`, on `branch`'s own source-side history — deliberately
/// narrower than [`scan_for_dest_marker`] (`Setup` excluded): every branch
/// trivially inherits the one shared `Setup` graft regardless of its own
/// name (`marker::verify`'s own hardcoded exception), so accepting it here
/// would rediscover the same coarse point `boundary_base` already reflects
/// for every candidate, not a real per-candidate refinement — exactly the
/// false "candidate found something" that made two equal-`cbase` siblings
/// with no `DestToSource` marker of their own read as disagreeing instead of
/// both correctly contributing nothing.
fn newest_dest_to_source_marker_at_or_before(
    repo: &Repository,
    cbase: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<(Oid, Oid)>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting a sibling candidate's own dest-to-source scan")?;
    revwalk
        .push(cbase)
        .context("seeding a sibling candidate's own dest-to-source scan")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .context("ordering a sibling candidate's own dest-to-source scan newest-first")?;
    revwalk.simplify_first_parent().context(
        "restricting a sibling candidate's own dest-to-source scan to first-parent history (decisions/0019)",
    )?;

    for (scanned, oid) in revwalk.enumerate() {
        if scanned >= crate::limits::MAX_MARKER_SCAN_COMMITS {
            anyhow::bail!(
                "sibling candidate dest-to-source scan exceeds the {} commit limit",
                crate::limits::MAX_MARKER_SCAN_COMMITS
            );
        }
        let oid =
            oid.context("walking a sibling candidate's own history for a dest-to-source marker")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in a sibling candidate's own history")?;
        if let Some(dest_oid) =
            marker::verify(&commit, branch, &[MarkerDirection::DestToSource], None, key)
        {
            return Ok(Some((oid, dest_oid)));
        }
    }

    Ok(None)
}

/// decisions/0043's per-branch hard-fail message for
/// [`DestAnchor::Ambiguous`] — names every equally specific candidate and
/// its merge-base oid, matching decisions/0007's and decisions/0023's own
/// hard-fail precedent of naming exactly what's ambiguous rather than
/// guessing, but surfaced as a per-branch halt (decisions/0024's
/// precedent), not a whole-run abort.
pub(super) fn ambiguous_anchor_message(branch: &str, candidates: &[(String, Oid)]) -> String {
    let list = candidates
        .iter()
        .map(|(name, cbase)| format!("{name:?} (merge-base {cbase})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{branch:?} halted — its dest anchor is ambiguous: {list} are equally specific \
         mirrored ancestors, none an ancestor of any other; gitprism won't guess which one \
         to anchor onto — merge or rebase to establish a real order between them, then \
         re-run gitprism sync"
    )
}

/// decisions/0043's per-branch hard-fail message for
/// [`DestAnchor::AmbiguousResolution`] — names every candidate that shares
/// the winning merge-base alongside the dest-space anchor its own history
/// resolved to, so the operator can see exactly how they disagree.
pub(super) fn ambiguous_resolution_message(branch: &str, candidates: &[(String, Oid)]) -> String {
    let list = candidates
        .iter()
        .map(|(name, dest_oid)| format!("{name:?} (resolves to dest commit {dest_oid})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{branch:?} halted — its dest anchor is ambiguous: {list} share the same source-side \
         fork point, but their own dest histories disagree on where it landed; gitprism won't \
         guess which one to anchor onto — merge or rebase to establish a real order between \
         them, then re-run gitprism sync"
    )
}
