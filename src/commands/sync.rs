//! `gitprism sync` — see design/playbooks/0001-gitlab-pipeline-triggers.md,
//! design/decisions/0003-mapping-state-in-commit-trailers.md,
//! design/decisions/0006-setup-uses-real-shared-history.md,
//! design/decisions/0007-conflict-policy-hard-stop.md,
//! design/decisions/0009-push-race-refetch-and-recompute.md, and
//! design/decisions/0013-repo-urls-optional-fall-back-to-env-vars.md.
//!
//! Both directions, per configured branch pair (decisions/0005), run one
//! after the other:
//!
//! **source→dest**: fetch dest's current tip, find every source commit not
//! yet reflected there, filter each one against the *current* exclude-list
//! (decisions/0004, 0011 — not a historical reconstruction of what it looked
//! like at that commit) and re-parent it onto dest's tip, then push the
//! result — fast-forward only, never forced (requirements/0001). Refuses to
//! sync a pair at all if dest's tip carries any commit gitprism didn't put
//! there since its own last push, rather than fast-forwarding a snapshot
//! that would silently drop dest's independent content — that content is
//! exactly what the next step brings back.
//!
//! **dest→source**: find every dest commit not yet reflected into source by
//! scanning *source's* history for the most recent `Gitprism-Dest-Commit`
//! trailer (decisions/0003) — setup's own graft commit (decisions/0006)
//! always carries one, so this never needs a special-cased first run — then
//! cherry-pick each pending dest commit onto source's tip and push the
//! result to source's own remote. A real content conflict hard-stops that
//! pair (decisions/0007): whatever cherry-picked cleanly before the conflict
//! is still pushed, and the conflicting commit is left for a human to
//! resolve (decisions/0008), retried automatically on the next run once it
//! is.
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

use crate::config::{BranchPair, Config};
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

    for pair in &config.pairs {
        // dest→source first: any content dest carries that gitprism didn't
        // itself put there (e.g. a merged PR) must be reflected into source
        // before source→dest's own refusal check below evaluates dest's tip
        // — that check refuses to build on dest content it doesn't
        // recognize, and reflecting it into source is exactly what makes it
        // recognized (see `dest_resume_point`'s third case).
        sync_pair_from_dest(&repo, &source_root, &config, pair).with_context(|| {
            format!("syncing {:?} -> {:?}", pair.dest_branch, pair.source_branch)
        })?;
        sync_pair_to_dest(&repo, &source_root, &config, pair).with_context(|| {
            format!("syncing {:?} -> {:?}", pair.source_branch, pair.dest_branch)
        })?;
    }

    Ok(())
}

/// Pushes `pair.source_branch`'s pending commits to `pair.dest_branch`,
/// filtered, one branch pair at a time. Recomputes from scratch (refetch,
/// rebuild, retry) on a lost fast-forward race rather than rebasing what it
/// already built (decisions/0009).
fn sync_pair_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    pair: &BranchPair,
) -> Result<()> {
    let source_tip = repo
        .find_branch(&pair.source_branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {:?}", pair.source_branch))?
        .get()
        .peel_to_commit()
        .with_context(|| {
            format!(
                "resolving source branch {:?} to a commit",
                pair.source_branch
            )
        })?
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
        git::fetch(source_root, &dest_url, &pair.dest_branch).with_context(|| {
            format!(
                "fetching dest branch {:?} from {dest_url:?}",
                pair.dest_branch
            )
        })?;
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();

        // There is no safe way to build a new commit straight from source's
        // filtered snapshot and fast-forward dest onto it unless this
        // clone's source_tip is known to be caught up with whatever dest
        // last synced from — either because dest carries independent content
        // gitprism hasn't reflected into source yet (dest→source, run just
        // above in `run`, normally handles this before we ever get here), or
        // because this clone's own source branch is behind or diverged from
        // the source commit dest was actually last synced from (e.g. another
        // clone already pushed for this pair). Either way, proceeding could
        // silently drop content some other commit already contributed, even
        // though the ref update itself would be a legitimate fast-forward.
        let boundary = dest_resume_point(repo, source_tip, dest_tip)?.with_context(|| {
            format!(
                "gitprism sync: dest branch {:?} isn't at a point this clone can safely build on — either dest→source hasn't reflected its content into source yet, or this clone's {:?} is behind or diverged from what dest was last synced from (fetch/pull the latest source history first)",
                pair.dest_branch, pair.source_branch
            )
        })?;

        let build =
            build_pending_dest_tip(repo, config, &exclude_list, boundary, dest_tip, source_tip)?;

        if let Some(new_dest_tip) = build.new_tip {
            match git::push(source_root, &dest_url, new_dest_tip, &pair.dest_branch)? {
                git::PushOutcome::Accepted => {}
                git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES => {
                    // dest's tip moved between fetch and push — refetch
                    // and recompute against its new state rather than
                    // rebasing what was already built (decisions/0009).
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                    "gitprism sync: pushing {:?} kept losing a fast-forward race after {} retries",
                    pair.dest_branch,
                    MAX_RACE_RETRIES
                ),
            }
        }

        if let Some(conflict_oid) = build.conflict {
            anyhow::bail!(
                "gitprism sync: {:?} <- {:?} hit a real conflict at source commit {conflict_oid} — resolve it with `gitprism resolve {:?}` (decisions/0007, decisions/0008, decisions/0014); commits before it were still pushed to dest's {:?} branch",
                pair.dest_branch,
                pair.source_branch,
                pair.source_branch,
                pair.dest_branch
            );
        }

        return Ok(());
    }
}

/// The result of [`build_pending_dest_tip`]: `new_tip` is the chain's tip if
/// anything applied cleanly (`None` if nothing was pending, or every pending
/// commit was either loop-prevented or filtered to a no-op), and `conflict`
/// names the first source commit that couldn't be applied cleanly onto dest,
/// if any (decisions/0007, decisions/0014) — processing always stops there
/// (decisions/0007's "Consequences": later commits may depend on it).
struct PendingDestBuild {
    new_tip: Option<Oid>,
    conflict: Option<Oid>,
}

/// Builds, in `repo`'s object database, a chain of new commits reflecting
/// every source commit between `boundary` and `source_tip`, each applied as
/// its *own* filtered diff onto dest's current chain tip (decisions/0014) —
/// not a full-tree snapshot replace, which would silently regress any
/// independent dest content a not-yet-processed dest→source cherry-pick
/// already landed further up source's history. Stops at the first commit
/// that doesn't apply cleanly (decisions/0007). `dest_tip` seeds the chain's
/// first parent.
fn build_pending_dest_tip(
    repo: &Repository,
    config: &Config,
    exclude_list: &ExcludeList,
    boundary: Oid,
    dest_tip: Oid,
    source_tip: Oid,
) -> Result<PendingDestBuild> {
    let pending = pending_commits(repo, boundary, source_tip)?;
    let empty_tree_oid = repo
        .treebuilder(None)
        .context("starting an empty tree builder")?
        .write()
        .context("writing an empty tree")?;

    let mut parent = dest_tip;
    let mut built_any = false;
    for source_oid in pending {
        let source_commit = repo
            .find_commit(source_oid)
            .context("resolving a pending source commit")?;

        // Loop prevention (decisions/0003): a source commit that itself came
        // from dest (dest→source sync) already exists on dest — pushing it
        // back would loop.
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
        let old_tree = match source_commit.parent(0) {
            Ok(p) => p
                .tree()
                .context("reading a pending source commit's parent tree")?,
            Err(_) => repo
                .find_tree(empty_tree_oid)
                .context("reading the empty tree")?,
        };
        let new_tree = source_commit
            .tree()
            .context("reading a pending source commit's tree")?;

        match apply_filtered_diff(
            repo,
            &parent_commit.tree()?,
            &old_tree,
            &new_tree,
            exclude_list,
        )? {
            ApplyOutcome::Conflict => {
                return Ok(PendingDestBuild {
                    new_tip: built_any.then_some(parent),
                    conflict: Some(source_oid),
                });
            }
            ApplyOutcome::Clean(filtered_tree_oid) => {
                // This commit's diff, once filtered, changed nothing dest-
                // side (e.g. it only touched excluded paths) — must not push
                // an empty commit (requirements/0001).
                if filtered_tree_oid == parent_commit.tree_id() {
                    continue;
                }

                parent =
                    build_dest_commit(repo, config, parent, &source_commit, filtered_tree_oid)?;
                built_any = true;
            }
        }
    }

    Ok(PendingDestBuild {
        new_tip: built_any.then_some(parent),
        conflict: None,
    })
}

/// The result of [`apply_filtered_diff`]: either the resulting tree, or that
/// the diff didn't apply cleanly onto `onto_tree` — a genuine dest↔source
/// content conflict (decisions/0007, decisions/0014), not a bug.
enum ApplyOutcome {
    Clean(Oid),
    Conflict,
}

/// Diffs `old_tree` against `new_tree` (a pending source commit against its
/// own parent), drops any delta touching an excluded path *before*
/// application — not after, since an already-excluded path's own history
/// (e.g. `.gitprismignore` being edited repeatedly) would otherwise look like
/// a modify/delete conflict on every sync, dest never having that path to
/// merge against at all — then applies what's left onto `onto_tree` (dest's
/// current chain tip).
fn apply_filtered_diff(
    repo: &Repository,
    onto_tree: &git2::Tree,
    old_tree: &git2::Tree,
    new_tree: &git2::Tree,
    exclude_list: &ExcludeList,
) -> Result<ApplyOutcome> {
    let diff = repo
        .diff_tree_to_tree(Some(old_tree), Some(new_tree), None)
        .context("diffing a pending source commit against its parent")?;

    let mut apply_opts = git2::ApplyOptions::new();
    apply_opts.delta_callback(|delta| {
        let path = delta.and_then(|d| d.new_file().path().or_else(|| d.old_file().path()));
        match path {
            Some(path) => !exclude_list.is_excluded(path, false),
            None => true,
        }
    });

    match repo.apply_to_tree(onto_tree, &diff, Some(&mut apply_opts)) {
        Ok(mut index) => {
            let tree_oid = index
                .write_tree_to(repo)
                .context("writing the diff-applied tree")?;
            Ok(ApplyOutcome::Clean(tree_oid))
        }
        Err(err) if err.code() == git2::ErrorCode::ApplyFail => Ok(ApplyOutcome::Conflict),
        Err(err) => Err(err).context("applying a pending source commit's filtered diff onto dest"),
    }
}

/// Whether `dest_tip` is a point gitprism already accounts for. Three cases,
/// checked in order:
///
/// 1. It's the tip of gitprism's own last source→dest push (carries a
///    `Gitprism-Source-Commit` trailer directly) — boundary is that trailer's
///    own value, a real source-space ancestor.
/// 2. dest hasn't advanced at all since `setup`'s graft (decisions/0006), i.e.
///    no sync has landed yet and nothing independent has landed either —
///    boundary is the graft point itself (`merge_base(source_tip, dest_tip)`).
/// 3. dest_tip has moved past the graft, but dest→source has already
///    reflected it into source this same run (source's history carries a
///    `Gitprism-Dest-Commit` trailer naming `dest_tip` exactly, checked via
///    [`newest_dest_marker`]) — boundary is *still* the graft point, same as
///    case 2: dest→source's cherry-pick has no real ancestry link back to
///    the dest commit it came from (decisions/0006's merge-base guarantee
///    only covers the original graft, not commits landing on dest
///    independently afterward), so the marker commit can't stand in for a
///    revwalk boundary — it only validates that it's *safe* to still use the
///    graft point, which never moves regardless of what dest→source did.
///
/// Returns the source-space commit to resume from (for [`pending_commits`])
/// on success, or `None` if dest carries history gitprism doesn't recognize
/// by any of the three.
///
/// Case 1 deliberately only ever looks at `dest_tip` itself, not dest's whole
/// history: if `dest_tip` isn't itself a known sync point by that trailer,
/// falling through to cases 2/3 is what actually decides whether it's still
/// safe (see this module's doc comment on the two-direction ordering).
///
/// A `Gitprism-Source-Commit` trailer is just a claim embedded in dest's
/// commit message, though — it names whatever source commit *some* clone
/// last synced from, not necessarily an ancestor of *this* clone's
/// `source_tip`. Trusting it unconditionally would let a source clone that's
/// behind or divergent from the one that produced it rebuild its own (older
/// or different) full snapshot on top of dest and silently drop content the
/// trusted commit already contributed — so it's only accepted once
/// `source_tip` is verified to actually descend from it.
fn dest_resume_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Option<Oid>> {
    let dest_commit = repo
        .find_commit(dest_tip)
        .context("resolving dest's tip commit")?;
    if let Some(value) = trailer_value(
        dest_commit.message().unwrap_or(""),
        "Gitprism-Source-Commit",
    ) {
        let boundary = Oid::from_str(value)
            .with_context(|| format!("parsing Gitprism-Source-Commit trailer {value:?}"))?;

        // The trailer might name a commit this clone doesn't even have — a
        // sibling clone's own source commit is never transmitted to dest,
        // only the filtered commit it produced is, so an unrelated or
        // behind clone has no way to have fetched it. That's just as unsafe
        // to build on as a confirmed non-ancestor, so it's checked (and
        // rejected) before asking libgit2 to compare ancestry, whose own
        // error surface for a missing object isn't a clean `NotFound` here.
        if boundary != source_tip && repo.find_commit(boundary).is_err() {
            return Ok(None);
        }

        let source_tip_descends_from_it = boundary == source_tip
            || repo.graph_descendant_of(source_tip, boundary).with_context(|| {
                format!(
                    "checking whether {source_tip} descends from the Gitprism-Source-Commit trailer {boundary}"
                )
            })?;
        return Ok(source_tip_descends_from_it.then_some(boundary));
    }

    // No Gitprism-Source-Commit trailer on dest's tip at all. The actual
    // resume boundary for source's own pending-commit walk is *always* the
    // real graft point (merge_base) from here on — dest→source's cherry-
    // picks never change source's real ancestry with dest, so this never
    // moves just because dest→source ran. What's actually in question is
    // only whether it's *safe* to build on dest_tip, which the graft point
    // alone can't answer once dest_tip has moved past it.
    let graft_point = repo.merge_base(source_tip, dest_tip).context(
        "no shared history between source and dest for this pair — has `gitprism setup` been run?",
    )?;

    // Case 3: dest hasn't moved past the original graft at all.
    if graft_point == dest_tip {
        return Ok(Some(graft_point));
    }

    // Case 2: dest_tip has moved past the graft with nothing gitprism wrote
    // there directly (case 1 would've caught that) — only safe if
    // dest→source has already reflected dest_tip into source, i.e. source's
    // own history carries a Gitprism-Dest-Commit trailer naming it exactly
    // (this same run, since it's ordered first — see `run`'s doc comment).
    // The boundary is still `graft_point`, not the marker commit itself:
    // dest→source's cherry-pick has no real ancestry link back to the dest
    // commit it came from, so it can't stand in for a revwalk boundary —
    // only validate that dest_tip is accounted for, don't relocate resume.
    let (_, marker_names) = newest_dest_marker(repo, source_tip)?;
    Ok((marker_names == dest_tip).then_some(graft_point))
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

/// Cherry-picks `pair.dest_branch`'s pending commits onto `pair.source_branch`
/// and pushes the result to source's own remote (decisions/0013), one branch
/// pair at a time. A real conflict hard-stops this pair (decisions/0007):
/// whatever applied cleanly before it is still pushed, and the conflict is
/// reported with enough detail for `gitprism resolve` (decisions/0008) to act
/// on later — no trailer is written for the unresolved commit, so the next
/// run's resume-scan naturally retries it once it's resolved.
fn sync_pair_from_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    pair: &BranchPair,
) -> Result<()> {
    let dest_url = config.dest_url()?;

    let mut attempt = 0;
    loop {
        git::fetch(source_root, &dest_url, &pair.dest_branch).with_context(|| {
            format!(
                "fetching dest branch {:?} from {dest_url:?}",
                pair.dest_branch
            )
        })?;
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
            repo.find_branch(&pair.source_branch, git2::BranchType::Local)
                .with_context(|| format!("resolving source branch {:?}", pair.source_branch))?
                .get()
                .peel_to_commit()
                .with_context(|| {
                    format!(
                        "resolving source branch {:?} to a commit",
                        pair.source_branch
                    )
                })?
                .id()
        } else {
            // Only resolved once actually needed — a config that omits
            // [source].url/GITPRISM_SOURCE_URL entirely (decisions/0013) is
            // valid as long as this pair never actually needs to push
            // anything to source, e.g. a branch that never receives
            // independent dest-side commits.
            let source_url = config.source_url()?;
            git::fetch(source_root, &source_url, &pair.source_branch).with_context(|| {
                format!(
                    "fetching source branch {:?} from {source_url:?}",
                    pair.source_branch
                )
            })?;
            repo.find_reference("FETCH_HEAD")
                .context("reading FETCH_HEAD after fetch")?
                .peel_to_commit()
                .context("resolving fetched source branch to a commit")?
                .id()
        };

        // The resume boundary lives in *source's* history, not dest's — every
        // commit gitprism creates on source carries `Gitprism-Dest-Commit`
        // (decisions/0003), including setup's own graft (decisions/0006), so
        // there's always at least one to find. `boundary` names a dest-space
        // commit; verify it's actually an ancestor of (or equal to) the dest
        // tip just fetched before trusting it to scope the pending-commit
        // walk — dest is fast-forward-only in normal operation
        // (requirements/0001), so this should always hold, but a missing
        // object or a genuine non-ancestor both mean something is wrong
        // enough to fail loudly rather than silently mis-walk.
        let (_, boundary) = newest_dest_marker(repo, source_tip)?;
        if boundary != dest_tip {
            let is_ancestor = repo.find_commit(boundary).is_ok()
                && repo
                    .graph_descendant_of(dest_tip, boundary)
                    .with_context(|| {
                        format!("checking whether {dest_tip} descends from {boundary}")
                    })?;
            if !is_ancestor {
                anyhow::bail!(
                    "gitprism sync: source's last-synced dest commit ({boundary}) isn't an ancestor of dest branch {:?}'s current tip ({dest_tip}) — has dest's history been rewritten outside gitprism?",
                    pair.dest_branch
                );
            }
        }

        let build = build_pending_source_tip(repo, config, boundary, dest_tip, source_tip)?;

        if let Some(new_source_tip) = build.new_tip {
            let source_url = config.source_url()?;
            match git::push(
                source_root,
                &source_url,
                new_source_tip,
                &pair.source_branch,
            )? {
                git::PushOutcome::Accepted => {
                    advance_local_source_branch(repo, &pair.source_branch, new_source_tip)?;
                }
                git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES => {
                    attempt += 1;
                    continue;
                }
                git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                    "gitprism sync: pushing {:?} kept losing a fast-forward race after {} retries",
                    pair.source_branch,
                    MAX_RACE_RETRIES
                ),
            }
        }

        if let Some(conflict_oid) = build.conflict {
            anyhow::bail!(
                "gitprism sync: {:?} <- {:?} hit a real conflict at dest commit {conflict_oid} — resolve it with `gitprism resolve {:?}` (decisions/0007, decisions/0008); commits before it were still pushed to source's {:?} branch",
                pair.source_branch,
                pair.dest_branch,
                pair.source_branch,
                pair.source_branch
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
/// Unlike source→dest's `dest_resume_point` (which only ever needs to check
/// dest's tip itself, since gitprism is the sole writer that advances dest),
/// source's tip routinely moves for reasons that have nothing to do with
/// dest→source (ordinary source-side development) — so this has to actually
/// scan source's history rather than look at the tip alone.
///
/// Always finds something for a properly set-up branch: setup's own graft
/// commit (decisions/0006) carries this trailer too.
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

/// The result of [`build_pending_source_tip`]: `new_tip` is the chain's tip
/// if anything was built (`None` only if every pending commit was loop-
/// prevented, or nothing was pending at all — a clean cherry-pick always
/// gets its own marker commit even when it changes nothing, see
/// `build_pending_source_tip`'s doc comment), and `conflict` names the first
/// dest commit that couldn't be cherry-picked cleanly, if any — processing
/// always stops there (decisions/0007's "Consequences": later commits may
/// depend on it).
struct PendingSourceBuild {
    new_tip: Option<Oid>,
    conflict: Option<Oid>,
}

/// Builds, in `repo`'s object database, a chain of new commits reflecting
/// every dest commit between `boundary` and `dest_tip` that cherry-picks
/// cleanly onto source — stopping at the first one that doesn't
/// (decisions/0007). `source_tip` seeds the chain's first parent.
fn build_pending_source_tip(
    repo: &Repository,
    config: &Config,
    boundary: Oid,
    dest_tip: Oid,
    source_tip: Oid,
) -> Result<PendingSourceBuild> {
    let pending = pending_commits(repo, boundary, dest_tip)?;

    let mut parent = source_tip;
    let mut built_any = false;
    for dest_oid in pending {
        let dest_commit = repo
            .find_commit(dest_oid)
            .context("resolving a pending dest commit")?;

        // Loop prevention (decisions/0003): a dest commit that itself came
        // from source (source→dest sync) already exists on source — cherry-
        // picking it back would loop.
        if trailer_value(
            dest_commit.message().unwrap_or(""),
            "Gitprism-Source-Commit",
        )
        .is_some()
        {
            continue;
        }

        let parent_commit = repo
            .find_commit(parent)
            .context("resolving the in-progress source chain's parent")?;
        // A merge commit's mainline (the parent side treated as "unchanged")
        // must be picked explicitly; a non-merge commit requires 0
        // (git2's own convention — passing 1 there is an error).
        let mainline = if dest_commit.parent_count() > 1 { 1 } else { 0 };
        let mut index = repo
            .cherrypick_commit(&dest_commit, &parent_commit, mainline, None)
            .with_context(|| format!("cherry-picking dest commit {dest_oid} onto source"))?;

        if index.has_conflicts() {
            return Ok(PendingSourceBuild {
                new_tip: built_any.then_some(parent),
                conflict: Some(dest_oid),
            });
        }

        let tree_oid = index
            .write_tree_to(repo)
            .with_context(|| format!("writing the merged tree for dest commit {dest_oid}"))?;

        // Unlike source→dest's "don't push an empty commit" rule
        // (requirements/0001, scoped to that direction only), a dest commit
        // that cherry-picks to no change (e.g. dest's edit was already
        // present on source) still needs its own marker commit here, even
        // though its tree is identical to its parent's — the resume boundary
        // *is* the newest Gitprism-Dest-Commit trailer on source's history
        // (decisions/0003), so skipping it would leave that trailer pointing
        // at an older dest oid forever, permanently blocking source→dest
        // from ever recognizing this dest commit (and anything after it) as
        // accounted for (`dest_resume_point`'s case 2).
        parent = build_source_commit(repo, config, parent, &dest_commit, tree_oid)?;
        built_any = true;
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
/// (decisions/0003) that lets a future sync resume from here.
fn build_source_commit(
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
    fn write_config(source_url: &str, dest_url: &str, pairs: &[(&str, &str)]) -> NamedTempFile {
        let pairs_toml: String = pairs
            .iter()
            .map(|(source_branch, dest_branch)| {
                format!(
                    "[[pairs]]\nsource_branch = \"{source_branch}\"\ndest_branch = \"{dest_branch}\"\n"
                )
            })
            .collect();

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [source]
            url = "{source_url}"

            [dest]
            url = "{dest_url}"

            {pairs_toml}
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

        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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

        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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

        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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

        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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

        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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
            &[("main", "main")],
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
        let config_a = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );
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
        let config_b = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );

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
            &[("main", "main")],
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
                &[("main", "main")],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let pair = BranchPair {
            source_branch: "main".to_string(),
            dest_branch: "main".to_string(),
        };
        sync_pair_from_dest(&repo, source_dir.path(), &config, &pair)
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
                &[("main", "main")],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let pair = BranchPair {
            source_branch: "main".to_string(),
            dest_branch: "main".to_string(),
        };

        let err = sync_pair_from_dest(&repo, source_dir.path(), &config, &pair).expect_err(
            "a real same-file conflict must hard-stop, not silently resolve either side",
        );
        let message = format!("{err:#}");
        assert!(message.contains(&conflicting_dest_commit.to_string()));
        assert!(message.contains("resolve"));

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
            write_config(
                "unused",
                &dest_dir.path().display().to_string(),
                &[("main", "main")],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let pair = BranchPair {
            source_branch: "main".to_string(),
            dest_branch: "main".to_string(),
        };

        let err = sync_pair_to_dest(&repo, source_dir.path(), &config, &pair).expect_err(
            "a real same-file conflict must hard-stop, not silently resolve either side",
        );
        let message = format!("{err:#}");
        assert!(message.contains(&conflicting_source_commit.to_string()));
        assert!(message.contains("resolve"));

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
                &[("main", "main")],
            )
            .path(),
        )
        .unwrap();
        let repo = Repository::open(source_dir.path()).unwrap();
        let pair = BranchPair {
            source_branch: "main".to_string(),
            dest_branch: "main".to_string(),
        };
        sync_pair_from_dest(&repo, source_dir.path(), &config, &pair)
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
        sync_pair_to_dest(&repo, source_dir.path(), &config, &pair).expect(
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
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "{}"

            [[pairs]]
            source_branch = "main"
            dest_branch = "main"
            "#,
            dest_dir.path().display()
        )
        .unwrap();

        run(source_dir.path(), config_file.path())
            .expect("a source→dest-only run must not fail merely for lacking a source URL");
    }
}
