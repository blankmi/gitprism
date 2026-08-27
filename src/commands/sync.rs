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

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
    // Validate the pair secret before fetching or constructing any commits.
    let state_key = marker::load_key()?;
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism sync must be run inside an existing git repository (none found at or above {}) — has `gitprism setup` been run?",
            cwd.display()
        )
    })?;
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
    let source_branches = list_source_branches(&repo)?;
    let reporter = Reporter::new(
        config.branches.len() + source_branches.len(),
        config
            .branches
            .iter()
            .map(String::as_str)
            .chain(source_branches.iter().map(String::as_str)),
    );

    // dest→source first, for every explicitly configured branch: any content
    // dest carries that gitprism didn't itself put there (e.g. a merged PR)
    // must be reflected into source before source→dest's own refusal check
    // below evaluates dest's tip — that check refuses to build on dest
    // content it doesn't recognize, and reflecting it into source is exactly
    // what makes it recognized (see `dest_resume_point`'s third case).
    // Grouping by phase rather than by branch still guarantees this ordering
    // per branch, since discovery above only runs once, before either phase
    // (decisions/0017, decisions/0020).
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
    // branch its sync. decisions/0043 extends the same halt-and-continue
    // signal to a discovered branch whose dest anchor is genuinely
    // ambiguous between two equally specific mirrored siblings. Either
    // failure is accumulated instead and turned into a non-zero exit only
    // once every branch has been processed, so CI can't mistake a halted
    // branch for a clean run.
    let mut any_branch_halted = false;
    for branch in &source_branches {
        let halted = sync_pair_to_dest_with_key(
            &repo,
            &source_root,
            &config,
            branch,
            &reporter,
            &state_key,
            &exclude_list,
            &ignore_raw,
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
            "gitprism sync: one or more branches halted — either a replayed commit's control file disagreed with the pinned policy, or a discovered branch's dest anchor was ambiguous between equally specific mirrored siblings — see the branch lines above for the affected commit(s)/branch(es) and remedy"
        );
    }

    Ok(())
}

fn load_run_policy(config_path: &Path, source_root: &Path) -> Result<policy::VerifiedPolicy> {
    policy::load(config_path, &source_root.join(exclude::FILENAME))
}

/// Every local branch that exists on source right now, sorted for
/// deterministic run order (git2's branch iteration order isn't guaranteed).
/// Read once by [`run`] — before either sync phase starts, purely to size
/// [`Reporter`]'s upfront total (decisions/0020, point 3) — and reused for
/// the source→dest loop rather than listed again. A local, read-only `git
/// branch` enumeration; no fetch involved.
fn list_source_branches(repo: &Repository) -> Result<Vec<String>> {
    let mut source_branches = Vec::new();
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
        let name = std::str::from_utf8(name_bytes).with_context(|| {
            format!(
                "a local branch has a non-UTF-8 name gitprism can't mirror by: {}",
                git::escape_bytes(name_bytes)
            )
        })?;
        crate::git::validate_branch_name(name)
            .with_context(|| format!("validating local branch {name:?}"))?;
        source_branches.push(name.to_string());
    }
    source_branches.sort();
    Ok(source_branches)
}

/// Pushes `branch`'s pending commits from source to a same-named branch on
/// dest, filtered, one branch at a time — `branch` is discovered on source at
/// run time by [`run`], not read from config (decisions/0017). Recomputes
/// from scratch (refetch, rebuild, retry) on a lost fast-forward race rather
/// than rebasing what it already built (decisions/0009).
#[cfg(test)]
fn sync_pair_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    reporter: &Reporter,
) -> Result<bool> {
    let key = marker::load_key()?;
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)?
        .get()
        .peel_to_commit()?
        .id();
    let exclude_list = load_current_exclude_list(repo, source_tip)?;
    // The working tree, not `branch`'s own tip — matching `run`'s real
    // `policy::load` (decisions/0037's pinned policy is one value shared by
    // every branch, never a per-branch read).
    let ignore_raw =
        std::fs::read_to_string(source_root.join(exclude::FILENAME)).unwrap_or_default();
    sync_pair_to_dest_with_key(
        repo,
        source_root,
        config,
        branch,
        reporter,
        &key,
        &exclude_list,
        &ignore_raw,
    )
}

/// Returns `Ok(true)` if this branch halted with nothing pushed — either
/// because a replayed commit's control file disagreed with the pinned
/// policy (decisions/0037), or because a discovered branch's dest anchor
/// was genuinely ambiguous between two or more equally specific mirrored
/// sibling branches (decisions/0043) — `Ok(false)` for every other outcome
/// (done, skipped, or decisions/0024's warning).
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
        // branch that's supposed to already exist.
        let dest_ref_exists = git::remote_ref_exists(source_root, &dest_url, branch)?;
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
                        repo,
                        source_root,
                        &dest_url,
                        branch,
                        source_tip,
                        state_key,
                    )? {
                        DestAnchor::Resolved(boundary, dest_tip) => (boundary, dest_tip),
                        DestAnchor::Ambiguous(candidates) => {
                            reporter.complete(
                                Outcome::Error,
                                branch,
                                Direction::SourceToDest,
                                round_tripped,
                                Some(&ambiguous_anchor_message(branch, &candidates)),
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
                None => anyhow::bail!(
                    "gitprism sync: dest branch {branch:?} isn't at a point this clone can safely build on — either dest→source hasn't reflected its content into source yet, or this clone's {branch:?} is behind or diverged from what dest was last synced from (fetch/pull the latest source history first)"
                ),
            }
        } else {
            // No dest ref to be unsafe about yet, so no safety check applies
            // either — this branch's own ancestry already carries dest
            // content, inherited from whichever branch it was created from
            // (typically a branch `setup` grafted), so the nearest
            // `Gitprism-Dest-Commit` trailer reachable from `source_tip`
            // names both the dest-space tree to build the new chain onto and
            // the source-space boundary `pending_commits` should resume
            // from — the same graft-derived ancestry decisions/0006
            // established, just read directly off source's own history
            // instead of off a dest ref that doesn't exist. But unlike a
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
                repo,
                source_root,
                &dest_url,
                branch,
                source_tip,
                state_key,
            )? {
                DestAnchor::Resolved(boundary, dest_tip) => (boundary, dest_tip),
                // decisions/0043: two or more sibling branches are equally
                // specific mirrored ancestors of `branch` and neither is an
                // ancestor of the other — gitprism won't guess which to
                // anchor onto. A per-branch halt (decisions/0024's own
                // precedent for a structural surprise this function merely
                // discovered, not one an operator directly asked gitprism to
                // manage), not a whole-run abort.
                DestAnchor::Ambiguous(candidates) => {
                    reporter.complete(
                        Outcome::Error,
                        branch,
                        Direction::SourceToDest,
                        round_tripped,
                        Some(&ambiguous_anchor_message(branch, &candidates)),
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
            branch,
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
        // new to merge onto an existing dest ref (a genuine no-op); it's a
        // brand-new branch with no commits of its own beyond whatever
        // graft/marker point it shares with dest (decisions/0017: still has
        // to be created on dest); or a detected rewrite's replacement
        // commits built nothing at all — either `pending_commits` was empty
        // (source was reset straight back to a commit already carrying a
        // marker) or every pending commit filtered to no change against the
        // rebuild base. The first case alone is a genuine no-op; the other
        // two still need `dest_tip` pushed — for a fresh branch it's already
        // the right content, just missing a ref name, and for a detected
        // rewrite `dest_tip` here is `rebuild_dest_tip`, the graft-derived
        // base dest must be rewound to (decisions/0039's addendum: a rewrite
        // that rebuilds to the base is still a rewrite, not a no-op, even
        // when the base and the built chain happen to coincide).
        let force_rebuild = matches!(push_mode, PushMode::ForceMirrorOnly { .. });
        let new_dest_tip = build
            .new_tip
            .or((force_rebuild || !dest_ref_exists).then_some(dest_tip));

        if let Some(new_dest_tip) = new_dest_tip {
            match git::push(source_root, &dest_url, new_dest_tip, branch, push_mode)? {
                git::PushOutcome::Accepted => {}
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
            } else if force_rebuild && build.new_tip.is_none() {
                // Honest about what actually happened: dest's ref moved, but
                // to the shared rebuild base, not to a newly built commit —
                // must not read as "N new commits pushed" when none were.
                Some("rebuilt from the shared graft; no new commits were needed")
            } else {
                None
            },
        );
        return Ok(false);
    }
}

/// A replayed commit whose control file disagreed with the pinned policy
/// (decisions/0037): which commit, and which of the two filenames — never
/// both conflated into one report, since the operator needs to know exactly
/// which file to look at.
struct PolicyMismatch {
    commit: Oid,
    filename: &'static str,
    reason: PolicyMismatchReason,
}

enum PolicyMismatchReason {
    DiffersFromPinnedPolicy,
    NotARegularFile,
    ExceedsSizeLimit,
}

/// Whether any commit in `pending` — already filtered the same way
/// [`build_pending_dest_tip`] filters its own loop-prevented commits, so this
/// only ever inspects commits that would actually be replayed — carries a
/// `.gitprismignore` that either can't be read and compared at all (a
/// non-blob entry, or a blob over the size limit) or whose bytes differ from
/// the digest-verified pinned policy (decisions/0037, with an addendum:
/// safety not being establishable is classified the same way as bytes
/// actively disagreeing). A commit whose tree has no entry is never a
/// mismatch: the trusted policy already governs that commit regardless of
/// what its own tree contains, so absence can't weaken anything. Only the
/// first such commit found (oldest first, matching replay order) is
/// reported.
///
/// `.gitprism.toml` is deliberately not checked here (decisions/0037's later
/// amendment): unlike `.gitprismignore`, it's self-excluded from dest and
/// never consulted per replayed commit — decisions/0026 loads and verifies
/// it exactly once, globally, before either sync phase runs — so an earlier
/// pending commit's differing bytes can't leak content the way an
/// unenforced exclusion can. Checking it per-commit only produced friction
/// (an operator iterating on `.gitprism.toml` before the first successful
/// sync accumulates several pending versions) with no matching security
/// benefit.
fn find_control_file_policy_mismatch(
    repo: &Repository,
    pending: &[Oid],
    branch: &str,
    key: &marker::StateKey,
    ignore_raw: &str,
) -> Result<Option<PolicyMismatch>> {
    for &commit_oid in pending {
        let commit = repo
            .find_commit(commit_oid)
            .context("resolving a pending commit to check its control files")?;

        // Same loop-prevention `build_pending_dest_tip` itself applies: a
        // commit that came from dest→source (or from `setup`'s own graft) is
        // never replayed onto dest by this branch's sync, so it's not this
        // check's business either.
        if marker::verify(
            &commit,
            branch,
            &[MarkerDirection::Setup, MarkerDirection::DestToSource],
            None,
            key,
        )
        .is_some()
        {
            continue;
        }

        let tree = commit
            .tree()
            .context("reading a pending commit's tree to check its control files")?;
        let filename = exclude::FILENAME;
        let reason = match read_control_file_blob(repo, &tree, filename, commit_oid)? {
            ControlFileRead::Absent => continue,
            ControlFileRead::Blob(bytes) if bytes == ignore_raw.as_bytes() => continue,
            ControlFileRead::Blob(_) => PolicyMismatchReason::DiffersFromPinnedPolicy,
            ControlFileRead::NotARegularFile => PolicyMismatchReason::NotARegularFile,
            ControlFileRead::TooLarge => PolicyMismatchReason::ExceedsSizeLimit,
        };
        return Ok(Some(PolicyMismatch {
            commit: commit_oid,
            filename,
            reason,
        }));
    }
    Ok(None)
}

/// What [`read_control_file_blob`] found at a tree entry: whether it's
/// missing, a comparable blob, or something safety can't be established for
/// at all. The latter two cases (a non-blob entry, or a blob over the size
/// limit) are still this branch's problem to report, not this function's —
/// it never errors for them, since an unreadable entry in one pending
/// commit's tree must not abort every other branch's sync (decisions/0037's
/// addendum).
enum ControlFileRead {
    Absent,
    Blob(Vec<u8>),
    NotARegularFile,
    TooLarge,
}

/// Reads `filename`'s entry straight from `tree`'s root, bounded by
/// decisions/0032's existing `MAX_CONTROL_FILE_BYTES` — the same limit every
/// other control-file read in gitprism already respects. Absence is the
/// caller's concern, not this function's: decisions/0037 treats a missing
/// control file as "not a mismatch," never as an empty one. An entry that
/// isn't a blob (a directory or a submodule gitlink) is reported rather than
/// passed to `find_blob`, which would otherwise fail with an ODB type
/// mismatch for a condition that isn't actually an I/O error.
fn read_control_file_blob(
    repo: &Repository,
    tree: &git2::Tree,
    filename: &str,
    commit: Oid,
) -> Result<ControlFileRead> {
    let Some(entry) = tree.get_name(filename) else {
        return Ok(ControlFileRead::Absent);
    };
    if entry.kind() != Some(git2::ObjectType::Blob) {
        return Ok(ControlFileRead::NotARegularFile);
    }
    let blob = repo
        .find_blob(entry.id())
        .with_context(|| format!("reading {filename}'s blob at commit {commit}"))?;
    if blob.size() > limits::MAX_CONTROL_FILE_BYTES {
        return Ok(ControlFileRead::TooLarge);
    }
    Ok(ControlFileRead::Blob(blob.content().to_vec()))
}

/// The operator-facing message for a halted branch (decisions/0037): names
/// the branch, the offending commit, exactly which control file differs, and
/// the remedy — update the pinned digest to the approved policy, or
/// reconcile the branch.
fn policy_mismatch_message(branch: &str, mismatch: &PolicyMismatch) -> String {
    let problem = match mismatch.reason {
        PolicyMismatchReason::DiffersFromPinnedPolicy => {
            format!(
                "carries a {} that differs from the pinned policy",
                mismatch.filename
            )
        }
        PolicyMismatchReason::NotARegularFile => {
            format!("carries a {} that is not a regular file", mismatch.filename)
        }
        PolicyMismatchReason::ExceedsSizeLimit => format!(
            "carries a {} that exceeds the {} byte limit",
            mismatch.filename,
            limits::MAX_CONTROL_FILE_BYTES
        ),
    };
    format!(
        "{branch:?} halted — commit {} {problem}; \
         update GITPRISM_POLICY_SHA256 to the approved policy (via `gitprism policy-hash`) \
         or reconcile the branch so its {} matches exactly",
        mismatch.commit, mismatch.filename
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
    for source_oid in pending {
        let source_commit = repo
            .find_commit(source_oid)
            .context("resolving a pending source commit")?;

        // Loop prevention (decisions/0003): a source commit that itself came
        // from dest (dest→source sync) already exists on dest — pushing it
        // back would loop. First thing in the loop now that there's no
        // cursor left to advance before it.
        if marker::verify(
            &source_commit,
            branch,
            &[MarkerDirection::Setup, MarkerDirection::DestToSource],
            None,
            key,
        )
        .is_some()
        {
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
                built_any = true;
            }
        }
    }

    Ok(PendingDestBuild {
        new_tip: built_any.then_some(parent),
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
        let Ok(merge_base) = repo.merge_base(branch_tip, landing_tip) else {
            continue;
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

/// `setup`'s own real graft between source and dest (decisions/0006) — the
/// one commit both sides actually share ancestry from. dest→source's
/// cherry-picks give dest content a *marker* commit on source (see
/// [`newest_dest_marker`]), but never change source's real ancestry with
/// dest, so this never moves once `setup` has run for the pair.
fn graft_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Oid> {
    repo.merge_base(source_tip, dest_tip).context(
        "no shared history between source and dest for this pair — has `gitprism setup` been run?",
    )
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
    if graft_point(repo, source_tip, dest_tip)? == dest_tip {
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

fn dest_tip_is_accounted_for(
    repo: &Repository,
    source_tip: Oid,
    dest_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<bool> {
    Ok(dest_tip_accounted_for(repo, source_tip, dest_tip, branch, key)? != DestTipAccountedFor::No)
}

/// Where source's pending-commit walk ([`pending_commits`], feeding
/// [`build_pending_dest_tip`]) resumes from — `None` if dest carries history
/// gitprism doesn't recognize as safe to build on at all.
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
///   [`pending_commits`] re-yield every source commit gitprism already
///   pushed to dest as soon as dest gained any independent content of its
///   own (a merged PR, say) — silently duplicating already-synced content on
///   dest, or hard-stopping every later sync on a bogus "conflict" if the
///   duplicate re-apply doesn't happen to apply cleanly. That was this
///   function's actual bug before this split.
///
/// When the newest marker isn't usable — missing from this clone's odb
/// entirely, or found but not actually an ancestor of `source_tip` — the
/// answer is to refuse (`Ok(None)`) rather than fall back to an older marker
/// or to the graft: an older boundary would make [`pending_commits`]
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
        // avoid one extra `merge_base` call.
        return Ok(Some(graft_point(repo, source_tip, dest_tip)?));
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
fn dest_resume_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Option<Oid>> {
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
/// fetched) is genuinely ambiguous on a shallow clone — rewritten, or just
/// not fetched back far enough yet — so it does not count as a detected
/// rewrite there; [`dest_resume_point_for_branch`]'s ordinary refusal
/// message stands for that case. On a non-shallow clone there is no other
/// explanation left, so it does count as a detected rewrite instead
/// (decisions/0039's addendum, via `Repository::is_shallow`). Any other
/// `find_commit` failure is a real error and propagates as `Err`, never
/// guessed either way.
fn mirror_only_rewrite_detected(
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
            // decisions/0039 addendum "a missing boundary object is
            // confirmed as a rewrite on a non-shallow clone".
            return Ok(!repo.is_shallow());
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

/// Loads the exclude-list *current* as of `source_tip` — the version this
/// whole sync run filters every pending commit with (decisions/0004: the
/// current list applies to whatever's being processed right now, not a
/// historical reconstruction of what it looked like at each commit).
#[cfg(test)]
fn load_current_exclude_list(repo: &Repository, source_tip: Oid) -> Result<ExcludeList> {
    let tree = repo
        .find_commit(source_tip)
        .context("resolving source's tip commit")?
        .tree()
        .context("reading source's tip tree")?;
    let ignore_raw = match tree.get_path(Path::new(exclude::FILENAME)) {
        Ok(entry) => {
            let blob = repo
                .find_blob(entry.id())
                .context("reading .gitprismignore's blob")?;
            String::from_utf8_lossy(blob.content()).into_owned()
        }
        Err(_) => String::new(),
    };
    ExcludeList::from_contents(&ignore_raw).context("parsing .gitprismignore")
}

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
        let is_dir = entry.kind() == Some(git2::ObjectType::Tree);

        if exclude_list.is_excluded(&rel_path, is_dir) {
            continue;
        }

        if is_dir {
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
fn empty_tree(repo: &Repository) -> Result<Oid> {
    repo.treebuilder(None)
        .context("starting an empty tree builder")?
        .write()
        .context("writing the empty tree")
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

/// Cherry-picks dest's pending commits on `branch` onto source's same-named
/// branch and pushes the result to source's own remote (decisions/0013), one
/// branch at a time — `branch` always comes from `config.branches`
/// (decisions/0017), the explicit, small set of branches dest content is
/// ported back for. A real conflict hard-stops this branch (decisions/0007):
/// whatever applied cleanly before it is still pushed, and the conflict is
/// reported with enough detail for `gitprism resolve` (decisions/0008) to act
/// on later — no trailer is written for the unresolved commit, so the next
/// run's resume-scan naturally retries it once it's resolved.
#[cfg(test)]
fn sync_pair_from_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    reporter: &Reporter,
) -> Result<()> {
    let key = marker::load_key()?;
    sync_pair_from_dest_with_key(repo, source_root, config, branch, reporter, &key)
}

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
fn preflight_local_source_branch(repo: &Repository, branch: &str, new_tip: Oid) -> Result<Oid> {
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

fn git_paths_conflict(left: &[u8], right: &[u8]) -> bool {
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
fn advance_local_source_branch(
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

/// Where dest→source resumes from: the most recent commit reachable from
/// `source_tip` carrying a `Gitprism-Dest-Commit` trailer (decisions/0003),
/// returned as `(that source commit's own oid, the dest-space oid it names)`.
/// Both halves matter to callers: [`sync_pair_from_dest`] only needs the
/// dest-space value, but [`dest_resume_point`]'s cherry-pick-recognition case
/// needs the *source* oid too — unlike source→dest's boundary (a real
/// ancestor by construction, either the original graft or a prior push),
/// dest→source's marker commit has no real ancestry link back to the dest
/// commit it names (cherry-picking a commit doesn't make the original an
/// ancestor of the copy), so only the marker's own identity can stand in for
/// it in a revwalk.
///
/// Source's tip routinely moves for reasons that have nothing to do with
/// dest→source (ordinary source-side development), so this has to actually
/// scan source's history rather than look at the tip alone. The same is true
/// of dest's tip and source→dest — gitprism is not dest's sole writer, which
/// is the entire premise dest→source exists to handle — so that direction
/// scans too ([`newest_source_marker`]); the tip-only check that remains
/// there ([`dest_tip_is_accounted_for`]) answers a *safety* question, not a
/// resume question.
///
/// `Ok(None)` means no `Gitprism-Dest-Commit` trailer is reachable at all —
/// for a properly set-up branch this never happens (setup's own graft commit,
/// decisions/0006, always carries one and is always a first-parent ancestor),
/// but a branch discovered on source with no ancestry to any grafted branch
/// (decisions/0017, decisions/0024) genuinely has none. Which of those two
/// meanings applies is the caller's call — see [`newest_dest_marker`] and
/// [`newest_dest_marker_opt`] below.
///
/// The walk is first-parent-only (decisions/0019,
/// `Revwalk::simplify_first_parent()`) — full ancestry used to mean a
/// mirror-only branch merged into this one via a real, two-parent merge
/// could hand this scan *that* branch's own `Gitprism-Dest-Commit` trailer
/// (reachable only through the merge's non-first parent) instead of this
/// branch's, once decisions/0017 made every branch on source eligible to be
/// merged into another. First-parent-only makes that unreachable: a merge
/// commit's non-first parents, and everything reachable only through them,
/// are never visited. This relies on the tracked branch staying first-parent
/// of its own merges — true for GitHub/GitLab/Azure DevOps' "merge PR"
/// button and for `git merge` run from the target branch, not guaranteed
/// otherwise (decisions/0019's documented limitation).
fn scan_for_dest_marker(
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
/// [`dest_tip_is_accounted_for`] and [`pending_dest_commits`], whose branch
/// always comes from `config.branches`, where `setup` (decisions/0006,
/// decisions/0023) guarantees the trailer exists. Finding none really does
/// mean "has `gitprism setup` been run for this pair?"
fn newest_dest_marker(
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

/// [`scan_for_dest_marker`], `Option`-returning (mirrors
/// [`newest_source_marker`]'s shape) — used only by `sync_pair_to_dest`'s
/// no-dest-ref case (decisions/0017), where the branch was discovered on
/// source rather than read from `config.branches`, so `setup` gives no
/// guarantee it shares any ancestry with dest at all. `None` there is a real,
/// expected outcome (decisions/0024) — a genuinely unrelated pre-existing
/// local branch — not a bug to bail on; the caller decides how to report it.
fn newest_dest_marker_opt_for_branch(
    repo: &Repository,
    source_tip: Oid,
    branch: &str,
    key: &marker::StateKey,
) -> Result<Option<(Oid, Oid)>> {
    scan_for_dest_marker(repo, source_tip, branch, key)
}

/// decisions/0043: `branch`'s dest-space anchor — [`newest_dest_marker_opt_for_branch`]'s
/// own baseline scan, refined by a search for a more specific anchor among
/// sibling branches discovered this run. A branch forked from another
/// already-mirrored branch (round-tripped or mirror-only) anchors its dest
/// chain on that branch's own mirror at their real merge-base, instead of
/// always falling back to the nearest round-tripped marker the baseline
/// scan finds by walking straight past every unmarked commit in between.
///
/// Shared by both of [`sync_pair_to_dest_with_key`]'s call sites — a
/// brand-new branch's own first mirror, and decisions/0039's rewrite-rebuild
/// arm — so both benefit with no second implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DestAnchor {
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
}

fn dest_anchor_for_branch(
    repo: &Repository,
    source_root: &Path,
    dest_url: &str,
    branch: &str,
    source_tip: Oid,
    key: &marker::StateKey,
) -> Result<DestAnchor> {
    let Some((boundary_base, dest_tip_base)) =
        newest_dest_marker_opt_for_branch(repo, source_tip, branch, key)?
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
    for candidate in list_source_branches(repo)? {
        if candidate == branch {
            continue;
        }
        if !git::remote_ref_exists(source_root, dest_url, &candidate)
            .with_context(|| format!("checking whether {candidate:?} has a dest ref"))?
        {
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
        let Ok(cbase) = repo.merge_base(source_tip, candidate_tip) else {
            continue;
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

    // The unique most-specific survivor: undominated by any other
    // survivor's merge-base, via `graph_descendant_of` — the same primitive
    // decisions/0039's own condition 4 already uses. A tie strictly at
    // `boundary_base` is always dominated by any real refinement above (per
    // the check just above, at least one exists here), so it never reaches
    // `maximal`. Two or more genuinely incomparable *refinements* are what
    // leaves more than one maximal survivor, handled below.
    let mut maximal: Vec<(String, Oid)> = Vec::new();
    for (index, candidate) in survivors.iter().enumerate() {
        let mut dominated = false;
        for (other_index, other) in survivors.iter().enumerate() {
            if index == other_index || other.1 == candidate.1 {
                continue;
            }
            if repo
                .graph_descendant_of(other.1, candidate.1)
                .with_context(|| {
                    format!(
                        "comparing sibling candidate merge-bases {} and {}",
                        other.1, candidate.1
                    )
                })?
            {
                dominated = true;
                break;
            }
        }
        if !dominated {
            maximal.push(candidate.clone());
        }
    }

    let (winner, cbase) = match maximal.len() {
        // Every survivor dominated by another is impossible for a nonempty
        // list under git's acyclic ancestry order, but treated as "the
        // search found nothing better" rather than panicking.
        0 => return Ok(DestAnchor::Resolved(boundary_base, dest_tip_base)),
        1 => maximal.into_iter().next().expect("checked len == 1"),
        _ => return Ok(DestAnchor::Ambiguous(maximal)),
    };

    git::fetch(source_root, dest_url, &winner)
        .with_context(|| format!("fetching sibling candidate {winner:?} from configured remote"))?;
    let winner_dest_tip = repo
        .find_reference("FETCH_HEAD")
        .context("reading FETCH_HEAD after fetching a sibling candidate")?
        .peel_to_commit()
        .context("resolving a fetched sibling candidate to a commit")?
        .id();

    match newest_source_marker_at_or_before(repo, winner_dest_tip, &winner, cbase, key)? {
        Some((found_dest_oid, found_source_oid)) => {
            Ok(DestAnchor::Resolved(found_source_oid, found_dest_oid))
        }
        // The winning candidate's own dest history carries nothing at or
        // before the shared merge-base (e.g. it's a round-tripped candidate
        // that has never itself been synced beyond `setup`'s own graft) —
        // degrades to the existing baseline rather than to a worse or
        // unsafe result (decisions/0043's own "Why": a bug in the new
        // search must never produce something less safe than today).
        None => Ok(DestAnchor::Resolved(boundary_base, dest_tip_base)),
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
        if scanned >= limits::MAX_MARKER_SCAN_COMMITS {
            anyhow::bail!(
                "sibling candidate resume-point scan exceeds the {} commit limit",
                limits::MAX_MARKER_SCAN_COMMITS
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

/// decisions/0043's per-branch hard-fail message for
/// [`DestAnchor::Ambiguous`] — names every equally specific candidate and
/// its merge-base oid, matching decisions/0007's and decisions/0023's own
/// hard-fail precedent of naming exactly what's ambiguous rather than
/// guessing, but surfaced as a per-branch halt (decisions/0024's
/// precedent), not a whole-run abort.
fn ambiguous_anchor_message(branch: &str, candidates: &[(String, Oid)]) -> String {
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

/// dest's own resume boundary for [`pending_commits`]: the most recent
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
fn newest_source_marker(
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

/// Every dest commit still pending reconciliation onto source, oldest first,
/// with loop-prevention already applied (decisions/0003) — exactly what
/// [`sync_pair_from_dest`] would attempt to build next. Pulled out as its own
/// function so `gitprism resolve` (decisions/0008, 0015) can compute the
/// identical list — resolve and sync must never disagree about which dest
/// commit is next.
///
/// `boundary` names a dest-space commit (via [`newest_dest_marker`]); it's
/// verified to actually be an ancestor of (or equal to) `dest_tip` before
/// trusting it to scope the walk — dest is fast-forward-only in normal
/// operation (requirements/0001), so this should always hold, but a missing
/// object or a genuine non-ancestor both mean something is wrong enough to
/// fail loudly rather than silently mis-walk.
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
mod tests {
    use std::fs;
    use std::io::Write;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    /// A bare repo with one commit on `branch` — dest is always reached over
    /// a remote URL in real use, so its fixture is bare here too, unlike
    /// `setup`'s (which only ever fetches from dest, never pushes to it).
    fn bare_repo_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> Oid {
        let repo = Repository::init_bare(dir).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Author", "author@example.com").unwrap();

        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "initial",
                &tree,
                &[],
            )
            .unwrap();
        // `Repository::init_bare` points HEAD at libgit2's environment
        // default branch, which need not be `branch` (e.g. it's "master"
        // in CI). Repoint it so push_head() in tests resolves correctly
        // regardless of that default.
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        oid
    }

    /// `source_url` only actually gets dereferenced (fetched from or pushed
    /// to) when a pair has something dest→source needs to push — plenty of
    /// tests below never reach that path and pass `"unused"`, same
    /// convention `setup`'s own tests use for an irrelevant `[dest].url`.
    fn write_config(source_url: &str, dest_url: &str, branches: &[&str]) -> NamedTempFile {
        let branches_toml: String = branches
            .iter()
            .map(|branch| format!("{branch:?}"))
            .collect::<Vec<_>>()
            .join(", ");

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
            branches = [{branches_toml}]

            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [source]
            url = '{source_url}'

            [dest]
            url = '{dest_url}'
            "#,
        )
        .unwrap();
        file
    }

    /// A bare repo seeded at `tip` on `branch`, standing in for source's own
    /// real remote — the push target dest→source uses (decisions/0013). The
    /// local `source_repo` fixtures below are non-bare working checkouts, so
    /// pushing dest→source's result *into* them directly would hit git's own
    /// `receive.denyCurrentBranch` guard; a separate bare "upstream" avoids
    /// that entirely, matching how a real CI checkout's `origin` is a
    /// different (bare, hosted) repo from the checkout itself.
    fn bare_source_remote_seeded_at(
        source_repo: &Repository,
        branch: &str,
        tip: Oid,
    ) -> tempfile::TempDir {
        let dir = tempdir().unwrap();
        Repository::init_bare(dir.path()).unwrap();
        let outcome = git::push(
            source_repo.workdir().unwrap(),
            &dir.path().display().to_string(),
            tip,
            branch,
            PushMode::FastForwardOnly,
        )
        .unwrap();
        assert_eq!(
            outcome,
            git::PushOutcome::Accepted,
            "seeding the fake source remote at its own graft tip must succeed"
        );
        dir
    }

    /// Sets up a source repo already grafted onto `dest`'s tip — exactly
    /// `gitprism setup`'s output shape (decisions/0006) — without depending
    /// on `commands::setup` itself, so these tests exercise `sync` in
    /// isolation.
    fn source_grafted_onto(
        source_dir: &Path,
        branch: &str,
        dest_tip: Oid,
        dest_repo: &Repository,
    ) -> Repository {
        let repo = Repository::init(source_dir).unwrap();
        let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
        // Re-read the dest tip's tree into the source repo's own odb via a
        // round trip through the filesystem, same as a real fetch would.
        git::fetch(source_dir, &dest_repo.path().to_string_lossy(), branch).unwrap();
        let fetched_tip_id = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        {
            let fetched_tip = repo.find_commit(fetched_tip_id).unwrap();
            let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
            let tree = fetched_tip.tree().unwrap();
            let message = marker::build_message(
                &format!("gitprism setup: graft ({})", source_dir.display()),
                MarkerDirection::Setup,
                branch,
                dest_tip_commit.id(),
                "Gitprism-Dest-Commit",
                &[fetched_tip.id()],
                tree.id(),
                &signature,
                &signature,
                &marker::load_key().unwrap(),
            );
            repo.commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&fetched_tip],
            )
            .unwrap();
        }
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(None).unwrap();
        // decisions/0034: a real `setup` run restores the two control files
        // byte-exact right after this same checkout, so a graft this helper
        // fabricates must too — otherwise a host with `core.autocrlf=true`
        // (Windows CI's default) mangles a checked-out `.gitprismignore`'s
        // line endings, and a later `sync_pair_to_dest` call sees that
        // mangled copy disagree with the identical blob it inherited,
        // failing decisions/0037's policy check for a reason that has
        // nothing to do with an actual content change.
        let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
        policy::restore_control_files_exact(&repo, &head_tree).unwrap();
        drop(head_tree);
        repo
    }

    fn add_commit(repo: &Repository, branch: &str, files: &[(&str, &str)]) -> Oid {
        add_commit_with_message(repo, branch, files, "a real change")
    }

    fn add_commit_with_message(
        repo: &Repository,
        branch: &str,
        files: &[(&str, &str)],
        message: &str,
    ) -> Oid {
        if let Some((_, contents)) = files.iter().find(|(name, _)| *name == exclude::FILENAME) {
            fs::write(
                repo.workdir().unwrap().join(exclude::FILENAME),
                contents.as_bytes(),
            )
            .unwrap();
        }
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                message,
                &tree,
                &[&tip],
            )
            .unwrap();
        refresh_checked_out_branch(repo, branch);
        oid
    }

    fn refresh_checked_out_branch(repo: &Repository, branch: &str) {
        let refname = format!("refs/heads/{branch}");
        let checked_out = repo
            .head()
            .ok()
            .and_then(|head| head.name().ok().map(str::to_owned))
            .is_some_and(|name| name == refname);
        if checked_out {
            let commit = repo
                .find_branch(branch, git2::BranchType::Local)
                .unwrap()
                .get()
                .peel_to_commit()
                .unwrap();
            repo.checkout_tree(
                commit.as_object(),
                Some(git2::build::CheckoutBuilder::new().force()),
            )
            .unwrap();
            let mut index = repo.index().unwrap();
            index.read_tree(&commit.tree().unwrap()).unwrap();
            index.write().unwrap();
        }
    }

    /// A fresh, independent clone of `branch` off `source_repo`, mimicking
    /// what a real CI checkout gives gitprism: only objects reachable from
    /// `branch`'s current tip, nothing this repo's own working history still
    /// happens to have lying around unreachable in its object database.
    /// `shallow` controls whether the clone is `--depth=1` (so
    /// `Repository::is_shallow` reports true) or a plain, full fetch of
    /// everything reachable (not shallow, but — unlike the in-place
    /// `branch(..., force: true)` rewrites the other rewrite tests use —
    /// still genuinely missing anything `source_repo` itself no longer
    /// reaches from a ref, e.g. a pre-amend/pre-rebase/pre-reset tip).
    fn fresh_clone_of_branch(
        source_repo: &Repository,
        branch: &str,
        shallow: bool,
    ) -> (tempfile::TempDir, Repository) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let url = source_repo.path().display().to_string();
        let refspec = format!("refs/heads/{branch}");
        if shallow {
            git::fetch_shallow(dir.path(), &url, branch, 1).unwrap();
        } else {
            git::fetch(dir.path(), &url, branch).unwrap();
        }
        {
            let fetched_tip = repo
                .find_reference("FETCH_HEAD")
                .unwrap()
                .peel_to_commit()
                .unwrap();
            repo.branch(branch, &fetched_tip, false).unwrap();
        }
        repo.set_head(&refspec).unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        (dir, repo)
    }

    fn add_dest_marker_commit(
        repo: &Repository,
        branch: &str,
        parent: Oid,
        counterpart: Oid,
    ) -> Oid {
        let parent_commit = repo.find_commit(parent).unwrap();
        let tree = parent_commit.tree().unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let message = marker::build_message(
            "gitprism sync: dest -> source",
            MarkerDirection::DestToSource,
            branch,
            counterpart,
            "Gitprism-Dest-Commit",
            &[parent],
            tree.id(),
            &signature,
            &signature,
            &marker::load_key().unwrap(),
        );
        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent_commit],
            )
            .unwrap();
        refresh_checked_out_branch(repo, branch);
        oid
    }

    /// Same shape as `add_commit`, for content that isn't valid UTF-8 (e.g. a
    /// binary blob with NUL/high bytes). `add_commit` takes `&str` contents
    /// because every other fixture only ever needs text; bending its
    /// signature to accept raw bytes would make every existing text-only call
    /// site less readable for no benefit, so this is a small sibling instead.
    fn add_commit_bytes(repo: &Repository, branch: &str, files: &[(&str, &[u8])]) -> Oid {
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "a binary change",
                &tree,
                &[&tip],
            )
            .unwrap();
        refresh_checked_out_branch(repo, branch);
        oid
    }

    /// Same shape as `add_commit`, for a commit that also *removes* paths — a
    /// rename is a removal plus an insertion, and `add_commit` can only
    /// insert.
    fn add_commit_removing(
        repo: &Repository,
        branch: &str,
        removals: &[&str],
        files: &[(&str, &str)],
    ) -> Oid {
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        for removal in removals {
            builder.remove(Path::new(removal)).unwrap();
        }
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "a rename",
                &tree,
                &[&tip],
            )
            .unwrap();
        refresh_checked_out_branch(repo, branch);
        oid
    }

    /// An independent change landing directly on dest — e.g. a PR merged
    /// straight to dest — content gitprism never put there. `message` is
    /// exposed (rather than fixed, like `add_commit`'s) so tests can stamp a
    /// `Gitprism-Source-Commit` trailer onto it for loop-prevention coverage.
    fn add_independent_dest_commit(
        dest_repo: &Repository,
        parent: Oid,
        file: (&str, &str),
        message: &str,
    ) -> Oid {
        let parent_commit = dest_repo.find_commit(parent).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(file.1.as_bytes()).unwrap();
        builder
            .insert(file.0, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        let message = if let Some(raw) =
            message.strip_prefix("gitprism sync: source -> dest\n\nGitprism-Source-Commit: ")
        {
            let source_oid = Oid::from_str(raw.trim()).unwrap();
            marker::build_message(
                "gitprism sync: source -> dest",
                MarkerDirection::SourceToDest,
                "main",
                source_oid,
                "Gitprism-Source-Commit",
                &[parent],
                tree.id(),
                &signature,
                &signature,
                &marker::load_key().unwrap(),
            )
        } else {
            message.to_owned()
        };
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent_commit],
            )
            .unwrap()
    }

    /// Same shape as [`add_independent_dest_commit`], but for a branch other
    /// than "main" — decisions/0017's discovered branches don't all mirror to
    /// "main" on dest, so tests covering them need an independent-dest-commit
    /// fixture parameterized by branch too.
    fn add_independent_dest_commit_on(
        dest_repo: &Repository,
        branch: &str,
        parent: Oid,
        file: (&str, &str),
        message: &str,
    ) -> Oid {
        let parent_commit = dest_repo.find_commit(parent).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(file.1.as_bytes()).unwrap();
        builder
            .insert(file.0, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        let message = if let Some(raw) =
            message.strip_prefix("gitprism sync: source -> dest\n\nGitprism-Source-Commit: ")
        {
            let source_oid = Oid::from_str(raw.trim()).unwrap();
            marker::build_message(
                "gitprism sync: source -> dest",
                MarkerDirection::SourceToDest,
                branch,
                source_oid,
                "Gitprism-Source-Commit",
                &[parent],
                tree.id(),
                &signature,
                &signature,
                &marker::load_key().unwrap(),
            )
        } else {
            message.to_owned()
        };
        dest_repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent_commit],
            )
            .unwrap()
    }

    /// Builds a dest commit stamped exactly as `build_dest_commit` would
    /// have — a `Gitprism-Source-Commit` trailer naming `source_oid` — for
    /// hand-constructing a dest history shaped like a prior sync run's
    /// output (decisions/0035's migration case) without going through `run`.
    fn add_source_marker_commit_on_dest(
        dest_repo: &Repository,
        branch: &str,
        parent: Oid,
        file: (&str, &str),
        source_oid: Oid,
    ) -> Oid {
        let parent_commit = dest_repo.find_commit(parent).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(file.1.as_bytes()).unwrap();
        builder
            .insert(file.0, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let message = marker::build_message(
            "gitprism sync: source -> dest",
            MarkerDirection::SourceToDest,
            branch,
            source_oid,
            "Gitprism-Source-Commit",
            &[parent],
            tree.id(),
            &signature,
            &signature,
            &marker::load_key().unwrap(),
        );
        dest_repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent_commit],
            )
            .unwrap()
    }

    #[test]
    fn run_pushes_a_new_source_commit_to_dest_filtered() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // `.gitprismignore` is versioned in source (decisions/0011) — it has
        // to be part of the commit itself, not just written to the
        // filesystem, since `sync` filters using each commit's *own* tree.
        add_commit(
            &source_repo,
            "main",
            &[
                ("shared.txt", "v2"),
                ("secret.txt", "only for source"),
                (exclude::FILENAME, "secret.txt\n"),
            ],
        );

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(new_dest_tip.parent_id(0).unwrap(), dest_tip);
        assert_eq!(new_dest_tip.author().name().unwrap(), "A Developer");
        assert_eq!(
            new_dest_tip.committer().email().unwrap(),
            "gitprism@example.com"
        );
        assert!(
            new_dest_tip
                .message()
                .unwrap()
                .contains("Gitprism-Source-Commit:")
        );

        let tree = new_dest_tip.tree().unwrap();
        let shared = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared.content(), b"v2");
        assert!(
            tree.get_name("secret.txt").is_none(),
            "an excluded file must never reach dest"
        );
        assert!(
            tree.get_name(exclude::FILENAME).is_none(),
            ".gitprismignore itself must never reach dest"
        );
    }

    #[test]
    fn run_pushes_a_new_binary_file_to_dest_byte_identical() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // NUL and high bytes — not valid UTF-8, and exactly the kind of
        // content libgit2 flags as a "binary" delta. With no `DiffOptions`,
        // `diff_tree_to_tree` omits the binary payload entirely, so
        // `apply_to_tree` can't reconstruct this file and misreports it as a
        // decisions/0007 content conflict instead of applying it.
        let binary_content: &[u8] = &[
            0x00, 0xFF, 0x01, 0xFE, b'b', b'i', b'n', 0x00, 0x89, b'P', b'N', b'G',
        ];
        add_commit_bytes(&source_repo, "main", &[("blob.bin", binary_content)]);

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path())
            .expect("sync should succeed and carry the binary file to dest");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_dest_tip.tree().unwrap();
        let entry = tree
            .get_name("blob.bin")
            .expect("the binary file must reach dest");
        let blob = dest_repo.find_blob(entry.id()).unwrap();
        assert_eq!(
            blob.content(),
            binary_content,
            "the binary file's content must reach dest byte-identical"
        );
    }

    #[test]
    fn run_pushes_nothing_when_the_only_pending_commit_filters_to_empty() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &source_repo,
            "main",
            &[
                ("secret.txt", "only for source"),
                (exclude::FILENAME, "secret.txt\n"),
            ],
        );

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path())
            .expect("sync should succeed even with nothing to push");

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip,
            "dest must not move when every pending commit filters to empty"
        );
    }

    #[test]
    fn run_resumes_from_the_last_synced_commit_not_the_graft() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("first sync should succeed");
        // A second run with nothing new on source must be a true no-op, not
        // re-walk all the way back to the graft and re-push v2 again.
        run(source_dir.path(), config.path()).expect("second, no-op sync should succeed");

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            2,
            "the no-op run must not add another commit"
        );
    }

    #[test]
    fn run_does_not_reflect_a_dest_originated_commit_back_to_dest() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Simulate dest→source having already reflected a dest commit back
        // onto source: a commit carrying Gitprism-Dest-Commit.
        let tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = source_repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        let blob = source_repo.blob(b"from dest").unwrap();
        builder
            .insert("shared.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let message = marker::build_message(
            "gitprism resolve",
            MarkerDirection::DestToSource,
            "main",
            dest_tip,
            "Gitprism-Dest-Commit",
            &[tip.id()],
            tree.id(),
            &signature,
            &signature,
            &marker::load_key().unwrap(),
        );
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &message,
                &tree,
                &[&tip],
            )
            .unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip,
            "a commit that already came from dest must not be pushed back to dest"
        );
    }

    #[test]
    fn run_does_not_trust_a_source_authored_mapping_trailer() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit_with_message(
            &source_repo,
            "main",
            &[("shared.txt", "source change")],
            &format!("ordinary source commit\n\nGitprism-Dest-Commit: {dest_tip}\n"),
        );

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("a forged trailer is ordinary user text");

        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let blob = dest_repo
            .find_blob(
                dest_tip
                    .tree()
                    .unwrap()
                    .get_name("shared.txt")
                    .unwrap()
                    .id(),
            )
            .unwrap();
        assert_eq!(blob.content(), b"source change");
    }

    #[test]
    fn run_applies_the_current_exclude_list_even_to_an_already_committed_secret() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // First commit adds a secret with no exclude rule in effect yet.
        add_commit(
            &source_repo,
            "main",
            &[("shared.txt", "v2"), ("secret.txt", "leaked?")],
        );
        // A later commit adds the exclude rule, but never touches
        // secret.txt itself — decisions/0004 says the *current* list
        // governs everything being processed this run, not a per-commit
        // historical snapshot, so this must still catch the earlier commit.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "secret.txt\n")]);

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("secret.txt").is_none(),
            "the current exclude-list must apply retroactively to an already-committed secret, not just commits made after the rule existed"
        );
    }

    #[test]
    fn run_reflects_an_independent_dest_commit_into_source_and_still_syncs_source_to_dest() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("only-in-source.txt", "v2")]);
        let source_remote = bare_source_remote_seeded_at(
            &source_repo,
            "main",
            source_repo
                .find_branch("main", git2::BranchType::Local)
                .unwrap()
                .get()
                .peel_to_commit()
                .unwrap()
                .id(),
        );

        // An independent change landing directly on dest (e.g. a PR merged
        // straight to dest) — content gitprism never put there and doesn't
        // know about yet. This must reach source (dest→source), and source's
        // own pending commit must still reach dest in the very same run
        // (source→dest) — neither direction blocks the other.
        let dest_only_tip = {
            let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
            let mut builder = dest_repo
                .treebuilder(Some(&dest_tip_commit.tree().unwrap()))
                .unwrap();
            let blob = dest_repo.blob(b"dest-only content").unwrap();
            builder
                .insert("dest-only.txt", blob, git2::FileMode::Blob.into())
                .unwrap();
            let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
            let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
            dest_repo
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "an independent dest-side change",
                    &tree,
                    &[&dest_tip_commit],
                )
                .unwrap()
        };

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("both directions should succeed");

        // dest→source: the independent commit landed on source's real
        // remote, cherry-picked, author preserved, gitprism as committer.
        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let new_source_tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(new_source_tip.author().name().unwrap(), "Dest Maintainer");
        assert_eq!(
            new_source_tip.committer().email().unwrap(),
            "gitprism@example.com"
        );
        assert!(
            new_source_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_only_tip}"))
        );
        let new_source_tree = new_source_tip.tree().unwrap();
        assert!(
            new_source_tree.get_name("dest-only.txt").is_some(),
            "dest's independent content must reach source"
        );
        assert!(
            new_source_tree.get_name("only-in-source.txt").is_some(),
            "source's own pre-existing content must survive the cherry-pick"
        );

        // The local checkout's own branch ref must have advanced to match
        // what was just pushed — later same-run logic (and any future git
        // command against this checkout) needs to see it.
        let local_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(local_tip.id(), new_source_tip.id());
        // The working directory must actually reflect it too, not just the
        // ref — this branch is the one checked out in `source_dir`, so a
        // ref-only move would leave `dest-only.txt` missing on disk (and the
        // checkout looking dirty relative to its own HEAD).
        assert_eq!(
            fs::read_to_string(source_dir.path().join("dest-only.txt")).unwrap(),
            "dest-only content",
            "dest→source must check the working tree out, not just move the branch ref"
        );

        // source→dest: source's own pending commit still reached dest, in
        // this same run, even though dest→source ran first.
        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(new_dest_tip.parent_id(0).unwrap(), dest_only_tip);
        let new_dest_tree = new_dest_tip.tree().unwrap();
        assert!(new_dest_tree.get_name("only-in-source.txt").is_some());
        assert!(
            new_dest_tree.get_name("dest-only.txt").is_some(),
            "dest's own pre-existing content must survive the filtered push"
        );
    }

    #[test]
    fn run_refuses_to_sync_a_divergent_clone_even_though_dest_has_a_trailer() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        // Two independent clones of the same freshly-grafted source, exactly
        // like two checkouts of one real repo — clone A syncs first.
        let clone_a_dir = tempdir().unwrap();
        let clone_a = source_grafted_onto(clone_a_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_a,
            "main",
            &[("shared.txt", "vA"), ("only-a.txt", "from A")],
        );
        let config_a = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(clone_a_dir.path(), config_a.path()).expect("clone A's sync should succeed");

        let dest_tip_after_a = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // Clone B was grafted from the *same* original dest tip, before A's
        // push — its own source_tip is a sibling of A's commit, not a
        // descendant of it. Dest's tip now carries a
        // `Gitprism-Source-Commit` trailer naming A's commit, which is not
        // an ancestor of clone B's source_tip at all.
        let clone_b_dir = tempdir().unwrap();
        let clone_b = source_grafted_onto(clone_b_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_b,
            "main",
            &[("shared.txt", "vB"), ("only-b.txt", "from B")],
        );
        let config_b = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        let err = run(clone_b_dir.path(), config_b.path()).expect_err(
            "a divergent clone must not rebuild its own snapshot on top of dest just because dest's tip has *some* Gitprism-Source-Commit trailer",
        );
        assert!(format!("{err:#}").contains("diverged"));

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip_after_a,
            "a refused sync must not touch dest's branch at all"
        );
        let tree = still_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("only-a.txt").is_some(),
            "clone A's already-synced content must survive clone B's refused sync"
        );
        assert!(
            tree.get_name("only-b.txt").is_none(),
            "clone B's content must never have been pushed"
        );
    }

    #[test]
    fn run_refuses_a_divergent_clone_even_when_dests_tip_is_an_independent_commit() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        // Clone A syncs first, same as the sibling test above.
        let clone_a_dir = tempdir().unwrap();
        let clone_a = source_grafted_onto(clone_a_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_a,
            "main",
            &[("shared.txt", "vA"), ("only-a.txt", "from A")],
        );
        let config_a = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(clone_a_dir.path(), config_a.path()).expect("clone A's sync should succeed");

        let dest_tip_after_a = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // An independent commit lands directly on dest afterward, e.g. a
        // merged PR — dest's tip is now this commit, not a gitprism-written
        // one.
        let independent = add_independent_dest_commit(
            &dest_repo,
            dest_tip_after_a,
            ("dest-only.txt", "from a merged PR"),
            "an independent dest-side change",
        );

        // Clone B was grafted from the *same original* dest tip, before A's
        // push — its own source_tip is a sibling of A's commit, not a
        // descendant of it.
        let clone_b_dir = tempdir().unwrap();
        let clone_b = source_grafted_onto(clone_b_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_b,
            "main",
            &[("shared.txt", "vB"), ("only-b.txt", "from B")],
        );
        let clone_b_tip = clone_b
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        // Clone B's dest→source will legitimately cherry-pick the
        // independent dest commit and push it — needs its own real source
        // remote for that push to land somewhere.
        let clone_b_remote = bare_source_remote_seeded_at(&clone_b, "main", clone_b_tip);
        let config_b = write_config(
            &clone_b_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        let err = run(clone_b_dir.path(), config_b.path()).expect_err(
            "a divergent clone must not rebuild its own snapshot on top of dest just because dest→source could reflect dest's independent tip into it",
        );
        assert!(format!("{err:#}").contains("diverged"));

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            independent,
            "a refused sync must not touch dest's branch at all"
        );
        let tree = still_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("only-a.txt").is_some(),
            "clone A's already-synced content must survive clone B's refused sync"
        );
        assert!(
            tree.get_name("only-b.txt").is_none(),
            "clone B's content must never have been pushed"
        );
    }

    #[test]
    fn run_does_not_reapply_an_already_synced_commit_after_an_independent_dest_commit() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let notes_commit = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", notes_commit);

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        let tip_after_first = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // An independent change lands directly on dest — e.g. a merged PR —
        // content gitprism never put there.
        add_independent_dest_commit(
            &dest_repo,
            tip_after_first,
            ("dest-only.txt", "x\n"),
            "dest: a merged PR",
        );

        run(source_dir.path(), config.path())
            .expect("second sync, after an independent dest commit, should still succeed");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let notes_blob = dest_repo
            .find_blob(tree.get_name("notes.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            notes_blob.content(),
            b"line1\n",
            "an already-synced commit must not be reapplied on top of itself"
        );

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            3,
            "dest history must be exactly: initial, the notes.txt push, the independent commit"
        );

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        let source_marker_count = revwalk
            .filter_map(|oid| oid.ok())
            .filter(|oid| {
                let commit = dest_repo.find_commit(*oid).unwrap();
                marker::parse(commit.message().unwrap_or(""))
                    .is_some_and(|state| state.counterpart == notes_commit)
            })
            .count();
        assert_eq!(
            source_marker_count, 1,
            "exactly one dest commit should carry a Gitprism-Source-Commit trailer naming the notes.txt commit"
        );

        let tip_before_third = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        run(source_dir.path(), config.path()).expect("third sync should succeed");
        let tip_after_third = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            tip_before_third, tip_after_third,
            "a third, no-op sync must not move dest's tip"
        );
    }

    #[test]
    fn run_does_not_conflict_on_an_already_synced_commit_after_an_independent_dest_commit() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let shared_commit = add_commit(&source_repo, "main", &[("shared.txt", "v2\n")]);
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", shared_commit);

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        let tip_after_first = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let independent = add_independent_dest_commit(
            &dest_repo,
            tip_after_first,
            ("dest-only.txt", "x\n"),
            "dest: a merged PR",
        );

        run(source_dir.path(), config.path())
            .expect("second sync, after an independent dest commit, should still succeed");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            dest_tip_commit.id(),
            independent,
            "with nothing new pending, dest's tip must still be the independent commit"
        );
        let tree = dest_tip_commit.tree().unwrap();
        let shared_blob = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared_blob.content(), b"v2\n");

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            3,
            "dest history must be exactly: initial, the shared.txt push, the independent commit"
        );

        run(source_dir.path(), config.path()).expect("third sync should succeed and move nothing");
        let tip_after_third = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            tip_after_third, independent,
            "the pair must not be permanently stuck — a later sync must still succeed and move nothing"
        );
    }

    #[test]
    fn run_does_not_duplicate_a_no_ff_merges_content_on_dest() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);

        // An ordinary `git merge --no-ff feature`: main hasn't moved since the
        // graft, so the merge's own tree is exactly f1's tree, with main's
        // tip as first parent and f1 as second.
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let main_tip = source_repo.find_commit(graft).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &f1_commit.tree().unwrap(),
                &[&main_tip, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let feature_blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            feature_blob.content(),
            b"line1\n",
            "the merge's own first-parent diff must not re-apply feature.txt's content on \
             top of what the revwalk already applied for f1 (pre-fix: b\"line1\\nline1\\n\")"
        );

        // The merge commit itself contributes nothing beyond its side branch
        // (main hadn't moved), so its filtered diff against the cursor (f1)
        // is empty — requirements/0001 forbids pushing an empty commit, so
        // dest gets exactly one gitprism commit for f1, not two.
        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            2,
            "dest history must be exactly: initial, one commit for f1 (the merge adds nothing)"
        );

        let tip_before_second = dest_tip_commit.id();
        run(source_dir.path(), config.path()).expect("second sync should succeed");
        let tip_after_second = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            tip_before_second, tip_after_second,
            "a second, no-op sync must not move dest's tip"
        );
    }

    #[test]
    fn run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

        // A merge commit on main with parents [a1, f1] whose tree carries
        // shared.txt, main.txt, and feature.txt.
        let a1_commit = source_repo.find_commit(a1).unwrap();
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let mut builder = source_repo
            .treebuilder(Some(&a1_commit.tree().unwrap()))
            .unwrap();
        let feature_entry = f1_commit
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .unwrap()
            .id();
        builder
            .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
            .unwrap();
        let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &merge_tree,
                &[&a1_commit, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        // decisions/0035 removed the interleaved-branch churn this comment
        // used to describe: feature's own commit (f1) is no longer emitted
        // by the first-parent-only walk, so dest never gets an intermediate
        // commit temporarily missing the other branch's file. Only the tip
        // is checked here regardless.
        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let shared_blob = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        let main_blob = dest_repo
            .find_blob(tree.get_name("main.txt").unwrap().id())
            .unwrap();
        let feature_blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared_blob.content(), b"v1\n");
        assert_eq!(main_blob.content(), b"m1\n");
        assert_eq!(
            feature_blob.content(),
            b"f1\n",
            "pre-fix: feature.txt's content is duplicated on dest's tip"
        );

        let tip_before_second = dest_tip_commit.id();
        run(source_dir.path(), config.path()).expect("second sync should succeed");
        let tip_after_second = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            tip_before_second, tip_after_second,
            "a second, no-op sync must not move dest's tip"
        );
    }

    #[test]
    fn run_honors_a_conflict_resolved_by_hand_inside_a_merge_commit() {
        // decisions/0035: R0 -> R1 -> R2 on main, R0 -> F1 on feature, both
        // R1 and F1 edit shared.txt differently, and M merges feature into
        // main with the conflict resolved by hand in M's own tree. Under the
        // full-DAG walk this hard-stops on a genuine merge-tree conflict
        // when F1 is replayed on its own; under the first-parent walk only
        // M itself is applied, carrying the human's resolution.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "base\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        add_commit(&source_repo, "main", &[("shared.txt", "landing\n")]);
        let r2 = add_commit(&source_repo, "main", &[("other.txt", "r2\n")]);

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("shared.txt", "feature\n")]);

        // M resolves the shared.txt conflict by hand: neither side's own
        // content, main's r2 as first parent, feature's f1 as second.
        let r2_commit = source_repo.find_commit(r2).unwrap();
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let mut builder = source_repo
            .treebuilder(Some(&r2_commit.tree().unwrap()))
            .unwrap();
        let resolved_blob = source_repo.blob(b"resolved-by-human\n").unwrap();
        builder
            .insert("shared.txt", resolved_blob, git2::FileMode::Blob.into())
            .unwrap();
        let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &merge_tree,
                &[&r2_commit, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect(
            "a merge's own hand-resolved conflict must not be re-litigated against the \
             feature branch's own diff",
        );

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let shared_blob = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            shared_blob.content(),
            b"resolved-by-human\n",
            "dest's tip must carry the human's resolution recorded in the merge commit itself"
        );
        let other_blob = dest_repo
            .find_blob(tree.get_name("other.txt").unwrap().id())
            .unwrap();
        assert_eq!(other_blob.content(), b"r2\n");
    }

    #[test]
    fn run_applies_a_clean_two_parent_merge_without_replaying_the_side_branchs_own_commit() {
        // decisions/0035: unlike `run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`
        // (which asserts only final content, identical under either walk),
        // this counts dest's own history to show feature's own commit is no
        // longer replayed onto dest individually — the merge is carried as
        // one net change against its first parent.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

        let a1_commit = source_repo.find_commit(a1).unwrap();
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let mut builder = source_repo
            .treebuilder(Some(&a1_commit.tree().unwrap()))
            .unwrap();
        let feature_entry = f1_commit
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .unwrap()
            .id();
        builder
            .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
            .unwrap();
        let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &merge_tree,
                &[&a1_commit, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("a clean two-parent merge should sync");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        assert!(tree.get_name("main.txt").is_some());
        assert!(tree.get_name("feature.txt").is_some());

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push(dest_tip_commit.id()).unwrap();
        assert_eq!(
            revwalk.count(),
            3,
            "dest history must be exactly: initial, one commit for a1, one for the merge — \
             feature's own commit (f1) must never be applied to dest on its own"
        );
    }

    #[test]
    fn pending_commits_still_hides_a_boundary_reachable_only_via_a_merges_second_parent() {
        // decisions/0035: hide()'s own ancestor-exclusion walks all of a
        // hidden commit's parents regardless of simplify_first_parent, which
        // only restricts what the walk *emits*. `boundary` here is a merge's
        // second parent; the root beneath it is also a first-parent ancestor
        // of `tip` and must still be excluded.
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let empty_tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();

        let root = repo
            .commit(None, &signature, &signature, "root", &empty_tree, &[])
            .unwrap();
        let root_commit = repo.find_commit(root).unwrap();
        let x = repo
            .commit(
                None,
                &signature,
                &signature,
                "x",
                &empty_tree,
                &[&root_commit],
            )
            .unwrap();
        let x_commit = repo.find_commit(x).unwrap();
        let c = repo
            .commit(
                None,
                &signature,
                &signature,
                "c",
                &empty_tree,
                &[&root_commit],
            )
            .unwrap();
        let c_commit = repo.find_commit(c).unwrap();

        let unrelated_root = repo
            .commit(
                None,
                &signature,
                &signature,
                "unrelated-root",
                &empty_tree,
                &[],
            )
            .unwrap();
        let unrelated_root_commit = repo.find_commit(unrelated_root).unwrap();
        let y = repo
            .commit(
                None,
                &signature,
                &signature,
                "y",
                &empty_tree,
                &[&unrelated_root_commit],
            )
            .unwrap();
        let y_commit = repo.find_commit(y).unwrap();

        // boundary: first parent y (unrelated to root), second parent x
        // (root's own child) — root is reachable from boundary only via its
        // second parent.
        let boundary = repo
            .commit(
                None,
                &signature,
                &signature,
                "boundary",
                &empty_tree,
                &[&y_commit, &x_commit],
            )
            .unwrap();
        let boundary_commit = repo.find_commit(boundary).unwrap();

        // merge: first parent c (root's other child, on tip's own
        // first-parent line), second parent boundary (already synced).
        let merge = repo
            .commit(
                None,
                &signature,
                &signature,
                "merge",
                &empty_tree,
                &[&c_commit, &boundary_commit],
            )
            .unwrap();
        let merge_commit = repo.find_commit(merge).unwrap();
        let tip = repo
            .commit(
                None,
                &signature,
                &signature,
                "tip",
                &empty_tree,
                &[&merge_commit],
            )
            .unwrap();

        let pending = pending_commits(&repo, boundary, tip).unwrap();
        assert_eq!(
            pending,
            vec![c, merge, tip],
            "root must stay hidden even though it's only reachable from `boundary` via a \
             merge's second parent, and is also a first-parent ancestor of tip via c"
        );
    }

    #[test]
    fn run_applies_a_squash_merged_source_commit_as_a_single_dest_commit() {
        // Regression case, not a fix case: `git merge --squash` never
        // records a second parent, so decisions/0035's
        // `simplify_first_parent()` has no effect on it. Confirms the
        // existing squash-merge shape still syncs unchanged.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
        let f2 = add_commit(
            &source_repo,
            "feature",
            &[("feature.txt", "line1\nline2\n")],
        );

        // A single-parent squash commit on main, carrying feature's combined
        // final tree with no second parent recorded.
        let f2_commit = source_repo.find_commit(f2).unwrap();
        let graft_commit = source_repo.find_commit(graft).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Squash merge branch 'feature'",
                &f2_commit.tree().unwrap(),
                &[&graft_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("a squash-merged commit should sync");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let feature_blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(feature_blob.content(), b"line1\nline2\n");
    }

    #[test]
    fn run_applies_a_rebased_linear_source_history_commit_by_commit() {
        // Regression case, not a fix case: a rebase-and-fast-forward
        // produces purely linear, single-parent history — decisions/0035's
        // `simplify_first_parent()` has no effect on it. Confirms ordinary
        // linear PR completion still syncs unchanged.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("a.txt", "a\n")]);
        add_commit(&source_repo, "main", &[("b.txt", "b\n")]);
        add_commit(&source_repo, "main", &[("c.txt", "c\n")]);

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("linear history should sync");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert!(tree.get_name(name).is_some(), "{name}");
        }
        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push(dest_tip_commit.id()).unwrap();
        assert_eq!(
            revwalk.count(),
            4,
            "initial commit plus one dest commit per linear source commit"
        );
    }

    #[test]
    fn run_carries_a_real_two_parent_merge_on_dest_into_source_as_one_net_change() {
        // decisions/0035: pending_commits is shared by both directions —
        // dest's own two-parent merges must collapse to their first
        // parent's diff on source too, not replay the merged-in branch's
        // own commit before the merge restores it. No equivalent coverage
        // existed for dest→source before this decision.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let source_remote = bare_source_remote_seeded_at(
            &source_repo,
            "main",
            source_repo
                .find_branch("main", git2::BranchType::Local)
                .unwrap()
                .get()
                .peel_to_commit()
                .unwrap()
                .id(),
        );

        let a1 = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("main.txt", "m1\n"),
            "an independent main-side change",
        );
        let f1 = add_independent_dest_commit_on(
            &dest_repo,
            "feature",
            dest_tip,
            ("feature.txt", "f1\n"),
            "an independent feature-side change",
        );

        // A real two-parent merge on dest: main stays first parent,
        // feature's own commit is second — its own tree carries shared.txt,
        // main.txt, and feature.txt.
        let a1_commit = dest_repo.find_commit(a1).unwrap();
        let f1_commit = dest_repo.find_commit(f1).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&a1_commit.tree().unwrap()))
            .unwrap();
        let feature_entry = f1_commit
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .unwrap()
            .id();
        builder
            .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
            .unwrap();
        let merge_tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature' into 'main'",
                &merge_tree,
                &[&a1_commit, &f1_commit],
            )
            .unwrap();

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("dest's real merge should reach source");

        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let new_source_tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_source_tip.tree().unwrap();
        assert!(tree.get_name("main.txt").is_some());
        assert!(
            tree.get_name("feature.txt").is_some(),
            "the merge's own content must reach source even though feature's own commit is \
             only reachable via the merge's second parent"
        );

        let mut revwalk = source_remote_repo.revwalk().unwrap();
        revwalk.push(new_source_tip.id()).unwrap();
        revwalk.hide(dest_tip).unwrap();
        assert_eq!(
            revwalk.count(),
            3,
            "the graft commit, one commit for a1, and one for the merge — feature's own dest \
             commit must never be applied to source on its own"
        );
    }

    #[test]
    fn run_correctly_merges_a_new_source_merge_onto_a_dest_tip_shaped_by_the_old_full_dag_walk() {
        // decisions/0035's migration case: a dest history already containing
        // a prior merge's side-branch commit individually — exactly what
        // the old, full-DAG `pending_commits` would have produced — must
        // still merge correctly once a later merge is processed under
        // first-parent semantics.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // The old merge, already fully processed by a hypothetical old-code
        // sync: R1 on main, F1 on a feature branch, OM merging them with its
        // own extra content so OM's own diff isn't a no-op.
        let r1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);
        let r1_commit = source_repo.find_commit(r1).unwrap();
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let mut builder = source_repo
            .treebuilder(Some(&r1_commit.tree().unwrap()))
            .unwrap();
        let feature_entry = f1_commit
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .unwrap()
            .id();
        builder
            .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
            .unwrap();
        let om_extra_blob = source_repo.blob(b"om-extra\n").unwrap();
        builder
            .insert("om-extra.txt", om_extra_blob, git2::FileMode::Blob.into())
            .unwrap();
        let om_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let om = source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &om_tree,
                &[&r1_commit, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        // Hand-build dest's history to match what the old, full-DAG
        // `pending_commits` would actually have produced: R1's own commit,
        // then F1's own commit (the side branch, replayed individually),
        // then OM's own commit (non-empty because of om-extra.txt) — three
        // dest commits, not the one net change the new walk would build.
        let d1 = add_source_marker_commit_on_dest(
            &dest_repo,
            "main",
            dest_tip,
            ("main.txt", "m1\n"),
            r1,
        );
        let d2 =
            add_source_marker_commit_on_dest(&dest_repo, "main", d1, ("feature.txt", "f1\n"), f1);
        add_source_marker_commit_on_dest(
            &dest_repo,
            "main",
            d2,
            ("om-extra.txt", "om-extra\n"),
            om,
        );

        // A brand-new merge, to be processed under the new first-parent walk
        // against this migrated dest state.
        let r3 = add_commit(&source_repo, "main", &[("main.txt", "m2\n")]);
        source_repo
            .branch("feature2", &source_repo.find_commit(om).unwrap(), false)
            .unwrap();
        let f2 = add_commit(&source_repo, "feature2", &[("feature2.txt", "f2\n")]);
        let r3_commit = source_repo.find_commit(r3).unwrap();
        let f2_commit = source_repo.find_commit(f2).unwrap();
        let mut builder2 = source_repo
            .treebuilder(Some(&r3_commit.tree().unwrap()))
            .unwrap();
        let feature2_entry = f2_commit
            .tree()
            .unwrap()
            .get_name("feature2.txt")
            .unwrap()
            .id();
        builder2
            .insert("feature2.txt", feature2_entry, git2::FileMode::Blob.into())
            .unwrap();
        let m2_tree = source_repo.find_tree(builder2.write().unwrap()).unwrap();
        let signature2 = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature2,
                &signature2,
                "Merge branch 'feature2'",
                &m2_tree,
                &[&r3_commit, &f2_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path())
            .expect("a new merge must apply correctly against a dest tip shaped by the old walk");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        for (name, expected) in [
            ("shared.txt", "v1\n"),
            ("main.txt", "m2\n"),
            ("feature.txt", "f1\n"),
            ("om-extra.txt", "om-extra\n"),
            ("feature2.txt", "f2\n"),
        ] {
            let blob = dest_repo
                .find_blob(tree.get_name(name).unwrap().id())
                .unwrap();
            assert_eq!(blob.content(), expected.as_bytes(), "{name}");
        }
    }

    #[test]
    fn run_does_not_push_dest_originated_content_back_when_a_later_source_commit_follows_it() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let notes_commit = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", notes_commit);

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        let tip_after_first = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // An independent change lands directly on dest.
        add_independent_dest_commit(
            &dest_repo,
            tip_after_first,
            ("dest-only.txt", "x\n"),
            "dest: a merged PR",
        );

        // dest→source cherry-picks it onto source; nothing goes to dest from
        // this run (source has nothing new pending).
        run(source_dir.path(), config.path())
            .expect("second sync (dest->source cherry-pick) should succeed");

        // A later, genuinely new source commit follows the loop-prevented
        // marker commit dest→source just wrote onto source. The cursor must
        // have advanced across that marker commit — otherwise this commit's
        // diff base stays behind it and re-includes dest-only.txt's content,
        // which is already on dest, duplicating it (or failing to apply
        // cleanly).
        add_commit(&source_repo, "main", &[("more.txt", "m\n")]);

        run(source_dir.path(), config.path()).expect("third sync should succeed");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_tip_commit.tree().unwrap();
        let more_blob = dest_repo
            .find_blob(tree.get_name("more.txt").unwrap().id())
            .unwrap();
        assert_eq!(more_blob.content(), b"m\n");
        let dest_only_blob = dest_repo
            .find_blob(tree.get_name("dest-only.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            dest_only_blob.content(),
            b"x\n",
            "dest-originated content must not be duplicated back onto dest \
             (pre-fix risk: b\"x\\nx\\n\")"
        );
        let notes_blob = dest_repo
            .find_blob(tree.get_name("notes.txt").unwrap().id())
            .unwrap();
        assert_eq!(notes_blob.content(), b"line1\n");

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            4,
            "dest history must be exactly: initial, notes.txt, the independent dest-only.txt \
             commit, more.txt"
        );
    }

    #[test]
    fn dest_resume_point_resumes_from_the_newest_gitprism_commit_in_dests_history() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let x1 = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);

        // Stands in for gitprism's own source→dest push having landed on
        // dest.
        let gitprism_push = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("notes.txt", "line1\n"),
            &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {x1}\n"),
        );

        // A second, genuinely independent dest commit landing after it.
        let d = add_independent_dest_commit(
            &dest_repo,
            gitprism_push,
            ("dest-only.txt", "from a merged PR\n"),
            "an independent, unrelated dest-side change",
        );

        // `dest_resume_point` is always called against a freshly fetched
        // dest tip in real use (`sync_pair_to_dest` fetches right before
        // calling it) — do the same here so `d` actually exists in this
        // repo's odb.
        git::fetch(
            source_dir.path(),
            &dest_repo.path().to_string_lossy(),
            "main",
        )
        .unwrap();

        // A marker commit on source naming `d` — same tree as its parent,
        // dest→source's own commit shape (copied from
        // `sync_pair_to_dest_hard_stops_on_a_real_conflict`).
        add_dest_marker_commit(&source_repo, "main", x1, d);

        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        assert_eq!(
            dest_resume_point(&source_repo, source_tip, d).unwrap(),
            Some(x1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn mirror_only_rewrite_detected_propagates_a_real_lookup_failure_instead_of_guessing() {
        use std::os::unix::fs::PermissionsExt;

        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A real commit, present in the odb — not a missing one, decisions/0039's
        // addendum case — so `find_commit` must fail for an unrelated reason:
        // its own loose object file is made unreadable below.
        let tree = source_repo.find_commit(source_tip).unwrap().tree().unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let boundary = source_repo
            .commit(None, &signature, &signature, "boundary", &tree, &[])
            .unwrap();
        drop(tree);

        let hex = boundary.to_string();
        let object_path = source_dir
            .path()
            .join(".git/objects")
            .join(&hex[..2])
            .join(&hex[2..]);
        assert!(
            object_path.exists(),
            "the boundary commit must be a real loose object"
        );
        fs::set_permissions(&object_path, fs::Permissions::from_mode(0o000)).unwrap();

        let dest_marker_tip = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("feature.txt", "v1\n"),
            &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {boundary}\n"),
        );
        git::fetch(
            source_dir.path(),
            &dest_repo.path().to_string_lossy(),
            "main",
        )
        .unwrap();
        let fetched_dest_tip = source_repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(fetched_dest_tip, dest_marker_tip);

        let key = marker::load_key().unwrap();
        let error =
            mirror_only_rewrite_detected(&source_repo, source_tip, fetched_dest_tip, "main", &key)
                .expect_err(
                    "a real lookup failure on the boundary object must propagate as Err, \
                     not be guessed as Ok(true)/Ok(false)",
                );
        assert!(
            error.to_string().contains(&boundary.to_string()),
            "unexpected error: {error}"
        );

        fs::set_permissions(&object_path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn run_dest_to_source_is_a_no_op_on_the_second_run() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);

        add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("dest-only.txt", "from a merged PR"),
            "an independent dest-side change",
        );

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );
        run(source_dir.path(), config.path()).expect("first run should succeed");

        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let tip_after_first = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // Nothing new landed on dest and nothing new landed on source since
        // the first run — a second run must be a true no-op, not re-walk
        // back to the graft and re-cherry-pick the same content again.
        run(source_dir.path(), config.path()).expect("second, no-op run should succeed");

        let tip_after_second = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            tip_after_second, tip_after_first,
            "the no-op run must not add another commit to source"
        );
    }

    fn repository_with_commits() -> (tempfile::TempDir, Repository, Oid, Oid) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "base\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let first = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "base",
                &tree,
                &[],
            )
            .unwrap();
        drop(tree);
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        std::fs::write(dir.path().join("tracked.txt"), "target\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let second = repo
            .commit(
                Some("refs/heads/target"),
                &signature,
                &signature,
                "target",
                &tree,
                &[&first_commit],
            )
            .unwrap();
        drop(first_commit);
        drop(tree);
        (dir, repo, first, second)
    }

    fn commit_with_raw_message(repo: &Repository, parent: Oid, message: &[u8]) -> Oid {
        let parent_commit = repo.find_commit(parent).unwrap();
        let mut commit = format!(
            "tree {}\nparent {}\nauthor Author <author@example.com> 0 +0000\ncommitter Committer <committer@example.com> 0 +0000\n\n",
            parent_commit.tree_id(), parent
        )
        .into_bytes();
        commit.extend_from_slice(message);
        let mut child = std::process::Command::new("git")
            .current_dir(repo.workdir().unwrap())
            .arg("hash-object")
            .arg("-t")
            .arg("commit")
            .arg("-w")
            .arg("--stdin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&commit).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git hash-object failed: {output:?}"
        );
        Oid::from_str(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap()
    }

    #[test]
    fn mirrored_commit_rejects_non_utf8_messages_without_advancing_refs() {
        let (dir, repo, first, _) = repository_with_commits();
        let invalid = commit_with_raw_message(&repo, first, b"bad\xff message\n");
        let config = Config::load(write_config("unused", "unused", &["main"]).path()).unwrap();
        let tree = repo.find_commit(invalid).unwrap().tree_id();
        let key = marker::load_key().unwrap();

        let dest_error = build_dest_commit(
            &repo,
            &config,
            first,
            &repo.find_commit(invalid).unwrap(),
            tree,
            "main",
            &key,
        )
        .unwrap_err();
        assert!(dest_error.to_string().contains(&invalid.to_string()));

        let source_error = build_source_commit(
            &repo,
            &config,
            first,
            &repo.find_commit(invalid).unwrap(),
            tree,
            "main",
            &key,
        )
        .unwrap_err();
        assert!(source_error.to_string().contains(&invalid.to_string()));
        assert_eq!(
            repo.find_branch("main", git2::BranchType::Local)
                .unwrap()
                .get()
                .target(),
            Some(first),
            "rejecting a malformed message must not advance the branch"
        );
        drop(dir);
    }

    #[test]
    fn mirrored_commit_preserves_valid_unicode_messages() {
        let (dir, repo, first, _) = repository_with_commits();
        let unicode = commit_with_raw_message(&repo, first, "héllö 世界\n".as_bytes());
        let config = Config::load(write_config("unused", "unused", &["main"]).path()).unwrap();
        let tree = repo.find_commit(unicode).unwrap().tree_id();
        let key = marker::load_key().unwrap();
        let built = build_dest_commit(
            &repo,
            &config,
            first,
            &repo.find_commit(unicode).unwrap(),
            tree,
            "main",
            &key,
        )
        .unwrap();
        assert!(
            repo.find_commit(built)
                .unwrap()
                .message()
                .unwrap()
                .contains("héllö 世界")
        );
        drop(dir);
    }

    #[test]
    fn dirty_checked_out_branch_fails_preflight_before_push() {
        let (dir, repo, first, second) = repository_with_commits();
        repo.reference("refs/heads/main", first, true, "restore test branch")
            .unwrap();
        repo.checkout_tree(
            repo.find_commit(first).unwrap().as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "local work\n").unwrap();

        let error = preflight_local_source_branch(&repo, "main", second).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local working-tree or index changes")
        );
    }

    #[test]
    fn staged_checked_out_branch_fails_preflight_before_push() {
        let (dir, repo, first, second) = repository_with_commits();
        repo.reference("refs/heads/main", first, true, "restore test branch")
            .unwrap();
        repo.checkout_tree(
            repo.find_commit(first).unwrap().as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "staged work\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();

        let error = preflight_local_source_branch(&repo, "main", second).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local working-tree or index changes")
        );
    }

    #[test]
    fn unrelated_untracked_file_is_allowed_by_preflight() {
        let (dir, repo, first, second) = repository_with_commits();
        repo.reference("refs/heads/main", first, true, "restore test branch")
            .unwrap();
        repo.checkout_tree(
            repo.find_commit(first).unwrap().as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), "keep me\n").unwrap();

        assert_eq!(
            preflight_local_source_branch(&repo, "main", second).unwrap(),
            first
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("unrelated.txt")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn colliding_untracked_file_fails_preflight() {
        let (dir, repo, first, second) = repository_with_commits();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let second_commit = repo.find_commit(second).unwrap();
        let blob = repo.blob(b"new file\n").unwrap();
        let mut tree_builder = repo
            .treebuilder(Some(&second_commit.tree().unwrap()))
            .unwrap();
        tree_builder.insert("new.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tree_builder.write().unwrap()).unwrap();
        let third = repo
            .commit(
                Some("refs/heads/target"),
                &signature,
                &signature,
                "add new file",
                &tree,
                &[&second_commit],
            )
            .unwrap();
        drop(tree);
        drop(second_commit);
        repo.reference("refs/heads/main", first, true, "restore test branch")
            .unwrap();
        repo.checkout_tree(
            repo.find_commit(first).unwrap().as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        std::fs::write(dir.path().join("new.txt"), "untracked collision\n").unwrap();

        let error = preflight_local_source_branch(&repo, "main", third).unwrap_err();
        assert!(error.to_string().contains("untracked or ignored path"));
    }

    #[cfg(unix)]
    #[test]
    fn invalid_byte_paths_are_compared_without_utf8_conversion() {
        let target = b"bad-\xff.txt";
        assert!(git_paths_conflict(target, target));
        assert!(git_paths_conflict(target, b"bad-\xff.txt/child"));
        assert!(!git_paths_conflict(target, b"bad-.txt"));
    }

    #[test]
    fn path_collision_requires_a_component_boundary() {
        assert!(!git_paths_conflict(b"foobar", b"foo"));
        assert!(git_paths_conflict(b"foo/bar", b"foo"));
        assert!(git_paths_conflict(b"foo", b"foo/bar"));
    }

    #[test]
    fn colliding_ignored_file_fails_preflight() {
        let (dir, repo, first, second) = repository_with_commits();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let second_commit = repo.find_commit(second).unwrap();
        let blob = repo.blob(b"ignored target\n").unwrap();
        let mut tree_builder = repo
            .treebuilder(Some(&second_commit.tree().unwrap()))
            .unwrap();
        tree_builder.insert("ignored.txt", blob, 0o100644).unwrap();
        let tree = repo.find_tree(tree_builder.write().unwrap()).unwrap();
        let third = repo
            .commit(
                Some("refs/heads/target"),
                &signature,
                &signature,
                "add ignored target",
                &tree,
                &[&second_commit],
            )
            .unwrap();
        drop(tree);
        drop(second_commit);
        repo.reference("refs/heads/main", first, true, "restore test branch")
            .unwrap();
        repo.checkout_tree(
            repo.find_commit(first).unwrap().as_object(),
            Some(git2::build::CheckoutBuilder::new().force()),
        )
        .unwrap();
        std::fs::write(dir.path().join(".git/info/exclude"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "local ignored file\n").unwrap();

        let error = preflight_local_source_branch(&repo, "main", third).unwrap_err();
        assert!(error.to_string().contains("untracked or ignored path"));
    }

    #[test]
    fn compare_and_swap_does_not_overwrite_a_concurrent_ref_move() {
        let (dir, repo, first, second) = repository_with_commits();
        let signature = Signature::now("concurrent", "concurrent@example.com").unwrap();
        std::fs::write(dir.path().join("tracked.txt"), "concurrent\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let first_commit = repo.find_commit(first).unwrap();
        let third = repo
            .commit(
                Some("refs/heads/concurrent"),
                &signature,
                &signature,
                "concurrent move",
                &tree,
                &[&first_commit],
            )
            .unwrap();
        drop(tree);
        drop(first_commit);
        repo.set_head_detached(first).unwrap();
        repo.reference("refs/heads/main", third, true, "concurrent Git move")
            .unwrap();

        let error = advance_local_source_branch(&repo, "main", second, first).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            repo.find_reference("refs/heads/main")
                .unwrap()
                .target()
                .unwrap(),
            third
        );
    }

    #[test]
    fn sync_pair_from_dest_does_not_reflect_a_source_originated_commit_back_to_source() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);

        // Simulate source→dest having already pushed a commit onto dest —
        // carries Gitprism-Source-Commit, gitprism's own trailer for that
        // direction. The fixture helper turns this legacy message spelling
        // into the authenticated commit shape production code writes.
        let looped_dest_tip = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "v2"),
            &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {graft_tip}\n"),
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());
        sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("a loop-prevented sync is still a successful no-op");

        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            tip.id(),
            graft_tip,
            "a dest commit carrying Gitprism-Source-Commit must not be cherry-picked back onto source"
        );
        let _ = looped_dest_tip; // the authenticated marker, not the tip identity, is under test
    }

    #[test]
    fn sync_pair_from_dest_fetches_the_local_branch_when_absent_from_the_checkout() {
        // Reproduces a real GitLab CI shape: a pipeline triggered by a push
        // to some other branch checks out only that branch, so a
        // round-tripped `config.branches` entry (here "develop") has no
        // local `refs/heads/develop` in this checkout at all, even though it
        // exists on both dest and source's own remote.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "develop", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("develop", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "develop", dest_tip, &dest_repo);
        let graft_tip = source_repo
            .find_branch("develop", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        // Stands in for source's own hosted remote — already grafted, as if
        // `gitprism setup` had run and pushed this branch there previously.
        let source_remote = bare_source_remote_seeded_at(&source_repo, "develop", graft_tip);

        // A merged dest PR, independent of anything gitprism has reflected
        // into source yet.
        add_independent_dest_commit_on(
            &dest_repo,
            "develop",
            dest_tip,
            ("shared.txt", "merged PR content"),
            "merged PR on dest",
        );

        // Simulate this checkout not having "develop" locally: detach HEAD
        // (git2 refuses to delete the branch HEAD currently points at) and
        // delete the local branch, without touching the object database — a
        // fresh single-ref CI clone would never have created this ref in the
        // first place, but the effect on `find_branch` is the same either
        // way.
        source_repo.set_head_detached(graft_tip).unwrap();
        source_repo
            .find_branch("develop", git2::BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["develop"],
            )
            .path(),
        )
        .unwrap();
        let reporter = Reporter::new(1, std::iter::empty());
        sync_pair_from_dest(&source_repo, source_dir.path(), &config, "develop", &reporter)
            .expect("a missing local branch should be fetched from source's own remote, not treated as a hard failure");

        let local_tip = source_repo
            .find_branch("develop", git2::BranchType::Local)
            .expect("the local branch should have been created from the fetched tip")
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_ne!(
            local_tip, graft_tip,
            "dest's independent commit should have been merged onto the newly created local branch"
        );

        let upstream = Repository::open(source_remote.path()).unwrap();
        let upstream_tip = upstream
            .find_branch("develop", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            upstream_tip.id(),
            local_tip,
            "the pushed remote tip and the newly created local branch must agree"
        );
        let blob = upstream
            .find_blob(
                upstream_tip
                    .tree()
                    .unwrap()
                    .get_name("shared.txt")
                    .unwrap()
                    .id(),
            )
            .unwrap();
        assert_eq!(blob.content(), b"merged PR content");
    }

    #[test]
    fn sync_pair_from_dest_does_not_trust_a_dest_authored_mapping_trailer() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);
        add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "dest change"),
            &format!("ordinary dest commit\n\nGitprism-Source-Commit: {graft_tip}\n"),
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let reporter = Reporter::new(1, std::iter::empty());
        sync_pair_from_dest(&source_repo, source_dir.path(), &config, "main", &reporter)
            .expect("a forged trailer is ordinary user text");

        let upstream = Repository::open(source_remote.path()).unwrap();
        let tip = upstream
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let blob = upstream
            .find_blob(tip.tree().unwrap().get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"dest change");
    }

    #[test]
    fn sync_pair_from_dest_hard_stops_on_a_real_conflict() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "line one\n")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Source independently changes the same file, differently from dest
        // below — the two sides now genuinely disagree.
        add_commit(
            &source_repo,
            "main",
            &[("shared.txt", "line one changed by source\n")],
        );
        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

        let conflicting_dest_commit = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "line one changed by dest\n"),
            "an independent, conflicting dest-side change",
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());

        let err = sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect_err(
                "a real same-file conflict must hard-stop, not silently resolve either side",
            );
        let message = format!("{err:#}");
        assert!(message.contains(&conflicting_dest_commit.to_string()));
        assert!(message.contains("resolve"));
        assert!(message.contains("shared.txt"));

        // Nothing must have been pushed to source's own remote — there was
        // exactly one pending dest commit, and it conflicted.
        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            tip.id(),
            source_tip,
            "a conflicting dest commit must not be partially applied or pushed"
        );
    }

    #[test]
    fn sync_pair_to_dest_hard_stops_on_a_real_conflict() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "line one\n")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let conflicting_source_commit = add_commit(
            &source_repo,
            "main",
            &[("shared.txt", "line one changed by source\n")],
        );

        // An independent, conflicting dest-side change to the same file —
        // genuinely disagrees with what source did to the same content.
        let dest_conflict_tip = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "line one changed by dest\n"),
            "an independent, conflicting dest-side change",
        );

        // Stamp a marker commit on source claiming dest→source already
        // accounted for dest_conflict_tip, so `dest_resume_point`'s own
        // boundary-recognition (covered elsewhere) doesn't get in the way of
        // this test, which targets source→dest's *apply* conflict handling
        // specifically. Same tree as the tip above it — a pure marker, no
        // content change of its own.
        add_dest_marker_commit(
            &source_repo,
            "main",
            conflicting_source_commit,
            dest_conflict_tip,
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());

        let err = sync_pair_to_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect_err(
                "a real same-file conflict must hard-stop, not silently resolve either side",
            );
        let message = format!("{err:#}");
        assert!(message.contains(&conflicting_source_commit.to_string()));
        assert!(message.contains("resolve"));
        assert!(message.contains("shared.txt"));

        // Nothing must have been pushed to dest at all.
        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_conflict_tip,
            "a conflicting source commit must not be partially applied or pushed"
        );
    }

    #[test]
    fn sync_pair_to_dest_warns_about_a_mirror_only_branch_with_no_shared_history() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "line one\n")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        // `ai-setup`: a genuinely unrelated local branch, a root commit with
        // no parents and no shared history with `main` at all — decisions
        // /0021's own example of a stray, untouched branch never touched by
        // `gitprism setup`.
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let tree = source_repo
            .find_tree(empty_tree(&source_repo).unwrap())
            .unwrap();
        source_repo
            .commit(
                Some("refs/heads/ai-setup"),
                &signature,
                &signature,
                "a stray pre-existing branch",
                &tree,
                &[],
            )
            .unwrap();

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "ai-setup", &reporter)
            .expect("a mirror-only branch with no shared history must warn, not hard-stop the run");

        // Nothing must have been pushed to dest for this branch at all.
        assert!(
            dest_repo
                .find_branch("ai-setup", git2::BranchType::Local)
                .is_err(),
            "a branch with no shared history must never be pushed to dest"
        );
    }

    #[test]
    fn sync_pair_from_dest_still_marks_a_dest_commit_that_cherry_picks_to_a_no_op() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Source already has note.txt="unchanged" before dest ever touches it.
        add_commit(&source_repo, "main", &[("note.txt", "unchanged")]);
        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

        // Dest commit A: a real, distinct change (cherry-picks with an
        // actual diff).
        let dest_a = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "v2"),
            "a real independent change",
        );
        // Dest commit B: adds note.txt="unchanged" too — coincidentally
        // identical to what source already has, so once cherry-picked onto
        // source (which already has it), the net diff is nothing.
        let dest_b = add_independent_dest_commit(
            &dest_repo,
            dest_a,
            ("note.txt", "unchanged"),
            "a no-op once merged onto source",
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());
        sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("dest→source should succeed even when the last commit is a no-op");

        // The newest commit on source must still name dest_b exactly, even
        // though cherry-picking it changed nothing — otherwise the resume
        // boundary stays stuck on dest_a forever and source→dest can never
        // recognize dest_b's tip as accounted for.
        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let new_tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            new_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_b}")),
            "a cherry-pick that changes nothing must still get its own marker commit"
        );

        // With that marker in place, source→dest must actually recognize
        // dest_b's tip as accounted for and proceed normally, not refuse.
        let repo = Repository::open(source_dir.path()).unwrap();
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch, &reporter).expect(
            "source→dest must recognize a dest tip whose only marker is a no-op commit, not refuse it",
        );
    }

    #[test]
    fn run_succeeds_without_a_source_url_when_nothing_needs_pushing_to_source() {
        let _guard = crate::config::ENV_VAR_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("GITPRISM_SOURCE_URL");
        }

        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Something for source→dest to do, but dest never advances
        // independently — dest→source has nothing to push this run.
        add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

        // No [source] section at all, and GITPRISM_SOURCE_URL unset — valid
        // per decisions/0013 as long as nothing actually needs it.
        let mut config_file = NamedTempFile::new().unwrap();
        write!(
            config_file,
            r#"
            branches = ["main"]

            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = '{}'
            "#,
            dest_dir.path().display()
        )
        .unwrap();

        run(source_dir.path(), config_file.path())
            .expect("a source→dest-only run must not fail merely for lacking a source URL");
    }

    #[test]
    fn list_source_branches_lists_every_local_branch_sorted() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let root = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "root",
                &tree,
                &[],
            )
            .unwrap();
        let root_commit = repo.find_commit(root).unwrap();
        // Created out of alphabetical order — asserting a specific sorted
        // order below pins the sorted-by-construction guarantee explicitly
        // (an explicit `.sort()`, not an assumption about git2's own
        // iteration order, which its docs don't promise) even though this
        // machine's libgit2 happens to already iterate loose refs
        // alphabetically.
        repo.branch("zeta", &root_commit, false).unwrap();
        repo.branch("alpha", &root_commit, false).unwrap();

        let branches =
            list_source_branches(&repo).expect("listing source's local branches should succeed");

        assert_eq!(
            branches,
            vec!["alpha".to_string(), "main".to_string(), "zeta".to_string()],
            "every local branch must be listed, sorted for deterministic run order"
        );
    }

    #[test]
    fn filter_tree_drops_an_excluded_directory_wholesale() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let deep_blob = repo.blob(b"deep").unwrap();
        let mut nested_builder = repo.treebuilder(None).unwrap();
        nested_builder
            .insert("deep.txt", deep_blob, git2::FileMode::Blob.into())
            .unwrap();
        let nested_tree = nested_builder.write().unwrap();

        let inner_blob = repo.blob(b"inner").unwrap();
        let mut secrets_builder = repo.treebuilder(None).unwrap();
        secrets_builder
            .insert("inner.txt", inner_blob, git2::FileMode::Blob.into())
            .unwrap();
        secrets_builder
            .insert("nested", nested_tree, git2::FileMode::Tree.into())
            .unwrap();
        let secrets_tree = secrets_builder.write().unwrap();

        let shared_blob = repo.blob(b"shared").unwrap();
        let mut root_builder = repo.treebuilder(None).unwrap();
        root_builder
            .insert("shared.txt", shared_blob, git2::FileMode::Blob.into())
            .unwrap();
        root_builder
            .insert("secrets", secrets_tree, git2::FileMode::Tree.into())
            .unwrap();
        let root_tree = repo.find_tree(root_builder.write().unwrap()).unwrap();

        let exclude_list = ExcludeList::from_contents("secrets/\n").unwrap();
        let filtered_oid = filter_tree(&repo, &root_tree, Path::new(""), &exclude_list)
            .expect("filtering a tree with an excluded directory should succeed");
        let filtered = repo.find_tree(filtered_oid).unwrap();

        assert!(filtered.get_name("shared.txt").is_some());
        assert!(
            filtered.get_name("secrets").is_none(),
            "an excluded directory must not appear at all, not even as an empty subtree"
        );
        assert_eq!(
            filtered.iter().count(),
            1,
            "the excluded directory must not be recursed into and re-added empty"
        );
    }

    #[test]
    fn filter_tree_preserves_file_modes_and_symlinks() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let regular_blob = repo.blob(b"regular").unwrap();
        let exec_blob = repo.blob(b"#!/bin/sh\n").unwrap();
        let link_blob = repo.blob(b"target.txt").unwrap();
        let excluded_blob = repo.blob(b"secret").unwrap();

        let mut builder = repo.treebuilder(None).unwrap();
        builder
            .insert("regular.txt", regular_blob, git2::FileMode::Blob.into())
            .unwrap();
        builder
            .insert("run.sh", exec_blob, git2::FileMode::BlobExecutable.into())
            .unwrap();
        builder
            .insert("link.txt", link_blob, git2::FileMode::Link.into())
            .unwrap();
        builder
            .insert("secret.txt", excluded_blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();

        let exclude_list = ExcludeList::from_contents("secret.txt\n").unwrap();
        let filtered_oid = filter_tree(&repo, &tree, Path::new(""), &exclude_list)
            .expect("filtering a tree with mixed filemodes should succeed");
        let filtered = repo.find_tree(filtered_oid).unwrap();

        assert_eq!(
            filtered.get_name("regular.txt").unwrap().filemode(),
            i32::from(git2::FileMode::Blob)
        );
        assert_eq!(
            filtered.get_name("run.sh").unwrap().filemode(),
            i32::from(git2::FileMode::BlobExecutable)
        );
        assert_eq!(
            filtered.get_name("link.txt").unwrap().filemode(),
            i32::from(git2::FileMode::Link)
        );
        assert!(
            filtered.get_name("secret.txt").is_none(),
            "an excluded file must still be dropped alongside preserving the others' filemodes"
        );
    }

    #[test]
    fn sync_pair_to_dest_carries_a_rename_and_keeps_dests_own_edit_to_the_renamed_file() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(
            dest_dir.path(),
            "main",
            &[("old.txt", "line one\nline two\nline three\n")],
        );

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let rename_commit = add_commit_removing(
            &source_repo,
            "main",
            &["old.txt"],
            &[("new.txt", "line one\nline two\nline three\n")],
        );

        let dest_edit_tip = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("old.txt", "line one\nline two\nline three edited by dest\n"),
            "an independent dest-side edit",
        );

        // Stamp a marker commit on source naming the dest edit (same
        // technique as `sync_pair_to_dest_hard_stops_on_a_real_conflict`) so
        // the boundary logic isn't what's under test here.
        add_dest_marker_commit(&source_repo, "main", rename_commit, dest_edit_tip);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("a rename carrying dest's own edit across it must merge cleanly");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("old.txt").is_none(),
            "the renamed-away path must not survive on dest"
        );
        let new_blob = dest_repo
            .find_blob(tree.get_name("new.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(new_blob.content()),
            "line one\nline two\nline three edited by dest\n"
        );
    }

    #[test]
    fn sync_pair_from_dest_carries_dests_edit_onto_a_file_source_renamed() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(
            dest_dir.path(),
            "main",
            &[("old.txt", "line one\nline two\nline three\n")],
        );

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit_removing(
            &source_repo,
            "main",
            &["old.txt"],
            &[("new.txt", "line one\nline two\nline three\n")],
        );
        // `add_commit_removing` only moves the branch ref, the same as every
        // other low-level fixture in this module — but this test, unlike
        // most, goes on to call `sync_pair_from_dest` directly, which checks
        // the merge result out onto the working tree
        // (`advance_local_source_branch`). A real checkout compares against
        // the actual on-disk index, so it has to be brought into step with
        // the rename first, or an unrelated fixture artifact (not gitprism's
        // own merge) would surface as a checkout conflict.
        source_repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

        let dest_edit_tip = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("old.txt", "line one\nline two\nline three edited by dest\n"),
            "an independent dest-side edit",
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("dest's edit to a file source renamed must carry across cleanly");

        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let new_tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_tip.tree().unwrap();
        assert!(
            tree.get_name("old.txt").is_none(),
            "the renamed-away path must not survive on source"
        );
        let new_blob = source_remote_repo
            .find_blob(tree.get_name("new.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(new_blob.content()),
            "line one\nline two\nline three edited by dest\n"
        );
        assert!(
            new_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_edit_tip}"))
        );
    }

    #[test]
    fn both_directions_treat_the_same_independent_change_as_no_conflict() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("shared.txt", "v2\n")]);
        let source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

        let dest_independent = add_independent_dest_commit(
            &dest_repo,
            dest_tip,
            ("shared.txt", "v2\n"),
            "the same one-line fix, made independently on dest",
        );

        let config = Config::load(
            write_config(
                &source_remote.path().display().to_string(),
                &dest_dir.path().display().to_string(),
                &["main"],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("an identical independent change must merge cleanly, not conflict");

        let source_remote_repo = Repository::open(source_remote.path()).unwrap();
        let new_source_tip = source_remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            new_source_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_independent}")),
            "a merge-to-no-op must still get its own marker commit on source"
        );

        let repo = Repository::open(source_dir.path()).unwrap();
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("a content no-op merge must not be misreported as a conflict");

        let dest_tip_commit = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            dest_tip_commit.id(),
            dest_independent,
            "a content no-op must not push an empty commit to dest"
        );
        let tree = dest_tip_commit.tree().unwrap();
        let shared_blob = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared_blob.content(), b"v2\n");

        let repo = Repository::open(source_dir.path()).unwrap();
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch, &reporter)
            .expect("a repeat sync of the same no-op merge must still succeed");
        let dest_tip_after_repeat = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_tip_after_repeat, dest_independent,
            "the pair must not be bricked by a commit that merges to a no-op and so never gets a trailer"
        );
    }

    #[test]
    fn run_never_leaves_an_intermediate_dest_commit_missing_a_file_from_an_interleaved_merge() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

        // A merge commit on main with parents [a1, f1] whose tree carries
        // shared.txt, main.txt, and feature.txt.
        let a1_commit = source_repo.find_commit(a1).unwrap();
        let f1_commit = source_repo.find_commit(f1).unwrap();
        let mut builder = source_repo
            .treebuilder(Some(&a1_commit.tree().unwrap()))
            .unwrap();
        let feature_entry = f1_commit
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .unwrap()
            .id();
        builder
            .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
            .unwrap();
        let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "Merge branch 'feature'",
                &merge_tree,
                &[&a1_commit, &f1_commit],
            )
            .unwrap();
        source_repo.set_head("refs/heads/main").unwrap();
        source_repo.checkout_head(None).unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let dest_tip_id = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push(dest_tip_id).unwrap();
        revwalk
            .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .unwrap();
        let commits: Vec<Oid> = revwalk.collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            commits.len(),
            3,
            "dest history must be exactly: initial, one commit for a1, one commit for f1 \
             (the merge commit's own merge is a content no-op, skipped per requirements/0001)"
        );

        let mut seen_so_far: std::collections::HashSet<String> = std::collections::HashSet::new();
        for oid in commits {
            let commit = dest_repo.find_commit(oid).unwrap();
            let names: std::collections::HashSet<String> = commit
                .tree()
                .unwrap()
                .iter()
                .map(|entry| entry.name().unwrap().to_string())
                .collect();
            let dropped: Vec<&String> = seen_so_far.difference(&names).collect();
            assert!(
                dropped.is_empty(),
                "commit {oid} dropped path(s) an earlier commit had: {dropped:?}"
            );
            seen_so_far.extend(names);
        }
    }

    #[test]
    fn run_never_pushes_an_excluded_directory_to_dest() {
        // Guard: this passes both before and after this change — a
        // regression check that decisions/0014's pre-filtering requirement
        // (an excluded directory never reaching dest, and its own history
        // never presenting as a spurious modify/delete conflict) still holds
        // now that source→dest merges through git merge-tree instead of
        // applying its own diff.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        let secret_blob = source_repo.blob(b"shh").unwrap();
        let mut secrets_builder = source_repo.treebuilder(None).unwrap();
        secrets_builder
            .insert("inner.txt", secret_blob, git2::FileMode::Blob.into())
            .unwrap();
        let secrets_tree = secrets_builder.write().unwrap();

        let mut builder = source_repo
            .treebuilder(Some(&graft_tip.tree().unwrap()))
            .unwrap();
        builder
            .insert("secrets", secrets_tree, git2::FileMode::Tree.into())
            .unwrap();
        let shared_blob = source_repo.blob(b"v2").unwrap();
        builder
            .insert("shared.txt", shared_blob, git2::FileMode::Blob.into())
            .unwrap();
        let ignore_blob = source_repo.blob(b"secrets/\n").unwrap();
        builder
            .insert(exclude::FILENAME, ignore_blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let first_commit_oid = source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "adds an excluded directory",
                &tree,
                &[&graft_tip],
            )
            .unwrap();
        fs::write(source_dir.path().join(exclude::FILENAME), "secrets/\n").unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("first sync should succeed");

        let dest_tip_after_first = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let dest_tree = dest_tip_after_first.tree().unwrap();
        let shared_blob_on_dest = dest_repo
            .find_blob(dest_tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared_blob_on_dest.content(), b"v2");
        assert!(
            dest_tree.get_name("secrets").is_none(),
            "an excluded directory must never reach dest"
        );

        let first_commit = source_repo.find_commit(first_commit_oid).unwrap();
        let revised_secret_blob = source_repo.blob(b"shh, revised").unwrap();
        let mut revised_secrets_builder = source_repo.treebuilder(None).unwrap();
        revised_secrets_builder
            .insert(
                "inner.txt",
                revised_secret_blob,
                git2::FileMode::Blob.into(),
            )
            .unwrap();
        let revised_secrets_tree = revised_secrets_builder.write().unwrap();
        let mut second_builder = source_repo
            .treebuilder(Some(&first_commit.tree().unwrap()))
            .unwrap();
        second_builder
            .insert("secrets", revised_secrets_tree, git2::FileMode::Tree.into())
            .unwrap();
        let second_tree = source_repo
            .find_tree(second_builder.write().unwrap())
            .unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "edits only the excluded path",
                &second_tree,
                &[&first_commit],
            )
            .unwrap();

        run(source_dir.path(), config.path()).expect(
            "a second sync touching only an excluded path must succeed, not hit a spurious \
             modify/delete conflict",
        );

        let dest_tip_after_second = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_tip_after_second,
            dest_tip_after_first.id(),
            "a commit touching only an excluded path must not produce an empty commit on dest"
        );
    }

    #[test]
    fn run_mirrors_an_ad_hoc_source_branch_with_no_config_entry() {
        // decisions/0017's central promise: a branch nobody ran `gitprism
        // setup` for and that appears nowhere in `config.branches` still
        // gets discovered and mirrored to dest — filtered and merge-tree'd
        // exactly like any configured branch — simulating a developer
        // branching off source's main with no setup step of their own.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(
            &source_repo,
            "feature-x",
            &[
                ("feature.txt", "line1\n"),
                ("secret.txt", "only for source"),
                (exclude::FILENAME, "secret.txt\n"),
            ],
        );

        // "feature-x" appears nowhere here.
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let dest_feature_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must be mirrored to dest even with zero config entry for it")
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = dest_feature_tip.tree().unwrap();
        let feature_blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(feature_blob.content(), b"line1\n");
        assert!(
            tree.get_name("secret.txt").is_none(),
            "an excluded file must never reach dest, even on a discovered branch"
        );
        assert!(
            tree.get_name(exclude::FILENAME).is_none(),
            ".gitprismignore itself must never reach dest, even on a discovered branch"
        );
    }

    #[test]
    fn run_mirrors_an_ad_hoc_branch_with_no_commits_of_its_own() {
        // decisions/0017: "every branch that exists on source is mirrored to
        // a same-named branch on dest" — including one that's freshly
        // branched off an already-synced tip with no commits of its own yet.
        // `build_pending_dest_tip` finds zero pending commits for a branch
        // like this (its boundary already equals its tip), which must not be
        // mistaken for "nothing to do": the branch itself still doesn't
        // exist on dest and has to be created there.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch(
                "feature-empty",
                &source_repo.find_commit(graft).unwrap(),
                false,
            )
            .unwrap();

        // "feature-empty" appears nowhere in config, and carries no commits
        // beyond the graft it was branched from.
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        dest_repo
            .find_branch("feature-empty", git2::BranchType::Local)
            .expect("feature-empty must be mirrored to dest even with no commits of its own");
    }

    #[test]
    fn run_does_not_pull_back_independent_content_from_a_non_configured_branch() {
        // decisions/0017's deliberate asymmetry: dest→source only ever
        // reflects content back for branches named in `config.branches`.
        // Content landing directly on a discovered-but-unconfigured branch's
        // dest mirror must never be pulled back into source — feature
        // branches are transient and never round-trip.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);
        let source_feature_tip = source_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");

        let dest_feature_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        // Content landing directly on dest's mirror — e.g. someone pushing
        // straight to it — independent of anything gitprism put there.
        add_independent_dest_commit_on(
            &dest_repo,
            "feature-x",
            dest_feature_tip,
            ("dest-only.txt", "pushed straight to the mirror"),
            "an independent change on the mirrored feature branch",
        );

        // "feature-x" isn't in config.branches, so dest→source never
        // considers it at all — this run's source→dest half correctly
        // refuses to fast-forward feature-x over dest content it doesn't
        // recognize, the same safety check any configured branch gets
        // (decisions/0009) — expected to surface as an error here precisely
        // because nothing will ever bring this branch's dest content back
        // into source to make it recognized.
        let err = run(source_dir.path(), config.path()).expect_err(
            "source→dest must refuse to build over dest content it doesn't recognize, even on a discovered branch",
        );
        assert!(format!("{err:#}").contains("feature-x"));

        let source_feature_tip_after = source_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            source_feature_tip_after, source_feature_tip,
            "feature-x's independent dest content must never be pulled back into source — \
             dest→source is scoped to config.branches only"
        );
    }

    #[test]
    fn run_fails_clearly_when_a_round_tripped_branchs_dest_ref_is_deleted() {
        // decisions/0018, Case 1: a round-tripped branch (config.branches) always
        // has a dest ref — gitprism's own `setup` grafted it — so it going missing
        // is a real error, not a routine "first sync" case. Before the fix,
        // `sync_pair_from_dest` fetched unconditionally and let git's own raw
        // "couldn't find remote ref" error leak through.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

        // git2 refuses to delete a bare repo's own current HEAD branch via the
        // branch API, so move HEAD off "main" first, then delete the ref
        // directly — simulating an operator (or some other process) deleting
        // main on dest.
        dest_repo.set_head("refs/heads/unrelated-head").unwrap();
        dest_repo
            .find_reference("refs/heads/main")
            .unwrap()
            .delete()
            .unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        let err = run(source_dir.path(), config.path()).expect_err(
            "a round-tripped branch whose dest ref has vanished must fail clearly, not panic through on git's own raw fetch error",
        );

        let message = format!("{err:#}");
        assert!(
            message.contains("main") && message.contains("out of sync"),
            "the error should be gitprism's own clear, actionable message naming the \
             affected branch and explaining that source/dest are out of sync: {message}"
        );
        assert!(
            !message.contains("git fetch"),
            "the fix must check existence *before* ever attempting the fetch, so the \
             raw git-fetch failure text must never appear: {message}"
        );
    }

    #[test]
    fn run_does_not_resurrect_a_mirror_only_branch_already_merged_and_deleted_on_dest() {
        // decisions/0018, Case 2: a mirror-only branch (not in config.branches)
        // that was mirrored to dest, then merged into a round-tripped branch via
        // an ordinary PR and cleaned up there, must not be blindly recreated —
        // that would undo the cleanup every single run.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);

        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        // First sync: feature-x is mirrored to dest with no config entry.
        run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
        let dest_feature_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap();

        // Simulate a real PR: feature-x is merged into dest's main via a genuine
        // new commit made directly on dest (not gitprism's own mirrored commit,
        // which would carry a Gitprism-Source-Commit trailer and get
        // loop-prevented) — single-parent, the same shape a squash-merge
        // produces, and deliberately *not* a real two-parent git merge: this
        // test means to exercise decisions/0018's merge-status check in
        // isolation, not decisions/0019's first-parent-only marker scan (see
        // `run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge`
        // for the real-merge case, which decisions/0019 now handles
        // correctly).
        let dest_main_tip_before = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &merge_signature,
                &merge_signature,
                "Merge branch 'feature-x' into 'main'",
                &dest_feature_tip.tree().unwrap(),
                &[&dest_main_tip_before],
            )
            .unwrap();

        // Second sync: dest→source reflects that merge back into source's main.
        run(source_dir.path(), config.path())
            .expect("second sync should bring the PR merge back into source's main");
        let source_main_tip_after = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            source_main_tip_after
                .tree()
                .unwrap()
                .get_name("feature.txt")
                .is_some(),
            "source's main must now carry feature-x's content via dest→source"
        );

        // dest deletes feature-x as routine post-merge cleanup.
        dest_repo
            .find_reference("refs/heads/feature-x")
            .unwrap()
            .delete()
            .unwrap();

        // Third sync must not recreate feature-x on dest.
        run(source_dir.path(), config.path())
            .expect("third sync should succeed without recreating feature-x");
        assert!(
            dest_repo
                .find_branch("feature-x", git2::BranchType::Local)
                .is_err(),
            "a mirror-only branch already merged into a round-tripped branch, then \
             deleted on dest, must not be resurrected"
        );
    }

    #[test]
    fn run_warns_about_and_skips_a_mirror_only_branch_with_no_shared_history() {
        // decisions/0024's own Consequences: a properly round-tripped branch
        // and a genuinely unrelated mirror-only branch coexist in one run —
        // the whole run still succeeds, the round-tripped branch's own sync
        // proceeds normally, and the unrelated branch is never created on
        // dest.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        // `ai-setup`: a genuinely unrelated local branch, a root commit with
        // no shared history with `main` at all — decisions/0021's own
        // example of a stray, untouched branch never touched by `gitprism
        // setup`.
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let tree = source_repo
            .find_tree(empty_tree(&source_repo).unwrap())
            .unwrap();
        source_repo
            .commit(
                Some("refs/heads/ai-setup"),
                &signature,
                &signature,
                "a stray pre-existing branch",
                &tree,
                &[],
            )
            .unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("an unrelated mirror-only branch must be skipped, not abort the whole run");

        // `main` — the round-tripped branch — is unaffected: still at its
        // already-synced tip.
        let dest_main_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip, dest_tip,
            "main's own sync must proceed normally alongside the skipped branch"
        );

        // `ai-setup` is never created on dest.
        assert!(
            dest_repo
                .find_branch("ai-setup", git2::BranchType::Local)
                .is_err(),
            "a mirror-only branch with no shared history must never be pushed to dest"
        );
    }

    #[test]
    fn run_does_not_resurrect_a_mirror_only_branch_merged_except_for_excluded_paths() {
        // decisions/0018 addendum: `already_merged_into_a_landing_branch` must
        // compare *filtered* trees, not raw source-side ones. A mirror-only
        // branch's commit touching an excluded path (`.gitprismignore`)
        // alongside an ordinary mirrored one is completely normal in a
        // source-is-a-superset repo — dest never receives that excluded path
        // either way, so its presence on `branch` but not on the landing
        // branch must not read as "genuinely unmerged." Before the fix, the
        // raw (unfiltered) tree comparison saw `secret.txt` as content
        // `landing` never received and wrongly concluded "not merged,"
        // resurrecting feature-x on dest every single run.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // `.gitprismignore` excludes secret.txt from ever reaching dest
        // (decisions/0011) — versioned on main like any other source content.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "secret.txt\n")]);
        let main_after_ignore = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch(
                "feature-x",
                &source_repo.find_commit(main_after_ignore).unwrap(),
                false,
            )
            .unwrap();
        // feature-x's own commit touches both a mirrored path and an excluded
        // path together — ordinary in a source-is-a-superset repo, and
        // exactly the shape the pre-fix bug mishandled.
        add_commit(
            &source_repo,
            "feature-x",
            &[
                ("feature.txt", "line1\n"),
                ("secret.txt", "only for source"),
            ],
        );

        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        // First sync: feature-x is mirrored to dest with no config entry —
        // secret.txt is filtered out.
        run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
        let dest_feature_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            dest_feature_tip
                .tree()
                .unwrap()
                .get_name("secret.txt")
                .is_none(),
            "secret.txt must never reach dest"
        );
        refresh_checked_out_branch(&source_repo, "main");

        // Simulate a real PR: feature-x is merged into dest's main via a
        // genuine new commit made directly on dest — single-parent, same
        // squash-shaped stand-in the sibling fixture uses, to isolate
        // decisions/0018's merge-status check from decisions/0019's
        // first-parent-only marker scan.
        let dest_main_tip_before = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &merge_signature,
                &merge_signature,
                "Merge branch 'feature-x' into 'main'",
                &dest_feature_tip.tree().unwrap(),
                &[&dest_main_tip_before],
            )
            .unwrap();

        // Second sync: dest→source reflects that merge back into source's
        // main — main's own tree still never gains secret.txt, since dest
        // never had it to bring back.
        run(source_dir.path(), config.path())
            .expect("second sync should bring the PR merge back into source's main");

        // dest deletes feature-x as routine post-merge cleanup.
        dest_repo
            .find_reference("refs/heads/feature-x")
            .unwrap()
            .delete()
            .unwrap();

        // Third sync must not recreate feature-x on dest, even though
        // feature-x's raw source-side tree carries secret.txt — an excluded
        // path main's tree never received and never will.
        run(source_dir.path(), config.path())
            .expect("third sync should succeed without recreating feature-x");
        assert!(
            dest_repo
                .find_branch("feature-x", git2::BranchType::Local)
                .is_err(),
            "a mirror-only branch already merged into a round-tripped branch (modulo \
             excluded paths dest never receives), then deleted on dest, must not be \
             resurrected"
        );
    }

    #[test]
    fn run_still_recreates_a_mirror_only_branch_with_genuinely_unmerged_content() {
        // decisions/0018, Case 2's fall-through: a mirror-only branch whose dest
        // ref is missing but whose content is only *partially* present in a
        // landing branch (e.g. resumed work after a squash merge that only
        // captured part of it) must still be rebuilt and pushed normally, not
        // mistaken for "already merged and cleaned up."
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);
        add_commit(&source_repo, "feature-x", &[("extra.txt", "line2\n")]);
        let feature_tip = source_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        // First sync: feature-x (both commits) is mirrored to dest.
        run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
        dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync");

        // A squash merge onto dest's main that only captures feature.txt, not
        // extra.txt — e.g. the PR was merged before the branch's second commit
        // was pushed.
        let dest_main_tip_before = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&dest_main_tip_before.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(b"line1\n").unwrap();
        builder
            .insert("feature.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let squash_tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        // Single-parent, same reasoning as the sibling test above: this test
        // means to exercise decisions/0018's merge-status check (a genuinely
        // unmerged remainder must still be recreated) in isolation from
        // decisions/0019's first-parent-only marker scan, which a real
        // two-parent second parent here would also exercise.
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &merge_signature,
                &merge_signature,
                "Merge branch 'feature-x' into 'main' (squash)",
                &squash_tree,
                &[&dest_main_tip_before],
            )
            .unwrap();

        run(source_dir.path(), config.path())
            .expect("second sync should bring the squash merge back into source's main");

        // dest deletes feature-x, believing it fully merged (only part of it
        // actually was).
        dest_repo
            .find_reference("refs/heads/feature-x")
            .unwrap()
            .delete()
            .unwrap();

        // Third sync: feature-x's tip still carries extra.txt, which main does
        // not have — not a no-op merge, so feature-x must be recreated on dest.
        run(source_dir.path(), config.path())
            .expect("third sync should succeed and recreate feature-x");
        let recreated = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect(
                "feature-x must be recreated on dest: its content isn't fully merged into main yet",
            )
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = recreated.tree().unwrap();
        assert!(tree.get_name("feature.txt").is_some());
        assert!(
            tree.get_name("extra.txt").is_some(),
            "the genuinely unmerged remainder must reach dest"
        );
        assert_eq!(
            recreated.tree().unwrap().id(),
            feature_tip.tree().unwrap().id(),
            "the recreated mirror must match feature-x's own current content"
        );
    }

    #[test]
    fn run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge() {
        // decisions/0019: a real, two-parent merge of a mirror-only branch
        // into a round-tripped branch on dest must not let the round-tripped
        // branch's own resume-point scan (`newest_source_marker`) cross into
        // the merged-in branch's own `Gitprism-Source-Commit` trailer via the
        // merge's second parent. Unlike decisions/0018's own Case 2 fixture
        // (which deliberately used a single-parent, squash-shaped stand-in to
        // avoid exactly this — see its comment and design/log.md), this test
        // performs the real thing: main stays first parent, feature-x's own
        // gitprism-authored mirror commit is the second — the ordinary shape
        // GitHub's/GitLab's "merge pull request" button, or `git merge`
        // run from the checked-out target branch, both produce.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);

        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        // First sync: feature-x is mirrored to dest with no config entry —
        // its dest tip carries gitprism's own Gitprism-Source-Commit trailer.
        run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
        let dest_feature_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap();

        // A real PR merge: main stays first parent, feature-x's own gitprism
        // mirror commit (carrying its own trailer) is the second parent —
        // deliberately the shape decisions/0018's own fixtures avoided.
        let dest_main_tip_before = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        let merge_commit = dest_repo
            .commit(
                Some("refs/heads/main"),
                &merge_signature,
                &merge_signature,
                "Merge branch 'feature-x' into 'main'",
                &dest_feature_tip.tree().unwrap(),
                &[&dest_main_tip_before, &dest_feature_tip],
            )
            .unwrap();

        // Second sync: dest→source reflects the merge back into source's
        // main, then source→dest's own resume-point scan for main must not
        // be confused by feature-x's own trailer, reachable via the merge's
        // second parent. Before decisions/0019's fix, this fails with a false
        // "isn't at a point this clone can safely build on" refusal, since
        // `newest_source_marker`'s full-ancestry walk reaches feature-x's own
        // marker before main's, and main's source tip isn't a descendant of
        // that unrelated oid.
        run(source_dir.path(), config.path()).expect(
            "second sync must not mistake feature-x's own merged-in trailer for main's own resume point",
        );

        let source_main_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            source_main_tip
                .tree()
                .unwrap()
                .get_name("feature.txt")
                .is_some(),
            "source's main must carry feature-x's content via dest→source"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip_after, merge_commit,
            "dest's main already carries everything source has (via the real merge); a \
             correct resume must find nothing new to push, leaving dest's tip unmoved"
        );
    }

    #[test]
    fn sync_pair_to_dest_halts_a_branch_whose_replayed_commit_has_a_differing_gitprismignore() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        // A replayed commit whose own .gitprismignore differs from what's
        // currently pinned — main's later commit (and, since checkout keeps
        // the working tree at HEAD, the working tree gitprism actually reads
        // the policy from) sets it to something else.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let halted = sync_pair_to_dest(&repo, source_dir.path(), &config, "main", &reporter)
            .expect("a policy mismatch halts the branch, it must not error the whole call");
        assert!(
            halted,
            "a replayed commit's differing .gitprismignore must halt this branch"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip_after, dest_tip,
            "nothing must be pushed to dest for a branch that halts on a policy mismatch"
        );
    }

    #[test]
    fn sync_pair_to_dest_replays_a_commit_with_a_differing_gitprism_toml_normally() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        // Two pending commits, neither yet synced to dest, each setting a
        // different .gitprism.toml — the setup-iteration shape: an operator
        // edits the config more than once before the first successful sync.
        // Unlike .gitprismignore, .gitprism.toml is never consulted per
        // replayed commit (decisions/0026's config is loaded exactly once,
        // globally) and is self-excluded from dest, so an earlier pending
        // commit's differing bytes can't leak anything the later commit
        // didn't already govern — decisions/0037's amendment: only
        // .gitprismignore is checked per pending commit.
        add_commit_bytes(
            &source_repo,
            "main",
            &[(crate::config::FILENAME, b"old config\n")],
        );
        add_commit_bytes(
            &source_repo,
            "main",
            &[
                (crate::config::FILENAME, b"new config\n"),
                ("normal.txt", b"content\n"),
            ],
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let halted = sync_pair_to_dest(&repo, source_dir.path(), &config, "main", &reporter)
            .expect("a differing .gitprism.toml must not error");
        assert!(
            !halted,
            "a replayed commit's differing .gitprism.toml must never halt the branch"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            dest_main_tip_after
                .tree()
                .unwrap()
                .get_name("normal.txt")
                .is_some(),
            "ordinary content alongside a differing .gitprism.toml must still reach dest"
        );
    }

    #[test]
    fn sync_pair_to_dest_replays_a_commit_with_no_control_file_at_all_normally() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("normal.txt", "content\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let halted = sync_pair_to_dest(&repo, source_dir.path(), &config, "main", &reporter)
            .expect("a commit with no control file at all must not error");
        assert!(
            !halted,
            "a replayed commit carrying no control file at all must never halt the branch"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            dest_main_tip_after
                .tree()
                .unwrap()
                .get_name("normal.txt")
                .is_some(),
            "a commit carrying no control file must still be pushed to dest"
        );
    }

    #[test]
    fn sync_pair_to_dest_syncs_normally_when_a_replayed_commits_control_file_matches_the_pin() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        // Two pending commits, each explicitly setting the same
        // .gitprismignore content — present, and byte-for-byte identical to
        // what ends up pinned (main's own tip, mirrored to the working
        // tree), at both of them.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
        add_commit(
            &source_repo,
            "main",
            &[("other.txt", "content\n"), (exclude::FILENAME, "*.log\n")],
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let halted = sync_pair_to_dest(&repo, source_dir.path(), &config, "main", &reporter)
            .expect("a control file matching the pin must not error");
        assert!(
            !halted,
            "a replayed commit whose control file matches the pinned policy byte-for-byte \
             must not halt the branch"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            dest_main_tip_after
                .tree()
                .unwrap()
                .get_name("other.txt")
                .is_some(),
            "a branch whose control file matches the pin must still sync its ordinary content"
        );
    }

    #[test]
    fn run_continues_other_branches_and_fails_overall_when_one_branch_halts_for_a_policy_mismatch()
    {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A healthy mirror-only branch with no control files at all — must
        // still sync even though "main" (sorted before it) is about to
        // halt.
        source_repo
            .branch("other", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

        // "main" gets a replayed commit whose .gitprismignore disagrees with
        // what main's own tip (and therefore the checked-out working tree)
        // ends up pinning.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        let err = run(source_dir.path(), config.path())
            .expect_err("a policy mismatch on one branch must fail the overall run");
        assert!(
            format!("{err:#}").to_lowercase().contains("polic"),
            "the run's own error should mention the policy mismatch: {err:#}"
        );

        assert!(
            dest_repo
                .find_branch("other", git2::BranchType::Local)
                .is_ok(),
            "other branches must still sync when one branch halts for a policy mismatch"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip_after, dest_tip,
            "nothing must be pushed to dest for the halted branch"
        );
    }

    #[test]
    fn sync_pair_to_dest_halts_a_control_file_only_branch_instead_of_classifying_it_already_merged()
    {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // "main" pins the approved policy via its own tip content, mirrored
        // to the checked-out working tree.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

        // "policy-update": the sanctioned GITPRISM_POLICY_SHA256-change
        // workflow (decisions/0037's own Context) — a branch whose sole diff
        // from "main" is a *different*, not-yet-approved .gitprismignore.
        // `add_commit_bytes` (unlike `add_commit`) never writes straight to
        // the working tree, so this doesn't clobber the pin main just set.
        source_repo
            .branch(
                "policy-update",
                &source_repo.find_commit(graft).unwrap(),
                false,
            )
            .unwrap();
        add_commit_bytes(
            &source_repo,
            "policy-update",
            &[(exclude::FILENAME, b"*.secret\n*.log\n")],
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let halted = sync_pair_to_dest(
            &repo,
            source_dir.path(),
            &config,
            "policy-update",
            &reporter,
        )
        .expect("a policy mismatch halts the branch, it must not error the whole call");
        assert!(
            halted,
            "a control-file-only branch that differs from the pinned policy must halt, not be \
             classified already-merged-and-cleaned-up"
        );

        assert!(
            dest_repo
                .find_branch("policy-update", git2::BranchType::Local)
                .is_err(),
            "a halted branch must never be pushed to dest"
        );
    }

    /// Builds a pending commit on `branch` whose `.gitprismignore` entry is a
    /// directory rather than a blob — deliberately not run through
    /// `refresh_checked_out_branch`, so the working tree keeps reflecting
    /// whichever control file was pinned by the last real commit. Only the
    /// git history, not the checkout, needs to carry the malformed tree for
    /// the pending-commit scan to find it; checking it out would also break
    /// `policy::load`'s own read of the pin before any branch is ever
    /// replayed.
    fn add_commit_with_gitprismignore_as_a_directory(repo: &Repository, branch: &str) -> Oid {
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut subtree_builder = repo.treebuilder(None).unwrap();
        let blob = repo.blob(b"unexpected\n").unwrap();
        subtree_builder
            .insert("unexpected.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let subtree = subtree_builder.write().unwrap();

        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        builder
            .insert(exclude::FILENAME, subtree, git2::FileMode::Tree.into())
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "replace the control file with a directory",
            &tree,
            &[&tip],
        )
        .unwrap()
    }

    /// Same shape as [`add_commit_with_gitprismignore_as_a_directory`], but
    /// the entry stays a blob and instead grows past
    /// `limits::MAX_CONTROL_FILE_BYTES`.
    fn add_commit_with_oversized_gitprismignore(repo: &Repository, branch: &str) -> Oid {
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let oversized = vec![b'a'; limits::MAX_CONTROL_FILE_BYTES + 1];
        let blob = repo.blob(&oversized).unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        builder
            .insert(exclude::FILENAME, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "grow the control file past the size limit",
            &tree,
            &[&tip],
        )
        .unwrap()
    }

    #[test]
    fn run_continues_other_branches_and_fails_overall_when_one_branchs_control_file_is_a_directory()
    {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A healthy mirror-only branch with no control files at all — must
        // still sync even though "main" (sorted before it) is about to halt.
        source_repo
            .branch("other", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

        // "main" pins a normal policy via its own tip content, then a pending
        // commit replaces the control file with a directory — unreadable as a
        // blob at all, let alone comparable to the pin.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
        add_commit_with_gitprismignore_as_a_directory(&source_repo, "main");

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        let err = run(source_dir.path(), config.path()).expect_err(
            "a branch whose pending commit turns the control file into a directory must fail the overall run",
        );
        assert!(
            format!("{err:#}").to_lowercase().contains("polic"),
            "the run's own error should mention the policy mismatch: {err:#}"
        );

        assert!(
            dest_repo
                .find_branch("other", git2::BranchType::Local)
                .is_ok(),
            "other branches must still sync when one branch halts because its control file isn't a regular file"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip_after, dest_tip,
            "nothing must be pushed to dest for the halted branch"
        );
    }

    #[test]
    fn run_continues_other_branches_and_fails_overall_when_one_branchs_control_file_exceeds_the_size_limit()
     {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A healthy mirror-only branch with no control files at all — must
        // still sync even though "main" (sorted before it) is about to halt.
        source_repo
            .branch("other", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

        // "main" pins a normal policy via its own tip content, then a pending
        // commit grows the control file past the byte limit.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
        add_commit_with_oversized_gitprismignore(&source_repo, "main");

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        let err = run(source_dir.path(), config.path()).expect_err(
            "a branch whose pending commit's control file exceeds the size limit must fail the overall run",
        );
        assert!(
            format!("{err:#}").to_lowercase().contains("polic"),
            "the run's own error should mention the policy mismatch: {err:#}"
        );

        assert!(
            dest_repo
                .find_branch("other", git2::BranchType::Local)
                .is_ok(),
            "other branches must still sync when one branch halts because its control file exceeds the size limit"
        );

        let dest_main_tip_after = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip_after, dest_tip,
            "nothing must be pushed to dest for the halted branch"
        );
    }

    // decisions/0039: a rewritten mirror-only source branch rebuilds its dest
    // projection instead of being refused. Three distinct fixtures below —
    // rebase, amend, hard reset — all reduce to the same underlying shape
    // (source's branch tip is a new commit that source's own history no
    // longer records as a descendant of what dest was last synced from), and
    // decisions/0039 is explicit that all three must produce the same
    // outcome: dest's old mirror history replaced wholesale.

    #[test]
    fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_a_rebase() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "original\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror feature-x to dest");
        let original_mirror_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A rebase-shaped rewrite: feature-x is reset back to the graft and
        // given a brand-new commit off it — the same parent the original
        // commit had, but not a descendant of it.
        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "rebased\n")]);

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_ne!(
            rebuilt.id(),
            original_mirror_tip,
            "the pre-rewrite mirror history must be replaced, not built upon"
        );
        let tree = rebuilt.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"rebased\n");
    }

    /// Shared setup for the amend/fresh-clone rewrite tests below: a dest
    /// with one commit, a source grafted onto it, a mirror-only `feature-x`
    /// branch carrying one commit ("original"), synced to dest once. Returns
    /// everything a rewrite variant needs to keep going from: dest's own
    /// dir/repo, source's own dir/repo, the graft point, and the loaded
    /// config — the same precedent as `repository_with_commits` above.
    fn mirror_only_feature_branch_synced_once() -> (
        tempfile::TempDir,
        Repository,
        tempfile::TempDir,
        Repository,
        Oid,
        Config,
    ) {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit_with_message(
            &source_repo,
            "feature-x",
            &[("feature.txt", "original\n")],
            "add feature",
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror feature-x to dest");

        (dest_dir, dest_repo, source_dir, repo, graft, config)
    }

    #[test]
    fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_an_amend() {
        let (_dest_dir, dest_repo, source_dir, repo, graft, config) =
            mirror_only_feature_branch_synced_once();
        let reporter = Reporter::new(1, std::iter::empty());
        let original_mirror_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // An amend-shaped rewrite: the branch's only commit is replaced by a
        // new commit object with the same parent — exactly what `git commit
        // --amend` produces at the plumbing level.
        repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
            .unwrap();
        add_commit_with_message(
            &repo,
            "feature-x",
            &[("feature.txt", "amended\n")],
            "add feature (amended)",
        );

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_ne!(
            rebuilt.id(),
            original_mirror_tip,
            "the pre-amend mirror history must be replaced, not built upon"
        );
        let tree = rebuilt.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"amended\n");
    }

    #[test]
    // decisions/0039's addendum "a missing boundary object is confirmed as a
    // rewrite on a non-shallow clone": the amend test above rewrites
    // `source_repo` in place, so the pre-amend commit never actually leaves
    // its object database and `find_commit(boundary)` trivially succeeds —
    // it never exercises the missing-object path a real, freshly fetched CI
    // clone hits. This test runs the sync against a genuinely separate clone
    // instead.
    fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_when_the_boundary_object_is_missing_from_a_non_shallow_clone()
     {
        let (dest_dir, dest_repo, _source_dir, repo, graft, config) =
            mirror_only_feature_branch_synced_once();
        let reporter = Reporter::new(1, std::iter::empty());

        // Amend, in place, exactly like the test above — but this time the
        // sync that follows runs against a *separate* clone that fetched
        // `feature-x` only after the amend, so it never had the pre-amend
        // tip the dest marker names.
        repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
            .unwrap();
        add_commit_with_message(
            &repo,
            "feature-x",
            &[("feature.txt", "amended\n")],
            "add feature (amended)",
        );

        let (fresh_dir, fresh_repo) = fresh_clone_of_branch(&repo, "feature-x", false);
        assert!(
            !fresh_repo.is_shallow(),
            "a plain fetch must not produce a shallow clone"
        );

        sync_pair_to_dest(
            &fresh_repo,
            fresh_dir.path(),
            &config,
            "feature-x",
            &reporter,
        )
        .expect(
            "a rewritten mirror-only branch must rebuild even when this clone \
             never had the pre-rewrite boundary commit, as long as it isn't shallow",
        );

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = rebuilt.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"amended\n");
        drop(dest_dir);
    }

    #[test]
    // The shallow counterpart of the test above: the ambiguity ("rewritten,
    // or just an incomplete clone?") is real on a shallow clone, so refusal
    // must stand — this is a regression guard, not new behavior.
    fn sync_pair_to_dest_still_refuses_a_mirror_only_branch_when_the_boundary_object_is_missing_from_a_shallow_clone()
     {
        let (dest_dir, dest_repo, _source_dir, repo, graft, config) =
            mirror_only_feature_branch_synced_once();
        let reporter = Reporter::new(1, std::iter::empty());

        repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
            .unwrap();
        add_commit_with_message(
            &repo,
            "feature-x",
            &[("feature.txt", "amended\n")],
            "add feature (amended)",
        );

        let (fresh_dir, fresh_repo) = fresh_clone_of_branch(&repo, "feature-x", true);
        assert!(
            fresh_repo.is_shallow(),
            "a --depth=1 fetch must produce a shallow clone"
        );

        let err = sync_pair_to_dest(
            &fresh_repo,
            fresh_dir.path(),
            &config,
            "feature-x",
            &reporter,
        )
        .expect_err("a shallow clone can't tell a rewrite from an incomplete fetch");
        assert!(
            err.to_string().contains("safely build on"),
            "unexpected error: {err}"
        );

        let untouched = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = untouched.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(
            blob.content(),
            b"original\n",
            "dest must be left exactly as the first sync produced it"
        );
        drop(dest_dir);
    }

    #[test]
    fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_reset_to_an_earlier_commit_plus_a_new_commit()
     {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        let earlier = add_commit(&source_repo, "feature-x", &[("feature.txt", "a\n")]);
        add_commit(
            &source_repo,
            "feature-x",
            &[("feature.txt", "a\n"), ("extra.txt", "b\n")],
        );

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror both commits to dest");
        let original_mirror_tip = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .expect("feature-x must exist on dest after the first sync")
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // A hard reset to an earlier, already-synced commit, plus a genuinely
        // new commit off it — dest was last synced through the second
        // commit, but source's tip no longer descends from that.
        source_repo
            .branch(
                "feature-x",
                &source_repo.find_commit(earlier).unwrap(),
                true,
            )
            .unwrap();
        add_commit(
            &source_repo,
            "feature-x",
            &[("feature.txt", "a\n"), ("different.txt", "c\n")],
        );

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_ne!(
            rebuilt.id(),
            original_mirror_tip,
            "the pre-reset mirror history must be replaced, not built upon"
        );
        let tree = rebuilt.tree().unwrap();
        assert!(tree.get_name("different.txt").is_some());
        assert!(
            tree.get_name("extra.txt").is_none(),
            "content only reachable through the discarded branch state must not survive"
        );
    }

    #[test]
    fn sync_pair_to_dest_rewinds_a_mirror_only_branch_reset_all_the_way_back_to_the_shared_graft() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "one\n")]);
        add_commit(&source_repo, "feature-x", &[("feature.txt", "two\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror both commits to dest");
        assert!(
            dest_repo
                .find_branch("feature-x", git2::BranchType::Local)
                .unwrap()
                .get()
                .peel_to_commit()
                .unwrap()
                .tree()
                .unwrap()
                .get_name("feature.txt")
                .is_some()
        );

        // The commonest rewrite shape: `git reset --hard` straight back to a
        // commit that already carries a Gitprism-Dest-Commit trailer (here,
        // the shared graft itself), with no new commit of its own. Source's
        // own graft-derived rebuild boundary now equals source's own tip, so
        // `pending_commits` finds nothing to build — the bug this test
        // guards against is `build_pending_dest_tip` reporting `new_tip:
        // None` and the caller then pushing nothing at all, leaving dest
        // silently holding the discarded history forever.
        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
            .unwrap();

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("a rewrite that rebuilds to the shared base must still be pushed");

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            rebuilt.id(),
            dest_tip,
            "dest must be rewound to the graft-derived rebuild base even though no commit was constructed"
        );
        assert!(
            rebuilt.tree().unwrap().get_name("feature.txt").is_none(),
            "the discarded commits' content must not survive on dest"
        );
    }

    #[test]
    fn sync_pair_to_dest_rewinds_a_mirror_only_branch_when_the_rewrite_filters_to_no_changes() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(
            dest_dir.path(),
            "main",
            &[("shared.txt", "v1"), (exclude::FILENAME, "secret.txt\n")],
        );

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "original\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror feature.txt to dest");
        assert!(
            dest_repo
                .find_branch("feature-x", git2::BranchType::Local)
                .unwrap()
                .get()
                .peel_to_commit()
                .unwrap()
                .tree()
                .unwrap()
                .get_name("feature.txt")
                .is_some()
        );

        // Reset back to the shared graft and replace the discarded commit
        // with one that only touches an already-excluded path. `pending` is
        // non-empty this time, but the one pending commit filters to no
        // change against the rebuild base (requirements/0001's "must not
        // push an empty commit"), so `build_pending_dest_tip` still
        // constructs no commit — a second, distinct way to reach `new_tip:
        // None` from the first test's.
        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("secret.txt", "ignored\n")]);

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter).expect(
            "a rewrite whose replacement commits all filter to no changes must still rewind dest",
        );

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            rebuilt.id(),
            dest_tip,
            "dest must be rewound to the graft-derived rebuild base even though no commit was constructed"
        );
        assert!(rebuilt.tree().unwrap().get_name("feature.txt").is_none());
        assert!(
            rebuilt.tree().unwrap().get_name("secret.txt").is_none(),
            "an excluded path must never reach dest"
        );
    }

    #[test]
    fn sync_pair_to_dest_pushes_nothing_for_a_round_tripped_branch_already_up_to_date() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let _source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        // A round-tripped branch, no rewrite involved at all — this is
        // `FastForwardOnly`'s own `(!dest_ref_exists).then_some(dest_tip)`
        // fallback, which must stay byte-identical: `dest_ref_exists` is
        // `true` here, so nothing pending must mean nothing pushed.
        sync_pair_to_dest(&repo, source_dir.path(), &config, "main", &reporter)
            .expect("a fresh graft with no source-side commits of its own has nothing to sync");

        let still = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still.id(),
            dest_tip,
            "a round-tripped branch with nothing pending must not have its dest ref touched"
        );
    }

    #[test]
    fn sync_pair_to_dest_incorporates_a_benign_race_on_a_mirror_only_branch_via_recompute_not_force()
     {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("feature.txt", "s1\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect("first sync should mirror feature-x to dest");
        let m1 = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let s2 = add_commit(&source_repo, "feature-x", &[("feature.txt", "s2\n")]);

        // Stands in for another clone legitimately completing this exact
        // sync step first: the SourceToDest marker it writes is gitprism's
        // own shape, naming s2 exactly, so this clone's own recompute must
        // recognize and build on it rather than treating it as unaccounted
        // for — and, crucially, without needing to detect (or fire) a
        // rewrite to do so, since source_tip still equals this exact marker.
        let key = marker::load_key().unwrap();
        let exclude_list = ExcludeList::from_contents("").unwrap();
        let s2_commit = repo.find_commit(s2).unwrap();
        let filtered_tree = filter_tree(
            &repo,
            &s2_commit.tree().unwrap(),
            Path::new(""),
            &exclude_list,
        )
        .unwrap();
        let m2 = build_dest_commit(
            &repo,
            &config,
            m1,
            &s2_commit,
            filtered_tree,
            "feature-x",
            &key,
        )
        .unwrap();
        let outcome = git::push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            m2,
            "feature-x",
            PushMode::FastForwardOnly,
        )
        .unwrap();
        assert_eq!(outcome, git::PushOutcome::Accepted);

        add_commit(&source_repo, "feature-x", &[("feature.txt", "s3\n")]);

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter).expect(
            "a benign, gitprism-shaped dest advance must be incorporated, not refused or forced over",
        );

        let m3 = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            m3.parent_id(0).unwrap(),
            m2,
            "the concurrently-added m2 must survive as m3's parent — a force rebuild would have \
             replaced it with a fresh chain off the graft instead"
        );
        let tree = m3.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("feature.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"s3\n");
    }

    /// Identical dest-side state for decisions/0039's authority invariant
    /// test: a bare dest and a grafted source sharing "main", a `feature-x`
    /// branch with one commit of this clone's own, and a dest ref for
    /// `feature-x` carrying a *valid* gitprism SourceToDest marker that
    /// names a sibling commit this clone's own `feature-x` never had —
    /// standing in for another, equally legitimate clone's own sync of a
    /// source state this clone doesn't share (never a rewrite: this clone's
    /// own source never moves). `feature_x_round_tripped` controls only
    /// whether `feature-x` appears in the returned config's
    /// `config.branches` — nothing else about the dest-side state differs,
    /// so any difference in outcome is licensed by that alone.
    fn authority_invariant_fixture(
        feature_x_round_tripped: bool,
    ) -> (
        tempfile::TempDir,
        Repository,
        tempfile::TempDir,
        Oid,
        Config,
    ) {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let original_dest_tip =
            bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo =
            source_grafted_onto(source_dir.path(), "main", original_dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        source_repo
            .branch("feature-x", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-x", &[("ours.txt", "mine\n")]);

        let signature = Signature::now("Sibling Clone", "sibling@example.com").unwrap();
        let their_tree = {
            let mut builder = source_repo
                .treebuilder(Some(
                    &source_repo.find_commit(graft).unwrap().tree().unwrap(),
                ))
                .unwrap();
            let blob = source_repo.blob(b"theirs\n").unwrap();
            builder
                .insert("theirs.txt", blob, git2::FileMode::Blob.into())
                .unwrap();
            source_repo.find_tree(builder.write().unwrap()).unwrap()
        };
        let their_tip = source_repo
            .commit(
                None,
                &signature,
                &signature,
                "a sibling clone's own commit",
                &their_tree,
                &[&source_repo.find_commit(graft).unwrap()],
            )
            .unwrap();

        let branches: &[&str] = if feature_x_round_tripped {
            &["main", "feature-x"]
        } else {
            &["main"]
        };
        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), branches).path(),
        )
        .unwrap();

        let key = marker::load_key().unwrap();
        let exclude_list = ExcludeList::from_contents("").unwrap();
        let their_commit = source_repo.find_commit(their_tip).unwrap();
        let filtered_tree = filter_tree(
            &source_repo,
            &their_commit.tree().unwrap(),
            Path::new(""),
            &exclude_list,
        )
        .unwrap();
        let their_mirror = build_dest_commit(
            &source_repo,
            &config,
            original_dest_tip,
            &their_commit,
            filtered_tree,
            "feature-x",
            &key,
        )
        .unwrap();
        let outcome = git::push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            their_mirror,
            "feature-x",
            PushMode::FastForwardOnly,
        )
        .unwrap();
        assert_eq!(outcome, git::PushOutcome::Accepted);

        (dest_dir, dest_repo, source_dir, their_mirror, config)
    }

    #[test]
    fn sync_pair_to_dest_discards_a_mirror_only_branchs_content_naming_an_unrelated_source_commit()
    {
        let (_dest_dir, dest_repo, source_dir, their_mirror, config) =
            authority_invariant_fixture(false);
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter).expect(
            "a mirror-only branch's unrecognized dest content must be discarded and rebuilt \
             — decisions/0039's authority invariant, licensed by config.branches absence alone",
        );

        let rebuilt = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_ne!(
            rebuilt.id(),
            their_mirror,
            "the sibling clone's unrecognized mirror must be replaced"
        );
        let tree = rebuilt.tree().unwrap();
        assert!(
            tree.get_name("ours.txt").is_some(),
            "the rebuild must reflect this clone's own source content"
        );
        assert!(
            tree.get_name("theirs.txt").is_none(),
            "the sibling clone's discarded content must not survive the rebuild"
        );
    }

    #[test]
    fn sync_pair_to_dest_stops_a_round_tripped_branchs_content_naming_an_unrelated_source_commit_instead_of_discarding_it()
     {
        // Identical dest-side state to the mirror-only test above — only
        // `feature-x`'s presence in `config.branches` differs — proving the
        // authority invariant's discard is licensed by that membership
        // alone, not by anything else about the dest-side content.
        let (_dest_dir, dest_repo, source_dir, their_mirror, config) =
            authority_invariant_fixture(true);
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        let err = sync_pair_to_dest(&repo, source_dir.path(), &config, "feature-x", &reporter)
            .expect_err(
                "a round-tripped branch must stop instead of discarding dest's unrecognized content",
            );
        let message = format!("{err:#}");
        assert!(
            !message.to_lowercase().contains("force"),
            "a round-tripped branch's refusal must never mention forcing"
        );

        let still = dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still.id(),
            their_mirror,
            "a stopped sync must not touch dest's branch at all"
        );
    }

    #[test]
    fn divergence_after_exhausted_retries_message_names_the_branch_says_diverged_and_never_prescribes_a_reconciliation_method()
     {
        for ff_target in ["dest", "source"] {
            let message = divergence_after_exhausted_retries_message("main", ff_target);
            assert!(
                message.contains("\"main\""),
                "must name the branch: {message}"
            );
            assert!(
                message.contains("diverged"),
                "must say the histories diverged: {message}"
            );
            assert!(
                message.contains("ordinary git"),
                "must hand reconciliation to the operator: {message}"
            );
            let lower = message.to_lowercase();
            for method in ["merge", "rebase", "cherry-pick", "cherry pick"] {
                assert!(
                    !lower.contains(method),
                    "must not prescribe a specific reconciliation method ({method}): {message}"
                );
            }
        }
    }

    #[test]
    fn mirror_only_skip_note_names_the_landing_branch_and_asserts_no_deletion_or_prior_existence() {
        let note = mirror_only_skip_note("main");
        assert!(
            note.contains("\"main\""),
            "must name the landing branch: {note}"
        );
        let lower = note.to_lowercase();
        for claim in ["delet", "clean", "existed", "existing", "recreat"] {
            assert!(
                !lower.contains(claim),
                "must not claim deletion, cleanup, or prior existence on dest ({claim}): {note}"
            );
        }
    }

    #[test]
    fn policy_mismatch_message_states_the_actual_reason_for_each_variant() {
        let commit = Oid::from_str("0000000000000000000000000000000000000abc").unwrap();

        let differs = policy_mismatch_message(
            "main",
            &PolicyMismatch {
                commit,
                filename: exclude::FILENAME,
                reason: PolicyMismatchReason::DiffersFromPinnedPolicy,
            },
        );
        assert!(
            differs.contains("\"main\""),
            "must name the branch: {differs}"
        );
        assert!(
            differs.contains(&format!("{commit}")),
            "must name the commit: {differs}"
        );
        assert!(
            differs.contains("that differs from the pinned policy"),
            "existing differing-bytes wording must survive unchanged: {differs}"
        );

        let not_regular = policy_mismatch_message(
            "main",
            &PolicyMismatch {
                commit,
                filename: exclude::FILENAME,
                reason: PolicyMismatchReason::NotARegularFile,
            },
        );
        assert!(
            not_regular.contains("is not a regular file"),
            "must state the entry could not be read as a control file: {not_regular}"
        );
        assert!(
            !not_regular.contains("differs from the pinned policy"),
            "must not claim a byte comparison that never happened: {not_regular}"
        );

        let too_large = policy_mismatch_message(
            "main",
            &PolicyMismatch {
                commit,
                filename: exclude::FILENAME,
                reason: PolicyMismatchReason::ExceedsSizeLimit,
            },
        );
        assert!(
            too_large.contains("exceeds"),
            "must state the size limit was exceeded: {too_large}"
        );
        assert!(
            !too_large.contains("differs from the pinned policy"),
            "must not claim a byte comparison that never happened: {too_large}"
        );

        for message in [&differs, &not_regular, &too_large] {
            assert!(
                message.contains("GITPRISM_POLICY_SHA256"),
                "every variant must still point to the remedy: {message}"
            );
        }
    }

    // decisions/0043: mirror-only branches graft onto their nearest
    // mirrored ancestor.

    #[test]
    fn ambiguous_anchor_message_names_every_candidate_and_its_merge_base() {
        let oid_a = Oid::from_str("4eb55376359199a3a77cd9f2e3aad225b78ea671").unwrap();
        let oid_b = Oid::from_str("7beb3580803dc21f883872ca2d9e010ff0078638").unwrap();
        let message = ambiguous_anchor_message(
            "task",
            &[
                ("feature-a".to_string(), oid_a),
                ("feature-b".to_string(), oid_b),
            ],
        );
        assert!(
            message.contains("\"task\""),
            "must name the branch: {message}"
        );
        assert!(
            message.contains("\"feature-a\"") && message.contains(&oid_a.to_string()),
            "must name feature-a and its merge-base: {message}"
        );
        assert!(
            message.contains("\"feature-b\"") && message.contains(&oid_b.to_string()),
            "must name feature-b and its merge-base: {message}"
        );
    }

    #[test]
    fn sync_pair_to_dest_anchors_a_task_branch_on_its_mirror_only_parent_feature_branch() {
        // task branched from mirror-only feature branched from round-tripped
        // main: task's dest chain must anchor on feature's own dest tip, not
        // on main's original graft — decisions/0043's central case.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
        let feature_tip = source_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch(
                "task",
                &source_repo.find_commit(feature_tip).unwrap(),
                false,
            )
            .unwrap();
        add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature", &reporter)
            .expect("feature must mirror to dest first");
        let dest_feature_tip = dest_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        sync_pair_to_dest(&repo, source_dir.path(), &config, "task", &reporter)
            .expect("task must mirror to dest, anchored on feature's own dest tip");
        let dest_task_tip = dest_repo
            .find_branch("task", git2::BranchType::Local)
            .expect("task must be mirrored to dest")
            .get()
            .peel_to_commit()
            .unwrap();

        // Real shared ancestry: task's dest commit is built directly onto
        // feature's own dest tip, not re-flattened onto main's graft.
        assert_eq!(
            dest_task_tip.parent_id(0).unwrap(),
            dest_feature_tip.id(),
            "task's dest commit must be built directly onto feature's own dest tip, \
             not re-mirrored from main's graft"
        );

        // A PR-shaped diff: exactly one new commit beyond feature's own
        // tip — task's own task.txt commit, not a second copy of feature's
        // history.
        let mut walk = dest_repo.revwalk().unwrap();
        walk.push(dest_task_tip.id()).unwrap();
        walk.hide(dest_feature_tip.id()).unwrap();
        let commits_since_feature: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(
            commits_since_feature.len(),
            1,
            "task's dest chain must add exactly one commit onto feature's own dest tip"
        );

        let task_tree = dest_task_tip.tree().unwrap();
        assert!(
            task_tree.get_name("feature.txt").is_some(),
            "task's dest tree must still carry feature.txt, inherited from feature's own tip"
        );
        assert!(task_tree.get_name("task.txt").is_some());
    }

    #[test]
    fn dest_anchor_for_branch_falls_back_to_baseline_then_finds_a_freshly_mirrored_sibling() {
        // The same task/feature/main topology, processed in the "wrong"
        // order: task's own anchor search runs before feature has a dest
        // ref, and must fall back to the coarser baseline (main's graft).
        // The identical search, run again once feature has since been
        // mirrored, self-corrects onto feature's own dest tip — decisions/0043's
        // own "no branch-processing-order guarantee, but corrects the run
        // after the sibling gets a dest ref."
        //
        // Exercises `dest_anchor_for_branch` directly rather than through a
        // full `sync_pair_to_dest` round trip: once task itself has a dest
        // ref, only a positively detected rewrite (decisions/0039) ever
        // re-invokes this search for it — an ordinary no-op resync does
        // not — so a direct call is what actually demonstrates the search
        // itself self-correcting run over run, without depending on
        // whichever rewrite shape happens to trigger a rebuild.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
        let feature_tip = source_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch(
                "task",
                &source_repo.find_commit(feature_tip).unwrap(),
                false,
            )
            .unwrap();
        add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);
        let task_tip = source_repo
            .find_branch("task", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let dest_url = dest_dir.path().display().to_string();
        let key = marker::load_key().unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();

        // feature has no dest ref yet — task's search falls back to the
        // baseline (main's graft) exactly.
        let anchor =
            dest_anchor_for_branch(&repo, source_dir.path(), &dest_url, "task", task_tip, &key)
                .unwrap();
        assert_eq!(
            anchor,
            DestAnchor::Resolved(graft, dest_tip),
            "with feature not yet mirrored, task's anchor must fall back to the baseline"
        );

        // feature is mirrored now — a clean, independent sync with no other
        // sibling holding a dest ref yet to interact with.
        let config = Config::load(write_config("unused", &dest_url, &["main"]).path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());
        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature", &reporter)
            .expect("feature must mirror to dest");
        let dest_feature_tip = dest_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // The identical search, run again for task, now finds feature as
        // the more specific anchor — self-correcting the earlier fallback.
        let anchor =
            dest_anchor_for_branch(&repo, source_dir.path(), &dest_url, "task", task_tip, &key)
                .unwrap();
        assert_eq!(
            anchor,
            DestAnchor::Resolved(feature_tip, dest_feature_tip),
            "once feature is mirrored, task's anchor must self-correct onto feature's own dest tip"
        );
    }

    #[test]
    fn run_hard_fails_only_the_branch_with_two_incomparable_mirrored_ancestors() {
        // Two mirror-only branches, feature-a and feature-b, both branched
        // directly from the graft and mirrored independently — neither an
        // ancestor of the other. task is a real two-parent merge of both:
        // decisions/0043's anchor search finds two equally specific,
        // incomparable candidates and must hard-fail — but only task's own
        // line, not the whole run (decisions/0024's per-branch precedent).
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature-a", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-a", &[("a.txt", "line1\n")]);
        let feature_a_tip = source_repo
            .find_branch("feature-a", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        source_repo
            .branch("feature-b", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature-b", &[("b.txt", "line1\n")]);
        let feature_b_tip = source_repo
            .find_branch("feature-b", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        // task: a real two-parent merge of feature-a and feature-b.
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();
        let merge_oid = source_repo
            .commit(
                None,
                &signature,
                &signature,
                "merge feature-a and feature-b",
                &feature_a_tip.tree().unwrap(),
                &[&feature_a_tip, &feature_b_tip],
            )
            .unwrap();
        source_repo
            .branch("task", &source_repo.find_commit(merge_oid).unwrap(), false)
            .unwrap();

        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
        let err = run(source_dir.path(), config.path())
            .expect_err("an ambiguous dest anchor must fail the overall run");
        // The run's own aggregate message stays deliberately generic
        // (decisions/0037's own precedent — the per-branch reporter line,
        // asserted via `ambiguous_anchor_message`'s own unit test below,
        // carries the specific candidate names and merge-base oids).
        assert!(
            format!("{err:#}").to_lowercase().contains("ambiguous"),
            "the run's own error should mention the ambiguous anchor: {err:#}"
        );

        // Other branches were unaffected: both siblings still mirrored.
        assert!(
            dest_repo
                .find_branch("feature-a", git2::BranchType::Local)
                .is_ok()
        );
        assert!(
            dest_repo
                .find_branch("feature-b", git2::BranchType::Local)
                .is_ok()
        );
        let dest_main_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            dest_main_tip, dest_tip,
            "main's own sync must proceed normally alongside the halted branch"
        );

        // task itself was never pushed to dest.
        assert!(
            dest_repo
                .find_branch("task", git2::BranchType::Local)
                .is_err(),
            "a branch with an ambiguous dest anchor must never be pushed to dest"
        );
    }

    #[test]
    fn sync_pair_to_dest_agrees_with_the_baseline_when_no_sibling_candidate_is_more_specific() {
        // Regression check: a mirror-only branch forked straight from
        // round-tripped main, with no other mirrored sibling more specific
        // than main's own graft — the new search must agree with the
        // baseline exactly, not produce anything different.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature", &reporter)
            .expect("feature must mirror to dest");

        let dest_feature_tip = dest_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            dest_feature_tip.parent_id(0).unwrap(),
            dest_tip,
            "with no sibling candidate more specific than main's own graft, the refined \
             search must agree with the baseline exactly"
        );
        let mut walk = dest_repo.revwalk().unwrap();
        walk.push(dest_feature_tip.id()).unwrap();
        walk.hide(dest_tip).unwrap();
        let commits_since_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(commits_since_graft.len(), 1);
    }

    #[test]
    fn sync_pair_to_dest_rewrite_rebuild_anchors_on_a_sibling_mirror_only_branch_too() {
        // decisions/0039's rewrite-rebuild arm shares dest_anchor_for_branch
        // with the brand-new-branch arm — a rewritten mirror-only branch
        // with a more specific mirrored sibling available must rebuild onto
        // that sibling, not onto main's graft, with no second
        // implementation.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let graft = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
            .unwrap();
        add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
        let feature_tip = source_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        source_repo
            .branch(
                "task",
                &source_repo.find_commit(feature_tip).unwrap(),
                false,
            )
            .unwrap();
        add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let reporter = Reporter::new(1, std::iter::empty());

        // feature mirrors first, then task correctly anchors on it.
        sync_pair_to_dest(&repo, source_dir.path(), &config, "feature", &reporter)
            .expect("feature must mirror to dest");
        sync_pair_to_dest(&repo, source_dir.path(), &config, "task", &reporter)
            .expect("task must mirror, anchored on feature's dest tip");
        let dest_task_tip_before = dest_repo
            .find_branch("task", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        // Rewrite task itself (amend-shaped): reset to feature's tip and
        // give it a brand-new commit, triggering decisions/0039's
        // rewrite-rebuild arm on the next sync.
        source_repo
            .branch("task", &source_repo.find_commit(feature_tip).unwrap(), true)
            .unwrap();
        add_commit(&source_repo, "task", &[("task.txt", "rewritten\n")]);

        sync_pair_to_dest(&repo, source_dir.path(), &config, "task", &reporter)
            .expect("a rewritten mirror-only branch must rebuild, anchored on its sibling");

        let dest_feature_tip = dest_repo
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let dest_task_tip_after = dest_repo
            .find_branch("task", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();

        assert_ne!(
            dest_task_tip_after.id(),
            dest_task_tip_before.id(),
            "the pre-rewrite mirror history must be replaced, not built upon"
        );
        assert_eq!(
            dest_task_tip_after.parent_id(0).unwrap(),
            dest_feature_tip.id(),
            "the rebuild must anchor on feature's own dest tip, not on main's graft, \
             confirming the shared call site benefits with no second implementation"
        );
        let tree = dest_task_tip_after.tree().unwrap();
        let blob = dest_repo
            .find_blob(tree.get_name("task.txt").unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), b"rewritten\n");
    }
}
