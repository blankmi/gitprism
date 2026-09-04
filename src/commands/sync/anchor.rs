//! decisions/0044/0046's dest anchor search: which dest-space commit a
//! discovered or rewritten mirror-only branch should build its chain onto,
//! and whether dest's current tip is a state gitprism recognizes as safe to
//! build on at all.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::git;
use crate::limits;
use crate::marker::{self, Direction as MarkerDirection};

use super::mapping_index::{MappingIndex, MappingLookup};
use super::marker_scan::{dest_to_source_boundary, newest_dest_marker, newest_source_marker};

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
/// All three cases are deliberately branch-scoped, matching this function's
/// other caller ([`super::branch_scoped_dest_tip`], decisions/0044's F-02):
/// "does `dest_tip` already carry *this branch's own* identity" is a
/// different, narrower question than decisions/0048's dest→source
/// represented-prefix walk answers (branch-agnostic by design, via
/// `marker::verify_self`) — collapsing the two would let a branch anchored
/// on a *sibling's* own marker commit (e.g. `task` forked from mirror-only
/// `feature` with no commits of its own, decisions/0043) wrongly conclude it
/// never needs a marker of its own, breaking every later branch-scoped scan
/// that depends on one existing. [`dest_resume_point_for_branch`] widens
/// *its own* safety question past this function's three cases separately —
/// see [`dest_tip_represented_in_source`] — rather than widening this
/// shared predicate itself.
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

/// decisions/0048 addendum: a *safety-only* widening of
/// [`dest_tip_accounted_for`]'s case 3, consulted solely by
/// [`dest_resume_point_for_branch`] — never by
/// [`super::branch_scoped_dest_tip`] (decisions/0044's F-02 identity
/// question, which must stay branch-scoped, see the doc comment on
/// [`dest_tip_accounted_for`] above) and never by
/// [`mirror_only_rewrite_detected`] (decisions/0039's rewrite detection
/// requires a genuine prior branch-scoped sync to license a force-push
/// rebuild; narrower there is fail-safe, and a round-tripped branch — the
/// only shape this function's own scenario applies to — never reaches that
/// arm regardless).
///
/// A dest→source pass can find `dest_tip` already represented purely via
/// decisions/0048's case 2 (an inherited marker, reachable from
/// `source_tip`, naming `dest_tip` exactly) with no case-1 marker of its own
/// anywhere on `dest_tip`'s own line — the exact shape that pass writes
/// *nothing* new to source for, so B1 never moves and
/// [`dest_tip_accounted_for`]'s case 3 never sees a fresh marker naming
/// `dest_tip`. Reusing [`dest_to_source_boundary`] here — the same
/// represented-prefix walk `pending_dest_commits` computes for that pass —
/// rather than reimplementing case 1/case 2 is what makes this agree with
/// it: `dest_tip` is safe to build on exactly when the walk's own boundary
/// *is* `dest_tip`, meaning every commit between B1 and `dest_tip` was
/// individually proven represented in `source_tip`.
///
/// Representation alone is not enough, though — a dest ref parked entirely
/// on a *sibling* branch's own mirror chain (e.g. dest `hotfix` cut from
/// dest `main` at `main`'s own mirror commit, with no mirror of `hotfix`
/// itself yet) can be fully represented by this walk — case 1 doesn't care
/// whose marker it is — while carrying no `hotfix`-branded marker anywhere
/// on its own line at all. That second half of the safety question is
/// [`newest_mirror_marker_belongs_to_branch`], applied by
/// [`dest_resume_point_for_branch`] after *either* accounting path accepts
/// `dest_tip` — not here, since [`dest_tip_accounted_for`]'s own case 3
/// reaches the same shape without ever calling this function (decisions/0048
/// addendum, "Corrected by a third review").
///
/// The walk's own B1-ancestor precondition is checked directly here first,
/// rather than by catching [`dest_to_source_boundary`]'s `Err`: a `dest_tip`
/// that simply isn't accounted for at all (nothing has synced yet, or a
/// genuine, ordinary divergence) must stay a per-branch `false` — the shape
/// every caller of this function already expects — not escalate into a
/// whole-run abort. [`b1_reachable_via_first_parent`] pre-checks, and
/// converts to `false` the same way, the refusal [`dest_to_source_boundary`]
/// would otherwise `bail!` on: decisions/0019's first-parent limitation
/// tripping on a `dest_tip` already confirmed by full-ancestry
/// `graph_descendant_of` to descend from B1, but whose first-parent line
/// never actually reaches it — left unchecked, that `Err` would abort the
/// *entire* run for a *discovered* branch instead of the per-branch halt
/// decisions/0046 Addendum 2 requires. Only the scan-limit bail from either
/// walk is still left to propagate as a genuine hard error, exactly as every
/// other bounded marker scan in this codebase already does (decisions/0032).
/// `newest_dest_marker` ends up scanning source's first-parent line more
/// than once across this function and its callees (here, again inside
/// [`dest_to_source_boundary`]'s own B1 lookup) — the same "cheap,
/// deliberate, no plumbing" tradeoff [`dest_resume_point_for_branch`]
/// already accepts for recomputing the graft point, not an oversight.
///
/// **Undocumented precondition:** this function returns `Ok(false)` whenever
/// `b1 == dest_tip`, since `graph_descendant_of` is non-reflexive (a commit
/// is never its own descendant) and `b1_is_ancestor` above is `false` for
/// that input. Harmless today only because this function's sole caller,
/// [`dest_resume_point_for_branch`], never reaches it with `b1 == dest_tip`:
/// [`dest_tip_is_accounted_for`]'s own case 3 already returns
/// `ViaPriorGitprismSync` for exactly that oid and short-circuits the `||`
/// before this function is called. A future caller invoking this function
/// directly with `b1 == dest_tip`, expecting it to answer "yes, trivially
/// represented", would get the wrong answer.
fn dest_tip_represented_in_source(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<bool> {
    let (_, b1) = newest_dest_marker(repo, source_tip, branch, key)?;
    let b1_is_ancestor = repo.find_commit(b1).is_ok()
        && repo
            .graph_descendant_of(dest_tip, b1)
            .with_context(|| format!("checking whether {dest_tip} descends from {b1}"))?;
    if !b1_is_ancestor {
        return Ok(false);
    }
    if !b1_reachable_via_first_parent(repo, dest_tip, b1)? {
        return Ok(false);
    }

    let boundary = dest_to_source_boundary(repo, source_tip, dest_tip, branch, key)?;
    Ok(boundary == dest_tip)
}

/// Whether [`newest_source_marker`] can be trusted to find the *right*
/// boundary on dest's line for `branch` — the second half of
/// [`dest_resume_point_for_branch`]'s safety question, applied after
/// *either* accounting path ([`dest_tip_accounted_for`] or
/// [`dest_tip_represented_in_source`]) has accepted `dest_tip`.
///
/// Representation alone is not enough: granting it must not outrun what
/// [`newest_source_marker`] can actually resume from. That scan is
/// branch-scoped by design ([`marker::verify`] against `branch` exactly, no
/// `verify_self` exemption for `SourceToDest`, unlike decisions/0048's
/// case 1/case 2), and it stops at the first branch-scoped match walking
/// down from `dest_tip`. A *foreign*-branded mirror marker sitting *above*
/// that match — a customer's fast-forward of dest `main` onto dest
/// `release`'s own mirror, say — is walked straight past, and the scan
/// resumes from a stale, older boundary, replaying already-mirrored content
/// as a phantom conflict. See decisions/0048's addendum ("Corrected by a
/// second review" and "Corrected by a third review") for the failure cases
/// and repros.
///
/// The check is therefore asked in one walk,
/// [`newest_source_to_dest_marker_branch_between`]: whichever self-verified
/// `SourceToDest` marker is *newest* anywhere on the **whole** of dest's
/// first-parent line must be branded for `branch` itself, or there must be
/// no such marker at all on the line (genuinely nothing has ever been
/// mirrored onto it, so `newest_source_marker`'s own `None` and its graft
/// fallback are correct, not a blind spot). That walk is deliberately
/// unbounded by B1: B1 is advanced by dest→source imports, which never
/// consult this gate, so the newest mirror marker can legitimately sit
/// below it.
///
/// This applies to every accounting path, not only the represented-prefix
/// one. [`dest_tip_accounted_for`]'s case 3 is satisfied by any ordinary
/// same-branch import naming `dest_tip` — the most common way into exactly
/// this shape — and case 3 says nothing about which mirror marker
/// `newest_source_marker` will find beneath. Case 1 passes trivially
/// (`dest_tip` is itself the newest marker and is branded for `branch`);
/// case 2 passes when nothing was ever mirrored below the graft. A
/// `dest_tip` sitting on a foreign-branded newest marker refuses today's
/// way regardless of how it was accounted for, and the operator's existing
/// recovery (delete the stray dest ref; the `!dest_ref_exists` arm then
/// re-anchors it via decisions/0046 and `branch_scoped_dest_tip` writes it
/// a marker of its own) stands.
fn newest_mirror_marker_belongs_to_branch(
    repo: &Repository,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<bool> {
    Ok(
        match newest_source_to_dest_marker_branch_between(repo, dest_tip, key)? {
            None => true,
            Some(found_branch) => found_branch == branch,
        },
    )
}

/// Whether `b1` is reachable from `dest_tip` via dest's first-parent line
/// alone (decisions/0019) — checked before [`dest_to_source_boundary`] runs
/// its own first-parent walk down to B1, so that walk's own `bail!` for this
/// exact shape doesn't have to propagate as `Err` through
/// [`dest_tip_represented_in_source`]'s `?` — see that function's own doc
/// comment for why an `Err` here is unsafe for a discovered branch.
/// [`newest_source_to_dest_marker_branch_between`] doesn't share this
/// precondition (it walks the whole of dest's first-parent line
/// unconditionally, with no B1 of its own to miss). Bounded by
/// [`limits::MAX_MARKER_SCAN_COMMITS`] the same way every other scan in this
/// module is (decisions/0032); exceeding it is still a genuine hard error,
/// not converted to `false` — in practice the one whole-run-abort path
/// decisions/0048's addendum left standing for a discovered branch, reached
/// only if dest's first-parent line exceeds `MAX_MARKER_SCAN_COMMITS`
/// commits without ever reaching B1 (over 100,000 commits), so the
/// addendum's "never a whole-run abort" framing isn't unconditionally true
/// because of it.
fn b1_reachable_via_first_parent(repo: &Repository, dest_tip: Oid, b1: Oid) -> Result<bool> {
    let mut current = dest_tip;
    for _ in 0..limits::MAX_MARKER_SCAN_COMMITS {
        if current == b1 {
            return Ok(true);
        }
        let commit = repo.find_commit(current).with_context(|| {
            format!("resolving {current} while checking dest's first-parent line for B1 ({b1})")
        })?;
        if commit.parent_count() == 0 {
            return Ok(false);
        }
        current = commit.parent_id(0).with_context(|| {
            format!("walking dest's first-parent history past {current} looking for B1 ({b1})")
        })?;
    }
    anyhow::bail!(
        "source->dest's B1-reachability pre-check exceeds the {} commit limit",
        limits::MAX_MARKER_SCAN_COMMITS
    )
}

/// The recorded branch of the *newest* self-verified `SourceToDest` marker
/// anywhere on the **whole** of dest's first-parent line from `dest_tip`
/// (bounded only by [`limits::MAX_MARKER_SCAN_COMMITS`], the same static
/// limit every other marker scan in this module uses — decisions/0032),
/// regardless of that marker's own recorded branch — the walk behind
/// [`newest_mirror_marker_belongs_to_branch`], replacing the unsafe
/// either/or its doc comment describes. Walking newest-first from `dest_tip` means the
/// first self-verified marker this finds *is* the newest one, so no separate
/// "which one is newest" comparison is needed. `Ok(None)` means nothing has
/// ever been mirrored onto this line at all — `newest_source_marker`'s own
/// `None`/graft-fallback case is genuinely safe, not a blind spot.
///
/// Deliberately **not** bounded to B1 (contrast [`dest_to_source_boundary`],
/// which is): see [`newest_mirror_marker_belongs_to_branch`]'s own doc comment
/// for why a B1-bounded version of this exact walk is unsafe — B1 is advanced by
/// dest→source imports this check never consults, so the newest mirror
/// marker on the line can legitimately sit below it. Walking
/// the whole line instead makes this exactly the range
/// [`newest_source_marker`] itself walks (unbounded, first-parent-only), so
/// the two can no longer disagree about what the newest marker on the line
/// is.
fn newest_source_to_dest_marker_branch_between(
    repo: &Repository,
    dest_tip: Oid,
    key: &marker::StateKey,
) -> Result<Option<String>> {
    let mut current = dest_tip;
    for _ in 0..limits::MAX_MARKER_SCAN_COMMITS {
        let commit = repo.find_commit(current).with_context(|| {
            format!("resolving {current} while checking dest's line for its newest mirror marker")
        })?;
        if let Some(marker) =
            marker::verify_self(&commit, &[MarkerDirection::SourceToDest], None, key)
        {
            return Ok(Some(marker.branch));
        }
        if commit.parent_count() == 0 {
            return Ok(None);
        }
        current = commit.parent_id(0).with_context(|| {
            format!(
                "walking dest's first-parent history past {current} looking for its newest mirror marker"
            )
        })?;
    }
    anyhow::bail!(
        "source->dest's prior-mirror safety check exceeds the {} commit limit",
        limits::MAX_MARKER_SCAN_COMMITS
    )
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
///   [`dest_tip_is_accounted_for`], tip/marker-only — widened by
///   [`dest_tip_represented_in_source`] (decisions/0048) for a `dest_tip`
///   that carries no marker of its own at all but is nonetheless the end of
///   B1's own represented-prefix walk, the shape a dest→source pass leaves
///   behind when it found `dest_tip` already represented and so wrote
///   nothing new to source for it. Then, whichever of the two accepted it,
///   [`newest_mirror_marker_belongs_to_branch`]: the boundary scan below is
///   branch-scoped, so it is only trustworthy when the newest mirror marker
///   on dest's line is this branch's own (or there is none).
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
    if !dest_tip_is_accounted_for(repo, source_tip, dest_tip, branch, key)?
        && !dest_tip_represented_in_source(repo, source_tip, dest_tip, branch, key)?
    {
        return Ok(None);
    }
    if !newest_mirror_marker_belongs_to_branch(repo, dest_tip, branch, key)? {
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

    // The trailer might name a commit this clone doesn't even have — another
    // clone's own source commit is never transmitted to dest, only the
    // filtered commit it produced is, so an unrelated or behind clone has
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

/// A branch's destination-space anchor resolved from its nearest exact
/// authenticated mapping on the branch's first-parent history.
///
/// Shared by both of [`super::sync_pair_to_dest_with_key`]'s call sites — a
/// brand-new branch's own first mirror, and decisions/0039's rewrite-rebuild
/// arm — so both benefit with no second implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DestAnchor {
    /// No exact authenticated mapping was found — decisions/0024's existing
    /// warn-and-continue path, untouched.
    None,
    /// The `(boundary, dest_tip)` pair to build the pending chain onto —
    /// the nearest exact mapping's source and destination commits.
    Resolved(Oid, Oid),
    /// Exact authenticated mappings for the nearest source commit point at
    /// incomparable canonical dest histories. The caller reports this as a
    /// per-branch halt with the source OID and provenance details intact.
    Contradictory(String),
}

/// Per-run cache for destination refs and fetched tips, plus the authenticated
/// source-to-destination mapping index reconstructed from both repositories.
///
/// `mapping_index` is populated exactly once, by [`run`](super::run) right
/// after dest→source has run and before source→dest scheduling starts
/// (decisions/0046) — never lazily, and never left unset for a call site to
/// discover. A default-constructed cache carries an empty index, which is
/// only ever the right starting point for that one init site or for a test
/// that builds its own via [`reconstruct_mapping_index`].
#[derive(Default)]
pub(super) struct RunCache {
    pub(super) dest_ref_exists: HashMap<String, bool>,
    pub(super) mapping_index: MappingIndex,
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

/// Fetches `branch`'s dest head for [`reconstruct_mapping_index`], resolving
/// the decisions/0046 Addendum 2, Finding G list/fetch race: `branch` was
/// advertised by reconstruction's own initial listing, but another writer
/// may have deleted it before this call runs. On a fetch failure, the dest
/// listing is refreshed at most once per run (via `refreshed_listing`,
/// shared across every branch this run reconstructs) and the two outcomes
/// are handled differently on purpose. A branch absent from the refreshed
/// listing is fully recovered — *if* that listing can actually support "dest
/// has no branch by this name" (decisions/0047 addendum:
/// `can_establish_absence()`, true unless the branch-limit horizon was hit —
/// an unrelated undecodable ref elsewhere never blocks this, since it
/// doesn't stop the scan and can't be `branch` itself). Any other fetch
/// failure — auth, network, transport, a corrupt remote, the branch still
/// being advertised, or the listing unable to establish absence — propagates
/// instead: silently skipping it would leave the mapping index incomplete
/// while every other branch's lookup still believed it complete, the exact
/// "a visible mapping hides an unscanned contradiction" hole Addendum 1
/// closed. A per-branch halt is the right shape for a per-branch fact;
/// reconstruction's completeness is a whole-run fact.
pub(super) fn fetch_dest_head_for_reconstruction(
    repo: &Repository,
    source_root: &Path,
    dest_url: &str,
    branch: &str,
    run_cache: &mut RunCache,
    refreshed_listing: &mut Option<git::RemoteBranchListing>,
) -> Result<Option<Oid>> {
    let fetch_err = match git::fetch(source_root, dest_url, branch) {
        Ok(()) => {
            let tip = repo
                .find_reference("FETCH_HEAD")
                .context("reading FETCH_HEAD after fetching a destination branch")?
                .peel_to_commit()
                .context("resolving a fetched destination branch to a commit")?
                .id();
            return Ok(Some(tip));
        }
        Err(err) => err,
    };
    if refreshed_listing.is_none() {
        *refreshed_listing = Some(
            git::remote_branch_names(source_root, dest_url)
                .context("refreshing the dest branch listing after a fetch failure")?,
        );
    }
    let listing = refreshed_listing.as_ref().expect("just populated above");
    if listing.completeness.can_establish_absence()
        && !listing.names.iter().any(|name| name == branch)
    {
        run_cache.dest_ref_exists.insert(branch.to_string(), false);
        return Ok(None);
    }
    Err(fetch_err.context(format!(
        "fetching destination branch {branch:?} from configured remote"
    )))
}

pub(super) fn reconstruct_mapping_index(
    repo: &Repository,
    source_root: &Path,
    dest_url: &str,
    source_branches: &[String],
    key: &marker::StateKey,
    run_cache: &mut RunCache,
) -> Result<MappingIndex> {
    let source_heads = source_branches
        .iter()
        .map(|branch| {
            let tip = repo
                .find_branch(branch, git2::BranchType::Local)
                .with_context(|| format!("resolving source branch {branch:?}"))?
                .get()
                .peel_to_commit()
                .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
                .id();
            Ok((branch.clone(), tip))
        })
        .collect::<Result<Vec<_>>>()?;
    // decisions/0046, F-A: a mirror-only branch's dest ref must still
    // contribute its own SourceToDest mappings once its local source branch
    // is deleted (decisions/0018 Case 2's routine post-merge cleanup) —
    // `source_branches` can no longer name it, so dest's actual branches are
    // asked for directly rather than inferred from what source still has.
    let mut dest_branch_names = source_branches.to_vec();
    let dest_listing = git::remote_branch_names(source_root, dest_url)?;
    // Existence for every listed name — and, when the listing can establish
    // absence, non-existence for every source branch it didn't list — is now
    // known from this one subprocess, recorded into the same cache
    // `dest_ref_exists_cached` reads below so it never re-queries per
    // branch (decisions/0043's own quadratic-cost lesson; decisions/0046
    // Addendum 2, Finding J for the non-existence half). Only the
    // branch-limit horizon blocks that negative claim (decisions/0047
    // addendum: `can_establish_absence()`) — an unlisted name might still
    // have a dest ref beyond it. An undecodable ref elsewhere doesn't: it
    // never stops the scan and can't be a distinct, valid-UTF-8 source
    // branch itself, so absence still seeds for every other case, and
    // `dest_ref_exists_cached` is left to query only what a real horizon
    // left unresolved.
    for name in &dest_listing.names {
        run_cache.dest_ref_exists.insert(name.clone(), true);
        if !dest_branch_names.contains(name) {
            dest_branch_names.push(name.clone());
        }
    }
    if dest_listing.completeness.can_establish_absence() {
        for branch in source_branches {
            if !dest_listing.names.contains(branch) {
                run_cache.dest_ref_exists.insert(branch.clone(), false);
            }
        }
    }

    let mut dest_heads = Vec::new();
    let mut refreshed_listing = None;
    for branch in &dest_branch_names {
        if !dest_ref_exists_cached(source_root, dest_url, branch, run_cache)? {
            continue;
        }
        if let Some(tip) = fetch_dest_head_for_reconstruction(
            repo,
            source_root,
            dest_url,
            branch,
            run_cache,
            &mut refreshed_listing,
        )? {
            dest_heads.push((branch.clone(), tip));
        }
    }
    let mut index = MappingIndex::reconstruct(repo, &source_heads, &dest_heads, key)?;
    if !dest_listing.completeness.is_complete() {
        // decisions/0046 Addendum 2, Finding F; decisions/0047 and its
        // addendum: an unlisted or undecodable dest branch may carry an
        // incomparable mapping for a source commit already observed, so any
        // recorded cause must taint exact lookups the same way a truncated
        // history scan already does — `describe()` names every cause found,
        // not just one.
        index.note_incomplete_reconstruction(dest_listing.completeness.describe());
    }
    Ok(index)
}

/// `branch` is excluded from canonicalization (decisions/0046, F-C):
/// `branch`'s own rewrite is what's being anchored here, so a mapping whose
/// only provenance is `branch` itself is that branch's own now-discarded
/// chain, never a valid anchor for its own rebuild — matching main's old
/// `if candidate == branch { continue; }` sibling-search exclusion, applied
/// at both of this function's own call sites (a brand-new branch's first
/// mirror, and a detected mirror-only rewrite's rebuild).
pub(super) fn dest_anchor_for_branch(
    repo: &Repository,
    source_tip: Oid,
    branch: &str,
    run_cache: &RunCache,
) -> Result<DestAnchor> {
    Ok(
        match run_cache.mapping_index.nearest_first_parent_mapping(
            repo,
            source_tip,
            Some(branch),
        )? {
            MappingLookup::Resolved(mapping) => DestAnchor::Resolved(mapping.source, mapping.dest),
            MappingLookup::None => DestAnchor::None,
            MappingLookup::Contradictory(diagnostic) => DestAnchor::Contradictory(diagnostic),
        },
    )
}

pub(super) fn mapping_distance_for_branch(
    repo: &Repository,
    branch: &str,
    run_cache: &RunCache,
) -> Result<Option<usize>> {
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {branch:?}"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
        .id();
    let Some((distance, lookup)) = run_cache
        .mapping_index
        .nearest_first_parent_mapping_with_distance(repo, source_tip, None)?
    else {
        return Ok(None);
    };
    Ok(match lookup {
        MappingLookup::None => None,
        MappingLookup::Resolved(_) | MappingLookup::Contradictory(_) => Some(distance),
    })
}
