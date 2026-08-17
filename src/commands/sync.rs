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
use crate::git;

/// A lost fast-forward race (decisions/0009) is refetched and recomputed
/// from scratch this many times before sync gives up and fails loudly. Exact
/// bound is an implementation detail, not a design fork.
const MAX_RACE_RETRIES: u32 = 3;

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
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
    let config = Config::load(&config_path)?;

    // Checked once per run, not once per merge (decisions/0016) — an
    // operator on a too-old git gets one clear version message up front
    // instead of a confusing failure the first time some pending commit
    // needs merging.
    git::ensure_merge_tree_supported()?;

    // dest→source first, for every explicitly configured branch: any content
    // dest carries that gitprism didn't itself put there (e.g. a merged PR)
    // must be reflected into source before source→dest's own refusal check
    // below evaluates dest's tip — that check refuses to build on dest
    // content it doesn't recognize, and reflecting it into source is exactly
    // what makes it recognized (see `dest_resume_point`'s third case).
    // Grouping by phase rather than by branch still guarantees this ordering
    // per branch, since discovery below only runs once every dest→source call
    // has returned (decisions/0017).
    for branch in &config.branches {
        sync_pair_from_dest(&repo, &source_root, &config, branch)
            .with_context(|| format!("syncing {branch:?} dest -> source"))?;
    }

    // source→dest discovers every branch that exists on source at run time
    // (decisions/0017) rather than reading `config.branches` — a brand-new
    // branch needs no config entry to start mirroring. Sorted for
    // deterministic run order: git2's branch iteration order isn't
    // guaranteed.
    let mut source_branches: Vec<String> = repo
        .branches(Some(git2::BranchType::Local))
        .context("listing source's local branches")?
        .map(|entry| {
            let (branch, _) = entry.context("reading a local branch")?;
            let name = branch
                .name()
                .context("reading a local branch's name")?
                .context("a local branch has a non-UTF-8 name gitprism can't mirror by")?;
            Ok(name.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    source_branches.sort();

    for branch in &source_branches {
        sync_pair_to_dest(&repo, &source_root, &config, branch)
            .with_context(|| format!("syncing {branch:?} source -> dest"))?;
    }

    Ok(())
}

/// Pushes `branch`'s pending commits from source to a same-named branch on
/// dest, filtered, one branch at a time — `branch` is discovered on source at
/// run time by [`run`], not read from config (decisions/0017). Recomputes
/// from scratch (refetch, rebuild, retry) on a lost fast-forward race rather
/// than rebasing what it already built (decisions/0009).
fn sync_pair_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
) -> Result<()> {
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {branch:?}"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
        .id();

    // The exclude-list *current* as of this sync run, loaded once — not
    // reloaded per pending commit. Decisions/0004 is explicit that a change
    // to it applies to whatever's being processed right now, not a
    // historical reconstruction of what it looked like when each commit was
    // originally made.
    let exclude_list = load_current_exclude_list(repo, source_tip)?;
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
            && !config.branches.iter().any(|b| b == branch)
            && let Some(landing) =
                already_merged_into_a_landing_branch(repo, config, source_tip, source_root)?
        {
            eprintln!(
                "{branch}: not recreating on dest — already merged into {landing:?} and cleaned up there (expected for a mirror-only branch)"
            );
            return Ok(());
        }

        let (dest_tip, boundary) = if dest_ref_exists {
            if config.branches.iter().any(|b| b == branch) {
                eprintln!(
                    "{branch}: fetching dest (finding resume point before merging from source)"
                );
            } else {
                eprintln!("{branch}: fetching dest (mirror-only branch, not round-tripped)");
            }
            git::fetch(source_root, &dest_url, branch)
                .with_context(|| format!("fetching dest branch {branch:?} from {dest_url:?}"))?;
            let dest_tip = repo
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
            let boundary = dest_resume_point(repo, source_tip, dest_tip)?.with_context(|| {
                format!(
                    "gitprism sync: dest branch {branch:?} isn't at a point this clone can safely build on — either dest→source hasn't reflected its content into source yet, or this clone's {branch:?} is behind or diverged from what dest was last synced from (fetch/pull the latest source history first)"
                )
            })?;
            (dest_tip, boundary)
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
            // instead of off a dest ref that doesn't exist.
            let (boundary, dest_tip) = newest_dest_marker(repo, source_tip)?;
            (dest_tip, boundary)
        };

        let build = build_pending_dest_tip(
            repo,
            config,
            &exclude_list,
            boundary,
            dest_tip,
            source_tip,
            source_root,
        )?;

        // `build.new_tip` is `None` both when the branch has nothing new to
        // merge onto an existing dest ref (a genuine no-op) *and* when it's a
        // brand-new branch with no commits of its own beyond whatever
        // graft/marker point it shares with dest (decisions/0017: still has
        // to be created on dest). Only the latter needs `dest_tip` itself
        // pushed — it's already the right content, just missing a ref name.
        let new_dest_tip = build.new_tip.or((!dest_ref_exists).then_some(dest_tip));

        if let Some(new_dest_tip) = new_dest_tip {
            match git::push(source_root, &dest_url, new_dest_tip, branch)? {
                git::PushOutcome::Accepted => {}
                git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES => {
                    // dest's tip moved between fetch and push — refetch
                    // and recompute against its new state rather than
                    // rebasing what was already built (decisions/0009).
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                    "gitprism sync: pushing {branch:?} kept losing a fast-forward race after {} retries",
                    MAX_RACE_RETRIES
                ),
            }
        }

        if let Some(conflict) = build.conflict {
            anyhow::bail!(
                "gitprism sync: {branch:?} <- {branch:?} hit a real conflict at source commit {} in {:?} — resolve it with `gitprism resolve {branch:?}`; commits before it were still pushed to dest's {branch:?} branch",
                conflict.commit,
                conflict.paths
            );
        }

        return Ok(());
    }
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
fn build_pending_dest_tip(
    repo: &Repository,
    config: &Config,
    exclude_list: &ExcludeList,
    boundary: Oid,
    dest_tip: Oid,
    source_tip: Oid,
    source_root: &Path,
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
        if trailer_value(
            source_commit.message().unwrap_or(""),
            "Gitprism-Dest-Commit",
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

                parent = build_dest_commit(repo, config, parent, &source_commit, merged)?;
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

        let base_tree = repo
            .find_commit(merge_base)
            .context("resolving a landing branch's merge-base commit")?
            .tree_id();
        let landing_tree = repo
            .find_commit(landing_tip)
            .context("resolving a landing branch's tip commit")?
            .tree_id();
        let branch_tree = repo
            .find_commit(branch_tip)
            .context("resolving a mirror-only branch's tip commit")?
            .tree_id();

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
fn dest_tip_is_accounted_for(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<bool> {
    let dest_commit = repo
        .find_commit(dest_tip)
        .context("resolving dest's tip commit")?;

    // Case 1.
    if trailer_value(
        dest_commit.message().unwrap_or(""),
        "Gitprism-Source-Commit",
    )
    .is_some()
    {
        return Ok(true);
    }

    // Case 2.
    if graft_point(repo, source_tip, dest_tip)? == dest_tip {
        return Ok(true);
    }

    // Case 3: dest_tip has moved past the graft with nothing gitprism wrote
    // there directly (case 1 would've caught that) — only safe if
    // dest→source has already reflected dest_tip into source, i.e. source's
    // own history carries a Gitprism-Dest-Commit trailer naming it exactly
    // (this same run, since it's ordered first — see `run`'s doc comment).
    let (_, marker_names) = newest_dest_marker(repo, source_tip)?;
    Ok(marker_names == dest_tip)
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
fn dest_resume_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Option<Oid>> {
    if !dest_tip_is_accounted_for(repo, source_tip, dest_tip)? {
        return Ok(None);
    }

    let Some(boundary) = newest_source_marker(repo, dest_tip)? else {
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

/// Every commit strictly after `boundary` up to and including `tip`, oldest
/// first — the order later commits may depend on must be preserved
/// (decisions/0007's "Consequences"). Used for both directions: `tip` is
/// source's tip when walking what's pending for dest, or dest's tip when
/// walking what's pending for source — the walk itself doesn't care which
/// repo-side branch it's scoped to (decisions/0005: the ref you scan already
/// supplies that context).
fn pending_commits(repo: &Repository, boundary: Oid, tip: Oid) -> Result<Vec<Oid>> {
    let mut revwalk = repo.revwalk().context("starting a pending-commit walk")?;
    revwalk
        .push(tip)
        .context("seeding the pending-commit walk")?;
    revwalk
        .hide(boundary)
        .context("excluding already-synced history")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .context("ordering pending commits oldest-first")?;

    revwalk
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("walking pending commits")
}

/// Extracts `key`'s value from a `Key: value` line anywhere in `message` —
/// the same trailer shape `setup` already writes
/// (`Gitprism-Dest-Commit: <oid>`), read back here for
/// `Gitprism-Source-Commit` (and, for loop prevention,
/// `Gitprism-Dest-Commit`).
fn trailer_value<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    message
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::trim)
}

/// Loads the exclude-list *current* as of `source_tip` — the version this
/// whole sync run filters every pending commit with (decisions/0004: the
/// current list applies to whatever's being processed right now, not a
/// historical reconstruction of what it looked like at each commit).
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
fn filter_tree(
    repo: &Repository,
    tree: &git2::Tree,
    prefix: &Path,
    exclude_list: &ExcludeList,
) -> Result<Oid> {
    let mut builder = repo
        .treebuilder(None)
        .context("starting a filtered tree builder")?;

    for entry in tree.iter() {
        let name = entry
            .name()
            .context("a tree entry has a non-UTF-8 name gitprism can't filter by")?;
        let rel_path = prefix.join(name);
        let is_dir = entry.kind() == Some(git2::ObjectType::Tree);

        if exclude_list.is_excluded(&rel_path, is_dir) {
            continue;
        }

        if is_dir {
            let subtree = repo
                .find_tree(entry.id())
                .with_context(|| format!("reading subtree {}", rel_path.display()))?;
            let filtered_oid = filter_tree(repo, &subtree, &rel_path, exclude_list)?;
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

/// Builds one new dest-bound commit in `repo`'s object database — object
/// only, no ref update, since the chain is pushed by oid once it's complete.
/// Preserves the original author, stamps gitprism's own committer identity
/// (decisions/0010), and carries the `Gitprism-Source-Commit` trailer
/// (decisions/0003) that lets a future sync resume from here.
fn build_dest_commit(
    repo: &Repository,
    config: &Config,
    parent: Oid,
    source_commit: &git2::Commit,
    filtered_tree_oid: Oid,
) -> Result<Oid> {
    let parent_commit = repo
        .find_commit(parent)
        .context("resolving the dest chain's parent commit")?;
    let tree = repo
        .find_tree(filtered_tree_oid)
        .context("reading the filtered tree")?;
    let committer = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = format!(
        "{}\n\nGitprism-Source-Commit: {}\n",
        source_commit.message().unwrap_or("").trim_end(),
        source_commit.id()
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
fn sync_pair_from_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
) -> Result<()> {
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

        eprintln!(
            "{branch}: fetching dest (checking for independent content to reflect into source)"
        );
        git::fetch(source_root, &dest_url, branch)
            .with_context(|| format!("fetching dest branch {branch:?} from {dest_url:?}"))?;
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();

        // On the first attempt, source's own tip is just this checkout's
        // local branch — `sync` always runs inside a real checkout of source
        // (decisions/0012), so there's nothing to fetch for it normally. A
        // retry means the push below lost a fast-forward race against
        // source's *remote*, which this checkout's local branch can't see by
        // itself — so from then on, refetch source's own branch too and
        // recompute against its actual current tip, the same
        // refetch-and-recompute principle decisions/0009 already established
        // for the source→dest direction.
        let source_tip = if attempt == 0 {
            repo.find_branch(branch, git2::BranchType::Local)
                .with_context(|| format!("resolving source branch {branch:?}"))?
                .get()
                .peel_to_commit()
                .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
                .id()
        } else {
            // Only resolved once actually needed — a config that omits
            // [source].url/GITPRISM_SOURCE_URL entirely (decisions/0013) is
            // valid as long as this branch never actually needs to push
            // anything to source, e.g. a branch that never receives
            // independent dest-side commits.
            let source_url = config.source_url()?;
            eprintln!("{branch}: refetching source (lost a push race, recomputing)");
            git::fetch(source_root, &source_url, branch).with_context(|| {
                format!("fetching source branch {branch:?} from {source_url:?}")
            })?;
            repo.find_reference("FETCH_HEAD")
                .context("reading FETCH_HEAD after fetch")?
                .peel_to_commit()
                .context("resolving fetched source branch to a commit")?
                .id()
        };

        let pending = pending_dest_commits(repo, source_tip, dest_tip).with_context(|| {
            format!("has dest branch {branch:?}'s history been rewritten outside gitprism?")
        })?;
        let build = build_pending_source_tip(repo, config, pending, source_tip, source_root)?;

        if let Some(new_source_tip) = build.new_tip {
            let source_url = config.source_url()?;
            match git::push(source_root, &source_url, new_source_tip, branch)? {
                git::PushOutcome::Accepted => {
                    advance_local_source_branch(repo, branch, new_source_tip)?;
                }
                git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES => {
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                    "gitprism sync: pushing {branch:?} kept losing a fast-forward race after {} retries",
                    MAX_RACE_RETRIES
                ),
            }
        }

        if let Some(conflict) = build.conflict {
            anyhow::bail!(
                "gitprism sync: {branch:?} <- {branch:?} hit a real conflict at dest commit {} in {:?} — resolve it with `gitprism resolve {branch:?}`; commits before it were still pushed to source's {branch:?} branch",
                conflict.commit,
                conflict.paths
            );
        }

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
fn advance_local_source_branch(repo: &Repository, branch: &str, new_tip: Oid) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let previous_tip = repo
        .find_reference(&refname)
        .ok()
        .and_then(|r| r.peel_to_commit().ok())
        .map(|c| c.id());

    if let Some(previous_tip) = previous_tip
        && previous_tip != new_tip
        && !repo.graph_descendant_of(new_tip, previous_tip).with_context(|| {
            format!(
                "checking whether {new_tip} is a fast-forward of local branch {branch:?}'s current tip {previous_tip}"
            )
        })?
    {
        anyhow::bail!(
            "gitprism sync: local branch {branch:?} (currently {previous_tip}) has diverged from what dest→source just pushed to source's remote ({new_tip}) — refusing to force it forward and silently discard that local work; reconcile it manually before syncing again"
        );
    }

    let head_points_here = repo
        .head()
        .ok()
        .and_then(|head_ref| head_ref.name().ok().map(str::to_owned))
        .is_some_and(|name| name == refname);

    if head_points_here {
        let new_commit = repo
            .find_commit(new_tip)
            .context("resolving the newly pushed local commit")?;
        // Deliberately not forced (`None` options default to a safe
        // checkout, same as `setup`'s own `checkout_head(None)`) — a real
        // local modification must surface as a conflict, not be silently
        // overwritten just because dest→source advanced the branch.
        repo.checkout_tree(new_commit.as_object(), None)
            .context("checking out what dest→source just pushed into the working tree")?;
    }

    repo.reference(&refname, new_tip, true, "gitprism sync: dest -> source")
        .with_context(|| {
            format!("advancing local branch {branch:?} to match what was just pushed")
        })?;

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
/// Always finds something for a properly set-up branch: setup's own graft
/// commit (decisions/0006) carries this trailer too, and is always a
/// first-parent ancestor of every branch it grafts.
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
fn newest_dest_marker(repo: &Repository, source_tip: Oid) -> Result<(Oid, Oid)> {
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

    for oid in revwalk {
        let oid = oid.context("walking source's history for a resume point")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in source's history")?;
        if let Some(value) = trailer_value(commit.message().unwrap_or(""), "Gitprism-Dest-Commit") {
            let dest_oid = Oid::from_str(value).with_context(|| {
                format!("parsing Gitprism-Dest-Commit trailer {value:?} on source commit {oid}")
            })?;
            return Ok((oid, dest_oid));
        }
    }

    anyhow::bail!(
        "gitprism sync: no Gitprism-Dest-Commit trailer found anywhere in source's history — has `gitprism setup` been run for this pair?"
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
/// The scan is deliberately unbounded along the history it walks — it does
/// NOT `revwalk.hide` the graft point as an optimization. Hiding it would be
/// a pure optimization on the usual case, but a marker sitting *before* the
/// graft point (e.g. a source repo re-grafted onto a dest gitprism had
/// already written to) would become invisible to the scan, and the caller
/// would then silently fall back to the graft and push instead of refusing.
/// Unbounded, plus [`dest_resume_point`]'s own ancestry guard on the result,
/// fails safe instead.
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
fn newest_source_marker(repo: &Repository, dest_tip: Oid) -> Result<Option<Oid>> {
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

    for oid in revwalk {
        let oid = oid.context("walking dest's history for a resume point")?;
        let commit = repo
            .find_commit(oid)
            .context("resolving a commit in dest's history")?;
        if let Some(value) = trailer_value(commit.message().unwrap_or(""), "Gitprism-Source-Commit")
        {
            let source_oid = Oid::from_str(value).with_context(|| {
                format!("parsing Gitprism-Source-Commit trailer {value:?} on dest commit {oid}")
            })?;
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
) -> Result<Vec<Oid>> {
    let (_, boundary) = newest_dest_marker(repo, source_tip)?;
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
        if trailer_value(commit.message().unwrap_or(""), "Gitprism-Source-Commit").is_none() {
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
                parent = build_source_commit(repo, config, parent, &dest_commit, tree_oid)?;
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
) -> Result<Oid> {
    let parent_commit = repo
        .find_commit(parent)
        .context("resolving the source chain's parent commit")?;
    let tree = repo
        .find_tree(tree_oid)
        .context("reading the cherry-picked tree")?;
    let committer = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = format!(
        "{}\n\nGitprism-Dest-Commit: {}\n",
        dest_commit.message().unwrap_or("").trim_end(),
        dest_commit.id()
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

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "initial",
            &tree,
            &[],
        )
        .unwrap()
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
            url = "{source_url}"

            [dest]
            url = "{dest_url}"
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
            repo.commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &format!(
                    // `source_dir` makes this graft commit's content unique
                    // per test fixture — two independently-`init`'d clones
                    // grafted onto the same dest tip must not collapse into
                    // the same commit object just because they also share a
                    // timestamp (a real risk: same tree, parent, message,
                    // and signature otherwise).
                    "gitprism setup: graft ({})\n\nGitprism-Dest-Commit: {}\n",
                    source_dir.display(),
                    dest_tip_commit.id()
                ),
                &fetched_tip.tree().unwrap(),
                &[&fetched_tip],
            )
            .unwrap();
        }
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(None).unwrap();
        repo
    }

    fn add_commit(repo: &Repository, branch: &str, files: &[(&str, &str)]) -> Oid {
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

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a real change",
            &tree,
            &[&tip],
        )
        .unwrap()
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

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a binary change",
            &tree,
            &[&tip],
        )
        .unwrap()
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

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a rename",
            &tree,
            &[&tip],
        )
        .unwrap()
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
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                message,
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
        dest_repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                message,
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
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                // Names dest's *real* tip — a fabricated, non-existent sha
                // here would trip dest→source's own resume-scan (it treats
                // this same trailer as "dest content already reflected up to
                // here"), which isn't what this test is exercising.
                &format!("gitprism resolve\n\nGitprism-Dest-Commit: {dest_tip}\n"),
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
                trailer_value(commit.message().unwrap_or(""), "Gitprism-Source-Commit")
                    == Some(notes_commit.to_string().as_str())
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

        // Intermediate dest commits are a linearization artifact: whichever
        // branch the revwalk (TOPOLOGICAL|REVERSE) emits second yields a dest
        // commit whose diff (against the cursor, the previously examined
        // pending commit on the *other* branch) temporarily removes that
        // other branch's file — restored again by the merge commit's own
        // diff. That's an accepted, recorded design question for the project
        // owner, not something this test asserts on or tries to fix — only
        // the tip is checked here.
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
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_tip_commit = source_repo.find_commit(x1).unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!("gitprism sync: dest -> source\n\nGitprism-Dest-Commit: {d}\n"),
                &source_tip_commit.tree().unwrap(),
                &[&source_tip_commit],
            )
            .unwrap();

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
        // direction. Only the trailer's *presence* matters for loop
        // prevention (decisions/0003), not whether the named oid resolves to
        // anything real, so `graft_tip` here is just a convenient real oid.
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
        sync_pair_from_dest(&repo, source_dir.path(), &config, branch)
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
        let _ = looped_dest_tip; // only its trailer mattered, not its identity
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

        let err = sync_pair_from_dest(&repo, source_dir.path(), &config, branch).expect_err(
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
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_tip_commit = source_repo.find_commit(conflicting_source_commit).unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!(
                    "gitprism sync: dest -> source\n\nGitprism-Dest-Commit: {dest_conflict_tip}\n"
                ),
                &source_tip_commit.tree().unwrap(),
                &[&source_tip_commit],
            )
            .unwrap();

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";

        let err = sync_pair_to_dest(&repo, source_dir.path(), &config, branch).expect_err(
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
        sync_pair_from_dest(&repo, source_dir.path(), &config, branch)
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
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch).expect(
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
            url = "{}"
            "#,
            dest_dir.path().display()
        )
        .unwrap();

        run(source_dir.path(), config_file.path())
            .expect("a source→dest-only run must not fail merely for lacking a source URL");
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
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let source_tip_commit = source_repo.find_commit(rename_commit).unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &format!(
                    "gitprism sync: dest -> source\n\nGitprism-Dest-Commit: {dest_edit_tip}\n"
                ),
                &source_tip_commit.tree().unwrap(),
                &[&source_tip_commit],
            )
            .unwrap();

        let config = Config::load(
            write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let branch = "main";

        sync_pair_to_dest(&repo, source_dir.path(), &config, branch)
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

        sync_pair_from_dest(&repo, source_dir.path(), &config, branch)
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

        sync_pair_from_dest(&repo, source_dir.path(), &config, branch)
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
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch)
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
        sync_pair_to_dest(&repo, source_dir.path(), &config, branch)
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
}
