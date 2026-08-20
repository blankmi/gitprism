//! `gitprism setup` — see design/decisions/0006-setup-uses-real-shared-history.md,
//! design/decisions/0012-config-versioned-in-source.md, and
//! design/decisions/0011-exclude-list-is-gitignore-syntax.md.
//!
//! For every configured branch (decisions/0005, decisions/0017),
//! independently: fetch dest's tip for that branch name (real `git`
//! subprocess, per decisions/0002), then create a same-named branch on source
//! as a brand-new commit — dest's tree plus this same `.gitprism.toml` and
//! `.gitprismignore` — parented directly on dest's tip commit.
//!
//! That graft commit also carries a `Gitprism-Dest-Commit` trailer
//! (decisions/0003) naming dest's tip, so `sync`'s dest→source resume-scan
//! recognizes dest's state as of setup as already reflected into source,
//! without needing a special-cased first run.
//!
//! Like `git` itself, gitprism takes no source-location config: `cwd` is a
//! discovery starting point (walked upward exactly like `git` does from a
//! subdirectory), and setup requires a real, already-`git init`'d repo
//! there. It doesn't create one. A detached HEAD unconditionally hard-fails
//! (a clone always leaves HEAD attached to a branch); beyond that, each
//! configured branch that already exists locally is reconciled with dest via
//! a merge-base check rather than required to match exactly
//! (decisions/0023, superseding decisions/0021's narrower oid-equality
//! precondition): identical tips graft as before, a real but differing
//! shared history produces a two-parent merge commit via the same
//! `merge_tree` primitive both sync directions use (decisions/0016), and no
//! shared history at all hard-fails permanently, with no flag to force it —
//! see [`run`]'s fetch loop below. A local branch not named in
//! `config.branches` (e.g. a stray `ai-setup` or `backup` branch) is outside
//! setup's view entirely: never inspected, never blocking, never touched.
//! Nothing here decides *how* dest is reached; that stays entirely in
//! `.gitprism.toml`'s `[dest] url`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Repository, Signature};

use crate::config::Config;
use crate::exclude::{self};
use crate::git;
use crate::marker::{self, Direction as MarkerDirection};
use crate::policy;

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism setup must be run inside an existing git repository (none found at or above {}) — run `git init` first, same as any other git command",
            cwd.display()
        )
    })?;
    let source_root = repo
        .workdir()
        .context("gitprism setup requires a repo with a working tree, not a bare repo")?
        .to_path_buf();

    // A fresh `git init` already leaves HEAD symbolically pointing at some
    // default branch (commonly "main") while still unborn. If that name
    // collides with a configured branch, rollback below needs to move HEAD
    // off of it before it can delete or reset that branch — captured now so
    // it can be restored to exactly this afterward.
    let original_head = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|head_ref| head_ref.symbolic_target().ok().flatten().map(str::to_owned));

    // `--config`'s default is a bare filename meant to resolve against
    // source's root — same place `.gitprismignore` lives below — not
    // against whatever subdirectory gitprism happened to be invoked from.
    // An explicit absolute path is left untouched.
    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        source_root.join(config_path)
    };
    let policy = policy::load(&config_path, &source_root.join(exclude::FILENAME))?;
    let config_raw = policy.config_raw;
    let ignore_raw = policy.ignore_raw;
    let config = policy.config;
    // Validate the pair secret after the immutable policy pin has passed, and
    // before any fetch or ref/tree mutation.
    let state_key = marker::load_key()?;
    let _operation_lock = crate::lock::OperationLock::acquire(&repo)?;
    // decisions/0021 needs config.branches available before the precondition
    // check below runs (to know which existing local branch names are
    // expected), so parsing config has to move ahead of that check —
    // reordered from where it originally sat in this function.
    if config.branches.is_empty() {
        anyhow::bail!(
            "gitprism setup: no branches configured in {} — nothing to graft",
            config_path.display()
        );
    }

    // Checked once per run, not only once a pre-existing branch turns out to
    // need it (decisions/0016, mirrored from `sync::run`) — an operator on a
    // too-old git gets one clear version message up front instead of a
    // confusing failure the first time a real merge-base reconciliation
    // (decisions/0023) needs `merge_tree`.
    git::ensure_merge_tree_supported()?;

    // decisions/0023 supersedes decisions/0021's mechanism (not just its
    // precondition wording): a detached HEAD remains an unconditional
    // hard-fail (a clone always leaves HEAD attached to a branch), but a
    // local branch is no longer required to either not exist or match dest
    // exactly. Every configured branch that already exists locally gets
    // reconciled via a merge-base check further down (folded into the fetch
    // loop, same as decisions/0021 already did for its narrower oid-equality
    // check). A local branch whose name isn't in `config.branches` is simply
    // invisible to setup — never inspected, never recorded, never blocks a
    // run.
    if repo
        .head_detached()
        .context("checking whether source repo's HEAD is detached")?
    {
        anyhow::bail!(
            "gitprism setup: source repo already has commits and/or branches — setup is a one-time graft onto an empty, freshly-initialized repo, not something to run against existing history"
        );
    }
    let mut pre_existing_branches: HashMap<String, git2::Oid> = HashMap::new();
    for branch_result in repo
        .branches(Some(git2::BranchType::Local))
        .context("listing source repo's existing branches")?
    {
        let (branch, _) = branch_result.context("reading an existing local branch")?;
        let name = branch
            .name()
            .context("reading existing branch's name")?
            .context("existing local branch name is not valid UTF-8")?
            .to_owned();
        if !config.branches.contains(&name) {
            continue;
        }
        let oid = branch
            .get()
            .peel_to_commit()
            .with_context(|| format!("resolving existing branch {name:?} to a commit"))?
            .id();
        pre_existing_branches.insert(name, oid);
    }

    // Fetch every branch's dest tip before writing anything, so a fetch
    // failure partway through never leaves some branches grafted and
    // others not. Also, per decisions/0021 (generalized by decisions/0023),
    // this is where a pre-existing local branch (already confirmed above to
    // be one of config.branches) gets reconciled against dest's own tip —
    // folded into this same pass rather than a second fetch loop, so any
    // hard-fail (no shared history, or a real conflict) surfaces before the
    // commit phase touches anything.
    let dest_url = config.dest_url()?;
    let mut branch_plans = Vec::with_capacity(config.branches.len());
    for branch in &config.branches {
        git::fetch(&source_root, &dest_url, branch)
            .with_context(|| format!("fetching dest branch {branch:?} from configured remote"))?;
        // FETCH_HEAD gets overwritten by the next fetch, so resolve it to a
        // concrete oid right away rather than re-reading it later.
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();
        // decisions/0023: `original_oid` is `Some` exactly when this branch
        // already existed locally before this run — whether it turns out to
        // exactly match dest's tip (single-parent graft, decisions/0021's
        // original case) or to have its own real shared history reconciled
        // via a two-parent merge below. `None` means the branch doesn't
        // exist locally yet, grafted fresh exactly as decisions/0006 always
        // has.
        let (plan, original_oid) = match pre_existing_branches.get(branch) {
            Some(&existing_oid) if existing_oid == dest_tip => {
                // Identical tips: skip merge machinery entirely rather than
                // constructing a two-parent commit whose parents are the
                // same commit (decisions/0023, point 3).
                (BranchPlan::Graft { dest_tip }, Some(existing_oid))
            }
            Some(&existing_oid) => {
                let existing_commit = repo
                    .find_commit(existing_oid)
                    .context("resolving a pre-existing branch's local tip commit")?;
                // decisions/0023's merge-base reconciliation would otherwise
                // happily merge dest's newer tip into setup's *own* prior
                // graft output — its parent already being dest's old tip
                // means a merge-base always exists. setup is a one-time step
                // (decisions/0006, decisions/0012); recognizing its own
                // trailer here restores that guarantee instead of silently
                // re-running against it. `gitprism sync` is the tool for
                // picking up dest's newer commits afterward, not a second
                // `setup`.
                if marker::verify(
                    &existing_commit,
                    branch,
                    &[MarkerDirection::Setup, MarkerDirection::DestToSource],
                    None,
                    &state_key,
                )
                .is_some()
                {
                    anyhow::bail!(
                        "gitprism setup: source's local branch {branch:?} already carries a Gitprism-Dest-Commit trailer — it's setup's own prior output, and setup is a one-time step that must never run against its own graft; run `gitprism sync` instead to pick up dest's newer commits."
                    );
                }
                let base = repo.merge_base(existing_oid, dest_tip).map_err(|_| {
                    anyhow::anyhow!(
                        "gitprism setup: source's local branch {branch:?} has no history in common with dest — gitprism won't merge unrelated histories automatically; merge dest into it yourself with real git first (e.g. `git merge --allow-unrelated-histories <dest-remote>/{branch}`), then re-run setup."
                    )
                })?;
                let base_tree = repo
                    .find_commit(base)
                    .context("resolving a pre-existing branch's merge-base commit")?
                    .tree_id();
                let local_tree = repo
                    .find_commit(existing_oid)
                    .context("resolving a pre-existing branch's local tip commit")?
                    .tree_id();
                let dest_tree = repo
                    .find_commit(dest_tip)
                    .context("resolving dest's fetched tip commit")?
                    .tree_id();
                match git::merge_tree(&source_root, base_tree, local_tree, dest_tree)? {
                    git::MergeTreeOutcome::Clean(merged_tree) => (
                        BranchPlan::Merge {
                            local_tip: existing_oid,
                            dest_tip,
                            merged_tree,
                        },
                        Some(existing_oid),
                    ),
                    git::MergeTreeOutcome::Conflict { paths } => anyhow::bail!(
                        "gitprism setup: {branch:?} has a real conflict between its existing content and dest's tip in {paths:?} — resolve it yourself with real git (e.g. `git merge <dest-remote>/{branch}` in this repo), then re-run setup once done; there is no `gitprism resolve` for this one-time case."
                    ),
                }
            }
            None => (BranchPlan::Graft { dest_tip }, None),
        };
        branch_plans.push((plan, original_oid));
    }

    // Commit phase: every precondition above already held, so failure here
    // should be rare — but if one branch still fails partway (e.g. an
    // invalid branch name), roll back this run's already-touched branches
    // rather than leaving a half-grafted repo behind.
    let control_files = ControlFiles {
        config_raw: &config_raw,
        ignore_raw: &ignore_raw,
    };
    let mut touched_branches: Vec<TouchedBranch> = Vec::with_capacity(config.branches.len());
    for (branch, (plan, original_oid)) in config.branches.iter().zip(&branch_plans) {
        let result = match plan {
            BranchPlan::Graft { dest_tip } => graft_branch(
                &repo,
                &config,
                &control_files,
                branch,
                *dest_tip,
                &state_key,
            ),
            BranchPlan::Merge {
                local_tip,
                dest_tip,
                merged_tree,
            } => merge_branch(
                &repo,
                &config,
                &control_files,
                branch,
                *local_tip,
                *dest_tip,
                *merged_tree,
                &state_key,
            ),
        };
        match result {
            Ok(()) => touched_branches.push(TouchedBranch {
                name: branch.as_str(),
                original_oid: *original_oid,
            }),
            Err(err) => {
                rollback_branches(&repo, &touched_branches, original_head.as_deref());
                return Err(err);
            }
        }
    }

    // The user's own on-disk `.gitprism.toml`/`.gitprismignore` are exactly
    // the bytes just committed into every graft above (`config_raw`,
    // `ignore_raw`) — not arbitrary local content, so clearing them here
    // before checkout is a true no-op, not data loss. Without this, libgit2's
    // safe checkout below flags them as conflicts on the very first run,
    // since it has no baseline yet to recognize the content as identical.
    for filename in [crate::config::FILENAME, exclude::FILENAME] {
        let path = source_root.join(filename);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("removing {} before checkout", path.display()))?;
        }
    }

    // Materialize the graft: point HEAD at the first configured branch and
    // check its tree out into the working directory, same as a fresh `git
    // clone` leaves you on a real, populated checkout rather than an unborn
    // HEAD with content that only exists as unreachable objects. A checkout
    // conflict here is treated the same as a commit-phase failure — roll
    // back every branch this run created rather than leaving grafted
    // branches behind that HEAD never actually landed on.
    if let Some(first) = config.branches.first()
        && let Err(err) = checkout_branch(
            &repo,
            first,
            touched_branches.first().and_then(|b| b.original_oid),
        )
    {
        // The control files were just deleted above to let checkout land
        // them cleanly; a failed checkout must not leave the user without
        // their own bootstrap `.gitprism.toml` — restore them from the exact
        // bytes already read, same as rollback restores the branch refs.
        restore_control_files(&source_root, &config_raw, &ignore_raw);
        rollback_branches(&repo, &touched_branches, original_head.as_deref());
        return Err(err);
    }

    Ok(())
}

fn restore_control_files(source_root: &Path, config_raw: &str, ignore_raw: &str) {
    let _ = fs::write(source_root.join(crate::config::FILENAME), config_raw);
    let _ = fs::write(source_root.join(exclude::FILENAME), ignore_raw);
}

/// What the commit phase does for one configured branch, decided during the
/// fetch/planning loop (decisions/0023) so any hard-fail (no shared history,
/// or a real conflict) surfaces before the commit phase touches anything.
enum BranchPlan {
    /// No local branch yet, or one whose tip is already identical to dest's
    /// (decisions/0021's original case, decisions/0023 point 3) — a plain
    /// single-parent graft onto `dest_tip`.
    Graft { dest_tip: git2::Oid },
    /// A local branch with real shared history that differs from dest's tip
    /// — reconciled into a two-parent commit from `merge_tree`'s clean
    /// result (decisions/0023 point 4).
    Merge {
        local_tip: git2::Oid,
        dest_tip: git2::Oid,
        merged_tree: git2::Oid,
    },
}

/// One branch this run touched, and what rolling it back means.
///
/// `original_oid: None` — setup created this branch fresh; rollback deletes
/// it. `original_oid: Some(oid)` — this branch already existed before this
/// run, whether it matched dest's tip exactly (decisions/0021's "clean clone
/// of dest" case) or had its own real, reconciled history (decisions/0023);
/// rollback resets it back to `oid` instead of deleting it, since deleting a
/// branch the user's own `git clone` (or independent work) produced would be
/// a worse outcome than the failure rollback is guarding against.
struct TouchedBranch<'a> {
    name: &'a str,
    original_oid: Option<git2::Oid>,
}

/// Rolls back `touched`'s branches (see [`TouchedBranch`]), restoring HEAD
/// to `original_head` afterward — used when a later pair fails partway
/// through the commit phase, so a failed run leaves the repo exactly as it
/// found it.
///
/// HEAD has to be moved off of any branch being deleted or reset first:
/// libgit2 refuses to touch the branch HEAD symbolically points at even
/// while still unborn, which a fresh `git init`'s default branch name
/// (commonly "main") can easily collide with.
fn rollback_branches(repo: &Repository, touched: &[TouchedBranch], original_head: Option<&str>) {
    let _ = repo.set_head("refs/heads/gitprism-setup-rollback-scratch");
    for branch in touched {
        match branch.original_oid {
            Some(oid) => {
                let _ = repo.reference(
                    &format!("refs/heads/{}", branch.name),
                    oid,
                    true,
                    "gitprism setup: rollback to pre-existing tip",
                );
            }
            None => {
                if let Ok(mut b) = repo.find_branch(branch.name, git2::BranchType::Local) {
                    let _ = b.delete();
                }
            }
        }
    }
    if let Some(target) = original_head {
        let _ = repo.set_head(target);
    }
}

/// `original_oid` is `branch`'s tip *before* this run touched it — `Some`
/// for a pre-existing branch (decisions/0021, decisions/0023), `None` for
/// one setup just created fresh. See the comment inside for why checkout
/// needs it.
fn checkout_branch(repo: &Repository, branch: &str, original_oid: Option<git2::Oid>) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    // libgit2 checkout's conflict/dirty detection defaults its "baseline" —
    // what it believes is already on disk — to HEAD's *current* tree. By
    // this point `branch`'s ref already points at the graft/merge commit
    // (the commit phase above moved it directly, bypassing the index), and
    // HEAD already symbolically resolves to that same ref — so HEAD's tree
    // and the checkout target are literally the same tree object. libgit2
    // reads that as "nothing changed," so it silently skips materializing
    // every path the graft/merge actually *added* (`.gitprism.toml`,
    // `.gitprismignore`, and — for a real decisions/0023 reconciliation —
    // any path dest introduced that the pre-existing local branch never
    // had): the working tree and index never get them, while HEAD's tree
    // does, which every subsequent `git status` reports as those paths
    // staged for deletion.
    //
    // Detaching HEAD to `branch`'s own *pre-run* tip first (or, if it didn't
    // exist before this run, to a nonexistent scratch ref — the same "make
    // HEAD unborn" trick `rollback_branches` uses below — so checkout sees
    // no baseline at all) gives checkout a real, non-degenerate baseline to
    // diff the target against, so it genuinely creates what's missing
    // instead of assuming there's nothing to do. `set_head` lands HEAD back
    // on `branch` itself afterward, once checkout has actually run against
    // the right comparison.
    match original_oid {
        Some(oid) => repo
            .set_head_detached(oid)
            .with_context(|| format!("detaching HEAD to {branch}'s pre-setup tip {oid}"))?,
        None => repo
            .set_head("refs/heads/gitprism-setup-checkout-scratch")
            .context("detaching HEAD to an unborn scratch ref before checkout")?,
    }
    let commit = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving branch {branch:?} to check it out"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving {refname}'s commit to check it out"))?;
    // Deliberately not forced: source's repo is required to be empty of
    // history above, but its working directory isn't guarded the same way,
    // so a stray local file that collides with dest's content should surface
    // as a checkout conflict, not get silently overwritten.
    repo.checkout_tree(commit.as_object(), None)
        .with_context(|| format!("checking out {refname} into the working directory"))?;
    repo.set_head(&refname)
        .with_context(|| format!("setting HEAD to {refname}"))?;
    Ok(())
}

/// This run's own bootstrap control files, read once up front — bundled
/// together purely to keep `graft_branch`/`merge_branch`'s argument counts
/// down, since the two bytes always travel together.
struct ControlFiles<'a> {
    config_raw: &'a str,
    ignore_raw: &'a str,
}

/// Seeds a tree builder from `base_tree` and layers this same run's
/// `.gitprism.toml`/`.gitprismignore` on top, writing the result — the one
/// step every commit setup creates (plain graft or merge alike) shares.
fn layer_control_files(
    repo: &Repository,
    base_tree: &git2::Tree,
    control_files: &ControlFiles,
) -> Result<git2::Oid> {
    let mut tree_builder = repo
        .treebuilder(Some(base_tree))
        .context("seeding tree builder from the base tree")?;
    let config_blob = repo
        .blob(control_files.config_raw.as_bytes())
        .context("writing .gitprism.toml blob")?;
    let ignore_blob = repo
        .blob(control_files.ignore_raw.as_bytes())
        .context("writing .gitprismignore blob")?;
    tree_builder
        .insert(
            crate::config::FILENAME,
            config_blob,
            git2::FileMode::Blob.into(),
        )
        .context("inserting .gitprism.toml into the tree")?;
    tree_builder
        .insert(exclude::FILENAME, ignore_blob, git2::FileMode::Blob.into())
        .context("inserting .gitprismignore into the tree")?;
    tree_builder.write().context("writing the tree")
}

fn graft_branch(
    repo: &Repository,
    config: &Config,
    control_files: &ControlFiles,
    branch: &str,
    dest_tip: git2::Oid,
    key: &marker::StateKey,
) -> Result<()> {
    let dest_tip = repo
        .find_commit(dest_tip)
        .context("resolving dest's fetched tip commit")?;

    let tree_oid = layer_control_files(
        repo,
        &dest_tip.tree().context("reading dest tip's tree")?,
        control_files,
    )?;
    let tree = repo.find_tree(tree_oid).context("reading the graft tree")?;

    let signature = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = marker::build_message(
        &format!(
            "gitprism setup: graft {branch:?} onto dest's tip {}",
            dest_tip.id()
        ),
        MarkerDirection::Setup,
        branch,
        dest_tip.id(),
        "Gitprism-Dest-Commit",
        &[dest_tip.id()],
        tree.id(),
        &signature,
        &signature,
        key,
    );

    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &signature,
        &signature,
        &message,
        &tree,
        &[&dest_tip],
    )
    .with_context(|| format!("creating graft commit for {branch:?}"))?;

    Ok(())
}

/// The decisions/0023 two-parent path: a pre-existing local branch has real
/// shared history with dest that differs from dest's current tip, and
/// `merge_tree` reported a clean merge. Builds the final commit from that
/// merged tree with `.gitprism.toml`/`.gitprismignore` layered on top, same
/// as `graft_branch` layers them onto dest's tree directly.
///
/// Parent order is load-bearing: the local branch's own tip must be parent
/// 0, since `update_ref` (below, via `refs/heads/{branch}`) requires the
/// ref's current target to already be the commit passed as the *first*
/// parent — which holds here because the ref is currently at `local_tip`,
/// not at `dest_tip`. This also keeps decisions/0019's first-parent-only
/// marker scans treating this branch's own history as primary going
/// forward.
#[allow(clippy::too_many_arguments)]
fn merge_branch(
    repo: &Repository,
    config: &Config,
    control_files: &ControlFiles,
    branch: &str,
    local_tip: git2::Oid,
    dest_tip: git2::Oid,
    merged_tree: git2::Oid,
    key: &marker::StateKey,
) -> Result<()> {
    let local_commit = repo
        .find_commit(local_tip)
        .context("resolving a pre-existing branch's local tip commit")?;
    let dest_commit = repo
        .find_commit(dest_tip)
        .context("resolving dest's fetched tip commit")?;
    let merged_tree = repo
        .find_tree(merged_tree)
        .context("reading merge_tree's resulting tree")?;

    let tree_oid = layer_control_files(repo, &merged_tree, control_files)?;
    let tree = repo.find_tree(tree_oid).context("reading the merge tree")?;

    let signature = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = marker::build_message(
        &format!(
            "gitprism setup: merge dest's tip {} into {branch:?}'s existing history",
            dest_commit.id()
        ),
        MarkerDirection::Setup,
        branch,
        dest_commit.id(),
        "Gitprism-Dest-Commit",
        &[local_commit.id(), dest_commit.id()],
        tree.id(),
        &signature,
        &signature,
        key,
    );

    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &signature,
        &signature,
        &message,
        &tree,
        &[&local_commit, &dest_commit],
    )
    .with_context(|| format!("creating merge commit for {branch:?}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    /// A repo with one commit on `branch`, containing `files` at its root —
    /// no working directory needed, same tree-building technique `setup`
    /// itself uses.
    fn repo_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> git2::Oid {
        let repo = Repository::init(dir).unwrap();
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

    /// A second commit on top of `parent`, layering `files` onto `parent`'s
    /// tree — used to build genuine shared history (a common ancestor both
    /// sides diverge from) for decisions/0023's merge-base reconciliation
    /// tests, as distinct from `repo_with_a_commit_on`'s parentless roots
    /// which share no object database at all across two separate
    /// `Repository::init` calls.
    fn commit_on_top(
        dir: &Path,
        branch: &str,
        parent: git2::Oid,
        files: &[(&str, &str)],
    ) -> git2::Oid {
        let repo = Repository::open(dir).unwrap();
        let parent_commit = repo.find_commit(parent).unwrap();
        let mut builder = repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
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
            "advance",
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    fn write_config(dest_url: &str, branches: &[&str]) -> NamedTempFile {
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

            [dest]
            url = "{dest_url}"
            "#,
        )
        .unwrap();
        file
    }

    #[test]
    fn run_grafts_every_configured_branch_onto_dests_tip() {
        let dest_dir = tempdir().unwrap();
        let main_tip = repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);
        let release_tip = repo_with_a_commit_on(dest_dir.path(), "release-2.0", &[("b.txt", "b")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        fs::write(source_dir.path().join(exclude::FILENAME), "ignored.txt\n").unwrap();

        let config = write_config(
            &dest_dir.path().display().to_string(),
            &["main", "release-2.0"],
        );

        run(source_dir.path(), config.path()).expect("setup should succeed");

        let repo = Repository::open(source_dir.path()).unwrap();

        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(main.parent_id(0).unwrap(), main_tip);
        assert_eq!(main.author().name().unwrap(), "gitprism");
        assert_eq!(main.committer().email().unwrap(), "gitprism@example.com");
        assert!(
            main.message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {main_tip}"))
        );
        let main_tree = main.tree().unwrap();
        assert!(main_tree.get_name("a.txt").is_some());
        assert!(main_tree.get_name(crate::config::FILENAME).is_some());
        assert!(main_tree.get_name(exclude::FILENAME).is_some());

        let release = repo
            .find_branch("release-2.0", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(release.parent_id(0).unwrap(), release_tip);
        let release_tree = release.tree().unwrap();
        assert!(release_tree.get_name("b.txt").is_some());
        assert!(release_tree.get_name(crate::config::FILENAME).is_some());

        // The first configured branch — "main" — must be materialized:
        // HEAD points at it, and its tree is actually checked out on disk.
        assert_eq!(repo.head().unwrap().name().unwrap(), "refs/heads/main");
        assert!(!repo.head_detached().unwrap());
        assert_eq!(
            fs::read_to_string(source_dir.path().join("a.txt")).unwrap(),
            "a"
        );
        assert!(source_dir.path().join(crate::config::FILENAME).is_file());
        assert!(source_dir.path().join(exclude::FILENAME).is_file());
    }

    #[test]
    fn run_leaves_an_unconfigured_local_branch_untouched_and_grafts_the_configured_one() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        // decisions/0023: a local branch not named in `config.branches` (the
        // real-world motivating case: an `ai-setup` or `backup` branch) is
        // entirely outside setup's view — never inspected, never blocking,
        // never touched — not the "source repo already has commits" hard-fail
        // decisions/0021 used to apply here.
        let unrelated_tip =
            repo_with_a_commit_on(source_dir.path(), "unrelated", &[("existing.txt", "x")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("an unconfigured local branch must not block setup");

        let repo = Repository::open(source_dir.path()).unwrap();
        let unrelated = repo
            .find_branch("unrelated", git2::BranchType::Local)
            .expect("the unconfigured branch must survive untouched")
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            unrelated.id(),
            unrelated_tip,
            "the unconfigured branch must not move at all"
        );
        assert!(
            repo.find_branch("main", git2::BranchType::Local).is_ok(),
            "the configured branch, having no local copy, must still get a fresh graft"
        );
    }

    #[test]
    fn run_resolves_a_relative_default_config_against_source_root_not_cwd() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let config_toml = format!(
            "branches = [\"main\"]\n\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"{}\"\n",
            dest_dir.path().display()
        );
        fs::write(source_dir.path().join(crate::config::FILENAME), config_toml).unwrap();

        // A relative "gitprism setup" run from a subdirectory, with the
        // default (relative) --config value, must still find
        // .gitprism.toml at the discovered repo root — same as `git`
        // resolving its own files relative to the repo it discovered, not
        // to the subdirectory it was invoked from.
        let sub_dir = source_dir.path().join("sub");
        fs::create_dir(&sub_dir).unwrap();

        run(&sub_dir, Path::new(crate::config::FILENAME)).expect("setup should succeed");

        let repo = Repository::open(source_dir.path()).unwrap();
        assert!(
            repo.find_branch("main", git2::BranchType::Local).is_ok(),
            "setup should have run against the discovered repo root, not the subdirectory"
        );
    }

    #[test]
    fn run_fails_loudly_instead_of_overwriting_a_conflicting_untracked_file() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        // An untracked local file that collides with dest's tree, but with
        // different content — must not be silently clobbered by checkout.
        fs::write(
            source_dir.path().join("a.txt"),
            "locally written, not dest's",
        )
        .unwrap();

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect_err("a conflicting untracked file must stop setup, not be overwritten");

        assert_eq!(
            fs::read_to_string(source_dir.path().join("a.txt")).unwrap(),
            "locally written, not dest's",
            "the local file must survive a failed checkout untouched"
        );
    }

    #[test]
    fn run_restores_control_files_when_checkout_fails() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        // An untracked, colliding file that forces checkout to fail, same as
        // the test above — but this time the config and exclude-list also
        // live at their default in-repo locations, so the delete-before-
        // checkout step actually touches them.
        fs::write(
            source_dir.path().join("a.txt"),
            "locally written, not dest's",
        )
        .unwrap();
        let config_toml = format!(
            "branches = [\"main\"]\n\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"{}\"\n",
            dest_dir.path().display()
        );
        fs::write(
            source_dir.path().join(crate::config::FILENAME),
            &config_toml,
        )
        .unwrap();
        fs::write(source_dir.path().join(exclude::FILENAME), "some-pattern\n").unwrap();

        run(
            source_dir.path(),
            &source_dir.path().join(crate::config::FILENAME),
        )
        .expect_err("a conflicting untracked file must stop setup, not be overwritten");

        assert_eq!(
            fs::read_to_string(source_dir.path().join(crate::config::FILENAME)).unwrap(),
            config_toml,
            "the user's own .gitprism.toml must survive a failed checkout"
        );
        assert_eq!(
            fs::read_to_string(source_dir.path().join(exclude::FILENAME)).unwrap(),
            "some-pattern\n",
            "the user's own .gitprismignore must survive a failed checkout"
        );
    }

    #[test]
    fn run_fails_loudly_on_an_empty_branches_list() {
        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let config_toml = "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"unused\"\n";
        fs::write(source_dir.path().join(crate::config::FILENAME), config_toml).unwrap();

        let err = run(
            source_dir.path(),
            &source_dir.path().join(crate::config::FILENAME),
        )
        .expect_err("an empty branches list must not silently succeed");

        assert!(err.to_string().contains("no branches configured"));
        assert_eq!(
            fs::read_to_string(source_dir.path().join(crate::config::FILENAME)).unwrap(),
            config_toml,
            "rejecting an empty branches list must not touch the user's config file"
        );
    }

    #[test]
    fn run_treats_a_missing_local_gitprismignore_as_empty() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        // No .gitprismignore written here at all.
        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("a missing .gitprismignore should not fail setup");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = main.tree().unwrap();
        let entry = tree.get_name(exclude::FILENAME).unwrap();
        let blob = repo.find_blob(entry.id()).unwrap();
        assert_eq!(blob.content(), b"");
    }

    #[test]
    fn run_fails_loudly_outside_an_existing_git_repository() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        // A plain directory, deliberately never `git init`'d — same as
        // running any other git command outside a repo.
        let not_a_repo = tempdir().unwrap();
        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        let err = run(not_a_repo.path(), config.path())
            .expect_err("setup must not silently create a repo that was never git-init'd");

        assert!(err.to_string().contains("git repository"));
    }

    #[test]
    fn run_rolls_back_created_branches_when_a_later_branch_fails_to_commit() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);
        repo_with_a_commit_on(dest_dir.path(), "release-2.0", &[("b.txt", "b")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        // Every branch's dest tip is fetched successfully before any commit
        // happens (both branches are configured as a plain name now, source
        // and dest can no longer disagree on it) — so the only way left to
        // fail a *later* branch's commit specifically is a real git-level
        // obstruction on the ref write itself. A stale `.lock` file sitting
        // next to where `refs/heads/release-2.0` would be written is exactly
        // that: a real failure mode (another process — or a crashed prior
        // run — holding the lock), not a contrived one, and it leaves "main"
        // free to succeed first.
        let refs_heads = source_dir.path().join(".git/refs/heads");
        fs::create_dir_all(&refs_heads).unwrap();
        fs::write(refs_heads.join("release-2.0.lock"), "").unwrap();

        let config = write_config(
            &dest_dir.path().display().to_string(),
            &["main", "release-2.0"],
        );

        run(source_dir.path(), config.path())
            .expect_err("a locked ref for a later branch must fail the whole run");

        let repo = Repository::open(source_dir.path()).unwrap();
        assert!(
            repo.find_branch("main", git2::BranchType::Local).is_err(),
            "the earlier branch must be rolled back, not left behind"
        );
    }

    #[test]
    fn run_succeeds_against_a_pre_existing_branch_matching_dests_tip() {
        let dest_dir = tempdir().unwrap();
        let main_tip = repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        // Arrive at the same state a plain `git clone <dest-url> source`
        // would leave behind (decisions/0021): dest's tip fetched and landed
        // on a same-named local branch — not a synthetic shortcut around
        // what setup itself checks for.
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let fetched_tip = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference("refs/heads/main", fetched_tip, true, "simulate git clone")
            .unwrap();

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("setup should accept a clean, unmodified clone of dest");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(main.parent_id(0).unwrap(), main_tip);
        assert!(
            main.message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {main_tip}"))
        );
    }

    #[test]
    fn run_leaves_the_index_in_sync_with_head_for_a_pre_existing_first_branch() {
        // Reproduces the case a plain `git clone <dest-url> source` leaves
        // behind, same setup as
        // run_succeeds_against_a_pre_existing_branch_matching_dests_tip: HEAD
        // is already attached to "main" before setup runs, so `checkout_branch`'s
        // `set_head` is a no-op and only `checkout_head(None)`'s *safe*
        // checkout (diffed against the stale pre-graft index) used to run —
        // which silently skipped materializing the two new control-file
        // paths into the index at all, leaving them staged as deleted
        // relative to HEAD's tree even though both exist on disk and in HEAD.
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let fetched_tip = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference("refs/heads/main", fetched_tip, true, "simulate git clone")
            .unwrap();
        // A real `git clone` doesn't just create the branch ref — it checks
        // it out, populating both the index and the working directory with
        // dest's tree (`a.txt` here). Without this step the index stays
        // completely empty, which doesn't reproduce the bug: `checkout_branch`
        // needs an index that already matches the *old* tree so the graft's
        // added paths (the control files) are genuinely new relative to it.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("setup should accept a clean, unmodified clone of dest");

        let repo = Repository::open(source_dir.path()).unwrap();
        for filename in [crate::config::FILENAME, exclude::FILENAME] {
            let status = repo.status_file(Path::new(filename)).unwrap();
            assert!(
                !status.is_index_deleted(),
                "{filename} must not show as staged for deletion after setup"
            );
            assert!(
                status.is_empty(),
                "{filename} must be fully in sync between HEAD, the index, and the working tree, got {status:?}"
            );
        }
    }

    #[test]
    fn run_fails_loudly_when_a_pre_existing_branch_has_no_history_in_common_with_dest() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        // "main" already exists locally, and is a configured branch name —
        // but it comes from an entirely separate `Repository::init`/object
        // database, so it shares no common ancestor with dest at all. This
        // is decisions/0023's "no merge-base exists" case, distinct from a
        // real (but conflicting) shared history.
        repo_with_a_commit_on(source_dir.path(), "main", &[("independent.txt", "x")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path()).expect_err(
            "a pre-existing branch with no history in common with dest must not be merged or grafted over",
        );

        let message = err.to_string();
        assert!(message.contains("\"main\""), "message was: {message}");
        assert!(
            message.contains("no history in common with dest"),
            "message was: {message}"
        );
        assert!(
            !message.contains("doesn't match dest's current tip"),
            "message was: {message} — this is the distinct no-merge-base case, not the oid-mismatch one"
        );
    }

    #[test]
    fn run_fails_loudly_when_a_pre_existing_branch_is_setups_own_prior_graft() {
        let dest_dir = tempdir().unwrap();
        let dest_first_tip = repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        // First run: an ordinary fresh graft.
        run(source_dir.path(), config.path()).expect("first run should succeed");
        let repo = Repository::open(source_dir.path()).unwrap();
        let first_tip = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // dest advances independently between the two setup runs — exactly
        // the situation a real merge-base reconciliation would otherwise
        // happily merge through, since the graft's own parent is dest's
        // (now-old) tip. setup must still refuse: its own prior output is
        // never something to run setup against again (decisions/0012,
        // decisions/0021's "not decided differently" carried forward by
        // decisions/0023) — `gitprism sync` is the tool for picking up
        // dest's newer commits, not a second `setup`.
        commit_on_top(dest_dir.path(), "main", dest_first_tip, &[("a.txt", "a2")]);

        let err = run(source_dir.path(), config.path()).expect_err(
            "setup must refuse to run again against its own prior graft, not merge over it",
        );

        let message = err.to_string();
        assert!(message.contains("\"main\""), "message was: {message}");
        assert!(message.contains("gitprism sync"), "message was: {message}");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            main.id(),
            first_tip,
            "the branch must be left exactly as the first setup run left it"
        );
    }

    #[test]
    fn run_reconciles_a_pre_existing_branch_with_real_shared_history_into_a_merge_commit() {
        let dest_dir = tempdir().unwrap();
        let base = repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "base")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        // Land the shared base commit locally first (as if a prior partial
        // setup, or a checkout predating dest's later commits) — before dest
        // advances any further, so the fetch below actually captures `base`
        // rather than whatever dest's tip becomes afterward.
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let fetched_base = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(fetched_base, base);
        repo.reference("refs/heads/main", fetched_base, true, "seed local main")
            .unwrap();
        // ... then diverge from it on both sides: locally on a file dest
        // never touches, and on dest on a different file entirely.
        let local_tip = commit_on_top(source_dir.path(), "main", base, &[("local_only.txt", "l")]);
        let dest_tip = commit_on_top(dest_dir.path(), "main", base, &[("dest_only.txt", "d")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("real, non-conflicting shared history must reconcile cleanly");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            main.parent_id(0).unwrap(),
            local_tip,
            "the local branch's own tip must be parent 0"
        );
        assert_eq!(
            main.parent_id(1).unwrap(),
            dest_tip,
            "dest's tip must be parent 1"
        );
        assert!(
            main.message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_tip}"))
        );
        let tree = main.tree().unwrap();
        assert!(tree.get_name("shared.txt").is_some());
        assert!(tree.get_name("local_only.txt").is_some());
        assert!(tree.get_name("dest_only.txt").is_some());
        assert!(tree.get_name(crate::config::FILENAME).is_some());
        assert!(tree.get_name(exclude::FILENAME).is_some());
    }

    #[test]
    fn run_checks_out_a_dest_only_file_from_a_real_reconciliation_not_just_the_control_files() {
        // Same reconciliation as
        // run_reconciles_a_pre_existing_branch_with_real_shared_history_into_a_merge_commit,
        // but this time the pre-existing branch is also the *first* configured
        // one — so it's the one checkout_branch actually materializes. The
        // bug this guards wasn't specific to `.gitprism.toml`/`.gitprismignore`:
        // any path the merge commit adds that the pre-existing local checkout
        // never had (here, `dest_only.txt`, genuinely new from dest's side)
        // is just as vulnerable to being left out of the index and working
        // tree while still landing in HEAD's tree.
        let dest_dir = tempdir().unwrap();
        let base = repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "base")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let fetched_base = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference("refs/heads/main", fetched_base, true, "seed local main")
            .unwrap();
        // Actually check out `base`, same as a real `git clone` would — an
        // unpopulated index/working tree (just moving the ref) doesn't
        // reproduce the bug; see
        // run_leaves_the_index_in_sync_with_head_for_a_pre_existing_first_branch.
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();

        // `commit_on_top` is a plumbing commit — it moves the branch ref but
        // never touches the index or working tree, unlike a real `git
        // commit`. Writing and staging the file directly makes "local" look
        // like a genuine, already-materialized local commit, same as `base`
        // above (a `checkout_head` here would hit this exact same bug a
        // second time, for the same reason: the ref it would check out
        // against just moved directly, underneath it). `local_only.txt`
        // being correct isn't the bug under test — `dest_only.txt` is.
        commit_on_top(source_dir.path(), "main", base, &[("local_only.txt", "l")]);
        fs::write(source_dir.path().join("local_only.txt"), "l").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("local_only.txt")).unwrap();
        index.write().unwrap();
        let dest_tip = commit_on_top(dest_dir.path(), "main", base, &[("dest_only.txt", "d")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        run(source_dir.path(), config.path())
            .expect("real, non-conflicting shared history must reconcile cleanly");

        let repo = Repository::open(source_dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(source_dir.path().join("dest_only.txt")).unwrap(),
            "d",
            "dest's new file must actually be materialized on disk, not just committed into HEAD"
        );
        for filename in ["shared.txt", "local_only.txt", "dest_only.txt"] {
            let status = repo.status_file(Path::new(filename)).unwrap();
            assert!(
                status.is_empty(),
                "{filename} must be fully in sync between HEAD, the index, and the working tree, got {status:?}"
            );
        }
        assert!(
            repo.status_file(Path::new(crate::config::FILENAME))
                .unwrap()
                .is_empty()
        );

        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(main.parent_id(1).unwrap(), dest_tip);
    }

    #[test]
    fn run_fails_loudly_on_a_real_conflict_leaving_the_pre_existing_branch_untouched() {
        let dest_dir = tempdir().unwrap();
        let base = repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "base")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        // Fetch the shared base commit before dest advances any further, so
        // the fetch actually captures `base` rather than dest's later tip.
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let fetched_base = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(fetched_base, base);
        repo.reference("refs/heads/main", fetched_base, true, "seed local main")
            .unwrap();
        // Both sides modify the very same path, differently, since their
        // real common ancestor — a genuine, unresolvable conflict.
        let local_tip = commit_on_top(
            source_dir.path(),
            "main",
            base,
            &[("shared.txt", "local's change")],
        );
        commit_on_top(
            dest_dir.path(),
            "main",
            base,
            &[("shared.txt", "dest's change")],
        );

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path())
            .expect_err("a real content conflict must hard-stop, not guess a resolution");

        let message = err.to_string();
        assert!(message.contains("\"main\""), "message was: {message}");
        assert!(message.contains("shared.txt"), "message was: {message}");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            main.id(),
            local_tip,
            "the pre-existing branch must be left completely untouched by a failed reconciliation"
        );
    }

    #[test]
    fn run_rolls_back_a_pre_existing_branch_to_its_original_tip_when_a_later_branch_fails() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);
        repo_with_a_commit_on(dest_dir.path(), "release-2.0", &[("b.txt", "b")]);

        let source_dir = tempdir().unwrap();
        let repo = Repository::init(source_dir.path()).unwrap();
        // "main" is already a clean clone of dest's tip (decisions/0021) and
        // must survive a later branch's failure by being reset back to this
        // exact oid, not deleted — deleting a branch the user's own `git
        // clone` produced would be worse than the failure being guarded
        // against.
        git::fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .unwrap();
        let original_main_tip = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        repo.reference(
            "refs/heads/main",
            original_main_tip,
            true,
            "simulate git clone",
        )
        .unwrap();

        // Same lock-file trick as
        // run_rolls_back_created_branches_when_a_later_branch_fails_to_commit:
        // force release-2.0's ref write to fail after main's graft succeeds.
        let refs_heads = source_dir.path().join(".git/refs/heads");
        fs::create_dir_all(&refs_heads).unwrap();
        fs::write(refs_heads.join("release-2.0.lock"), "").unwrap();

        let config = write_config(
            &dest_dir.path().display().to_string(),
            &["main", "release-2.0"],
        );

        run(source_dir.path(), config.path())
            .expect_err("a locked ref for a later branch must fail the whole run");

        let repo = Repository::open(source_dir.path()).unwrap();
        let main = repo
            .find_branch("main", git2::BranchType::Local)
            .expect("the pre-existing branch must survive rollback, not be deleted")
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            main.id(),
            original_main_tip,
            "the pre-existing branch must be reset to its original tip, not left on the graft commit"
        );
        assert!(
            repo.find_branch("release-2.0", git2::BranchType::Local)
                .is_err(),
            "the branch this run would have newly created must not be left behind"
        );
    }
}
