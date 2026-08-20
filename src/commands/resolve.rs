//! `gitprism resolve` — see design/decisions/0007-conflict-policy-hard-stop.md,
//! design/decisions/0008-ship-resolve-helper.md, and
//! design/decisions/0015-resolve-real-git-cherry-pick-explicit-continue.md.
//!
//! Two invocations, matching git's own `rebase`/`cherry-pick`/`merge
//! --continue` convention rather than one command guessing which case it is:
//!
//! - `gitprism resolve <branch>` — identifies the oldest still-pending dest
//!   commit for `branch` (recomputing exactly what `sync` would build next,
//!   see `commands::sync::pending_dest_commits`) and drives a real `git
//!   cherry-pick` subprocess against it. Clean: gitprism finishes it
//!   immediately, no human needed. Conflict: real conflict markers are left
//!   in the working tree for the human to resolve with ordinary git.
//! - `gitprism resolve <branch> --continue` — finishes a cherry-pick already
//!   started above, once the human has resolved its conflicts and `git
//!   add`ed them.
//!
//! With `--direction source-to-dest`, the same surface starts an authenticated
//! filtered patch in an isolated linked worktree and `--continue` finalizes its
//! staged index without changing the source checkout (decision 0027).
//!
//! Either way, "finishing" never trusts git's own auto-committed result: it
//! rebuilds the commit via `commands::sync::build_source_commit`, the exact
//! function `sync` itself uses, so a human's hands touching one commit never
//! produces a different shape than gitprism's own automatic ones
//! (decisions/0003, 0010) — then pushes it to source, ff-only
//! (decisions/0009).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use git2::{Oid, Repository, Signature};

use crate::commands::sync::{
    build_dest_commit, build_source_commit, dest_resume_point_for_branch, filter_tree,
    pending_commits, pending_dest_commits,
};
use crate::config::Config;
use crate::exclude;
use crate::git::{self, CherryPickOutcome};
use crate::limits;
use crate::marker;
use crate::policy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    DestToSource,
    SourceToDest,
}

fn read_state_file(path: &Path, description: &str) -> Result<String> {
    let bytes = limits::read_regular_file(path, limits::MAX_STATE_FILE_BYTES, description)?;
    std::str::from_utf8(&bytes)
        .with_context(|| format!("{description} at {} is not valid UTF-8", path.display()))
        .map(str::to_owned)
}

#[cfg(test)]
pub fn run(cwd: &Path, config_path: &Path, branch: &str, r#continue: bool) -> Result<()> {
    run_with_direction(
        cwd,
        config_path,
        branch,
        r#continue,
        Direction::DestToSource,
    )
}

pub fn run_with_direction(
    cwd: &Path,
    config_path: &Path,
    branch: &str,
    r#continue: bool,
    direction: Direction,
) -> Result<()> {
    // Validate before fetching or changing the working tree.
    let state_key = marker::load_key()?;
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism resolve must be run inside an existing git repository (none found at or above {}) — has `gitprism setup` been run?",
            cwd.display()
        )
    })?;
    let source_root = repo
        .workdir()
        .context("gitprism resolve requires a repo with a working tree, not a bare repo")?
        .to_path_buf();

    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        source_root.join(config_path)
    };
    let verified_policy = policy::load(&config_path, &source_root.join(exclude::FILENAME))?;
    let config = verified_policy.config;
    let config_raw = verified_policy.config_raw;
    let ignore_raw = verified_policy.ignore_raw;
    let exclude_list = verified_policy.exclude_list;
    git::validate_branch_name(branch)
        .with_context(|| format!("validating requested branch {branch:?}"))?;

    let branch = match direction {
        Direction::DestToSource => config
            .branches
            .iter()
            .find(|b| b.as_str() == branch)
            .with_context(|| {
                format!("gitprism resolve {branch}: no configured branch {branch:?}")
            })?,
        Direction::SourceToDest => {
            repo.find_branch(branch, git2::BranchType::Local)
                .with_context(|| {
                    format!("gitprism resolve {branch}: no local source branch {branch:?}")
                })?;
            branch
        }
    };

    let _operation_lock = crate::lock::OperationLock::acquire(&repo)?;

    let cherry_pick_head = repo.path().join("CHERRY_PICK_HEAD");

    match direction {
        Direction::DestToSource => {
            if r#continue {
                resolve_continue(
                    &repo,
                    &source_root,
                    &config,
                    branch,
                    &cherry_pick_head,
                    &state_key,
                )
            } else {
                resolve_start(
                    &repo,
                    &source_root,
                    &config,
                    branch,
                    &cherry_pick_head,
                    &state_key,
                )
            }
        }
        Direction::SourceToDest => resolve_source_to_dest(
            &repo,
            &source_root,
            &config,
            branch,
            r#continue,
            &state_key,
            &exclude_list,
            &policy::digest_bytes(config_raw.as_bytes(), ignore_raw.as_bytes()),
        ),
    }
}

/// Verifies `branch` is the one actually checked out — both `resolve_start`
/// (cherry-pick operates on the working tree, so the wrong branch checked
/// out would silently pick onto the wrong history) and `resolve_continue`
/// (finishing needs to read/move the same branch) depend on this.
fn require_branch_checked_out(repo: &Repository, branch: &str) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => {
            anyhow::bail!(
                "gitprism resolve: branch {branch:?} isn't checked out — check it out first with `git checkout <branch>`"
            );
        }
    };
    let head_name = std::str::from_utf8(head.name_bytes()).with_context(|| {
        format!(
            "HEAD names a non-UTF-8 ref {}; gitprism cannot safely select the checked-out branch",
            git::escape_bytes(head.name_bytes())
        )
    })?;
    let head_points_here = head_name == refname;

    if !head_points_here {
        anyhow::bail!(
            "gitprism resolve: branch {branch:?} isn't checked out — check it out first with `git checkout <branch>`"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Source-to-dest resolution (decision 0027)
// ---------------------------------------------------------------------------

struct SourceToDestOperation {
    worktree: PathBuf,
    worktree_repo: Repository,
    state_commit: Oid,
    cherry_pick: Oid,
    source_commit: Oid,
    checkout_base: Oid,
    refname: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceToDestFinishMode {
    Immediate,
    Continue,
}

/// Resolve source→dest in a linked worktree. The source checkout remains
/// untouched while the operator edits the filtered destination-space tree.
#[allow(clippy::too_many_arguments)]
fn resolve_source_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    r#continue: bool,
    state_key: &marker::StateKey,
    exclude_list: &exclude::ExcludeList,
    policy_digest: &str,
) -> Result<()> {
    require_branch_checked_out(repo, branch)?;
    if r#continue {
        let operation = find_source_to_dest_operation(repo, branch, state_key)?;
        finish_source_to_dest(
            repo,
            source_root,
            config,
            branch,
            operation,
            state_key,
            exclude_list,
            policy_digest,
            SourceToDestFinishMode::Continue,
        )
    } else {
        if find_source_to_dest_operation(repo, branch, state_key).is_ok() {
            anyhow::bail!(
                "gitprism resolve: a source-to-dest resolution is already in progress for {branch:?} — resolve it in the reported worktree and run `gitprism resolve <branch> --direction source-to-dest --continue`, or remove it with `git worktree remove --force <path>`"
            );
        }
        start_source_to_dest(
            repo,
            source_root,
            config,
            branch,
            state_key,
            exclude_list,
            policy_digest,
        )
    }
}

fn start_source_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    state_key: &marker::StateKey,
    exclude_list: &exclude::ExcludeList,
    policy_digest: &str,
) -> Result<()> {
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {branch:?}"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
        .id();
    let dest_url = config.dest_url()?;
    if !git::remote_ref_exists(source_root, &dest_url, branch)? {
        anyhow::bail!(
            "gitprism resolve: source-to-dest has no destination ref for mirror-only branch {branch:?}; run sync to create its first destination branch"
        );
    }
    git::fetch(source_root, &dest_url, branch)
        .with_context(|| format!("fetching dest branch {branch:?} from configured remote"))?;
    let dest_tip = repo
        .find_reference("FETCH_HEAD")
        .context("reading FETCH_HEAD after fetch")?
        .peel_to_commit()
        .context("resolving fetched dest branch to a commit")?
        .id();
    let boundary = dest_resume_point_for_branch(repo, source_tip, dest_tip, branch, state_key)?
        .with_context(|| format!("dest branch {branch:?} is not safe to build on"))?;
    let pending = pending_commits(repo, boundary, source_tip)?;

    let mut parent = dest_tip;
    let mut selected = None;
    for source_oid in pending {
        let source_commit = repo.find_commit(source_oid)?;
        if marker::verify(
            &source_commit,
            branch,
            &[marker::Direction::Setup, marker::Direction::DestToSource],
            None,
            state_key,
        )
        .is_some()
        {
            continue;
        }
        let parent_commit = repo.find_commit(parent)?;
        let base_tree = match source_commit.parent(0) {
            Ok(base) => filter_tree(repo, &base.tree()?, Path::new(""), exclude_list)?,
            Err(_) => repo.treebuilder(None)?.write()?,
        };
        let theirs_tree = filter_tree(repo, &source_commit.tree()?, Path::new(""), exclude_list)?;
        match git::merge_tree(source_root, base_tree, parent_commit.tree_id(), theirs_tree)? {
            git::MergeTreeOutcome::Clean(tree) if tree == parent_commit.tree_id() => continue,
            git::MergeTreeOutcome::Clean(tree) => {
                parent = build_dest_commit(
                    repo,
                    config,
                    parent,
                    &source_commit,
                    tree,
                    branch,
                    state_key,
                )?;
            }
            git::MergeTreeOutcome::Conflict { paths } => {
                selected = Some((source_oid, parent, paths));
                break;
            }
        }
    }
    let Some((source_oid, dest_base, paths)) = selected else {
        anyhow::bail!(
            "gitprism resolve: {branch:?} <- {branch:?} has no source-to-dest conflict to resolve"
        )
    };
    if dest_base != dest_tip {
        match git::push(source_root, &dest_url, dest_base, branch)? {
            git::PushOutcome::Accepted => {}
            git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                "gitprism resolve: clean source-to-dest commits before {source_oid} lost a fast-forward race; run sync again"
            ),
        }
    }

    let source_commit = repo.find_commit(source_oid)?;
    let filtered_parent = match source_commit.parent(0) {
        Ok(source_parent) => {
            filter_tree(repo, &source_parent.tree()?, Path::new(""), exclude_list)?
        }
        Err(_) => repo.treebuilder(None)?.write()?,
    };
    let filtered_source = filter_tree(repo, &source_commit.tree()?, Path::new(""), exclude_list)?;
    let signature = Signature::now(&config.committer.name, &config.committer.email)?;
    let synthetic_base = repo.commit(
        None,
        &signature,
        &signature,
        "gitprism resolve: filtered source base",
        &repo.find_tree(filtered_parent)?,
        &[],
    )?;
    let synthetic_base_commit = repo.find_commit(synthetic_base)?;
    let base_commit = repo.commit(
        None,
        &signature,
        &signature,
        "gitprism resolve: destination base",
        &repo.find_commit(dest_base)?.tree()?,
        &[&repo.find_commit(dest_base)?],
    )?;
    let path = resolution_worktree_path()?;
    let worktree_locator = encode_worktree_locator(&path)?;
    let operation_body = format!(
        "Resolve-State: v1\nResolve-Source-Tip: {source_tip}\nResolve-Dest-Tip: {dest_tip}\nResolve-Dest-Base: {dest_base}\nResolve-Checkout-Base: {base_commit}\nResolve-Source-Commit: {source_oid}\nResolve-Patch-Commit: PLACEHOLDER\nResolve-Worktree-Path: {worktree_locator}\nResolve-Policy-SHA256: {policy_digest}\nResolve-Dest-Ref-Existed: true"
    );
    let synthetic_message = marker::build_message(
        &operation_body,
        marker::Direction::ResolveSourceToDestPatch,
        branch,
        source_oid,
        "Gitprism-Source-Commit",
        &[synthetic_base],
        filtered_source,
        &source_commit.author(),
        &signature,
        state_key,
    );
    let synthetic = repo.commit(
        None,
        &source_commit.author(),
        &signature,
        &synthetic_message,
        &repo.find_tree(filtered_source)?,
        &[&synthetic_base_commit],
    )?;
    let operation_body = operation_body.replace(
        "Resolve-Patch-Commit: PLACEHOLDER",
        &format!("Resolve-Patch-Commit: {synthetic}"),
    );
    let state_message = marker::build_message(
        &operation_body,
        marker::Direction::ResolveSourceToDestState,
        branch,
        source_oid,
        "Gitprism-Source-Commit",
        &[base_commit],
        repo.find_commit(base_commit)?.tree_id(),
        &source_commit.author(),
        &signature,
        state_key,
    );
    let base_tree = repo.find_commit(base_commit)?.tree()?;
    let state_commit = repo.commit(
        None,
        &source_commit.author(),
        &signature,
        &state_message,
        &base_tree,
        &[&repo.find_commit(base_commit)?],
    )?;
    let refname = format!("refs/gitprism/resolve/source-to-dest/{branch}");
    repo.reference(&refname, state_commit, true, "gitprism resolve: start")?;
    if let Err(error) = reserve_resolution_worktree_path(&path) {
        let _ = repo
            .find_reference(&refname)
            .and_then(|mut reference| reference.delete());
        return Err(error);
    }
    if let Err(error) = git::worktree_add(source_root, &path, state_commit) {
        let _ = repo
            .find_reference(&refname)
            .and_then(|mut reference| reference.delete());
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    let worktree_repo = match validate_registered_worktree(repo, &path) {
        Ok(worktree_repo) => worktree_repo,
        Err(error) => {
            let _ = repo
                .find_reference(&refname)
                .and_then(|mut reference| reference.delete());
            let _ = git::worktree_remove(source_root, &path);
            return Err(error).context("validating newly-created resolution worktree");
        }
    };
    match git::cherry_pick_no_commit(&path, synthetic, None) {
        Ok(git::CherryPickOutcome::Clean) => {
            let operation = SourceToDestOperation {
                worktree: path,
                worktree_repo,
                state_commit,
                cherry_pick: synthetic,
                source_commit: source_oid,
                checkout_base: base_commit,
                refname,
            };
            finish_source_to_dest(
                repo,
                source_root,
                config,
                branch,
                operation,
                state_key,
                exclude_list,
                policy_digest,
                SourceToDestFinishMode::Immediate,
            )
        }
        Ok(git::CherryPickOutcome::Conflict) => {
            write_source_to_dest_cherry_pick_head(&path, synthetic)?;
            anyhow::bail!(
                "gitprism resolve: {branch:?} source-to-dest conflict at source commit {source_oid} in {paths:?}; resolve the conflict markers in {path:?}, `git add` them, then run `gitprism resolve <branch> --direction source-to-dest --continue`"
            )
        }
        Err(error) => {
            anyhow::bail!(
                "gitprism resolve: source-to-dest operation failed ({error:#}); resolution worktree preserved at {} for inspection or manual cleanup",
                path.display()
            )
        }
    }
}

fn write_source_to_dest_cherry_pick_head(worktree: &Path, patch: Oid) -> Result<()> {
    let path = Repository::open(worktree)?.path().join("CHERRY_PICK_HEAD");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                anyhow::bail!(
                    "gitprism resolve: refusing to use a non-regular linked-worktree CHERRY_PICK_HEAD"
                );
            }
            let actual = read_state_file(&path, "linked-worktree CHERRY_PICK_HEAD")?;
            if actual.trim() != patch.to_string() {
                anyhow::bail!(
                    "gitprism resolve: linked worktree already has a different CHERRY_PICK_HEAD"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            use std::io::Write as _;
            writeln!(file, "{patch}").with_context(|| format!("writing {}", path.display()))?;
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish_source_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    operation: SourceToDestOperation,
    state_key: &marker::StateKey,
    exclude_list: &exclude::ExcludeList,
    policy_digest: &str,
    finish_mode: SourceToDestFinishMode,
) -> Result<()> {
    let worktree_repo = &operation.worktree_repo;
    let state = worktree_repo.head()?.peel_to_commit()?;
    if state.id() != operation.state_commit {
        anyhow::bail!(
            "gitprism resolve: resolution worktree HEAD no longer matches its authenticated operation state"
        );
    }
    if finish_mode == SourceToDestFinishMode::Continue {
        let cherry_pick_head = worktree_repo.path().join("CHERRY_PICK_HEAD");
        let actual = read_state_file(&cherry_pick_head, "linked-worktree CHERRY_PICK_HEAD")
            .with_context(|| {
            "gitprism resolve: source-to-dest continuation requires the authenticated cherry-pick to still be active"
        })?;
        if actual.trim() != operation.cherry_pick.to_string() {
            anyhow::bail!(
                "gitprism resolve: linked worktree CHERRY_PICK_HEAD does not name the authenticated source-to-dest patch"
            );
        }
    }
    if worktree_repo.index()?.has_conflicts() {
        anyhow::bail!(
            "gitprism resolve: source-to-dest resolution still has unresolved conflicts in {:?}",
            operation.worktree
        );
    }
    let checkout_base = state
        .parent(0)
        .context("resolving authenticated source-to-dest checkout base")?;
    let dest_base = checkout_base
        .parent(0)
        .context("resolving original destination tip")?;
    let index_tree = worktree_repo.index()?.write_tree()?;
    let resolved_tree = repo.find_tree(index_tree)?;
    validate_operation_state(
        repo,
        branch,
        &operation,
        &state,
        &checkout_base,
        dest_base.id(),
        policy_digest,
        state_key,
        exclude_list,
    )?;
    reject_excluded_edits(repo, &checkout_base.tree()?, &resolved_tree, exclude_list)?;
    let source_commit = repo.find_commit(operation.source_commit)?;
    let new_dest = build_dest_commit(
        repo,
        config,
        dest_base.id(),
        &source_commit,
        resolved_tree.id(),
        branch,
        state_key,
    )?;
    match git::push(source_root, &config.dest_url()?, new_dest, branch).with_context(|| {
        format!(
            "source-to-dest push failed; resolution worktree and authenticated state remain at {}",
            operation.worktree.display()
        )
    })? {
        git::PushOutcome::Accepted => {
            if let Err(error) = validate_registered_worktree(repo, &operation.worktree) {
                anyhow::bail!(
                    "gitprism resolve: destination push succeeded as {new_dest}, but cleanup was refused because the authenticated resolution worktree changed ({error:#}); operation state and worktree remain at {}",
                    operation.worktree.display()
                );
            }
            if let Err(error) = git::worktree_remove(source_root, &operation.worktree) {
                anyhow::bail!(
                    "gitprism resolve: destination push succeeded as {new_dest}, but cleanup failed ({error:#}); operation state and worktree remain at {}",
                    operation.worktree.display()
                );
            }
            if let Err(error) = repo
                .find_reference(&operation.refname)
                .and_then(|mut reference| reference.delete())
            {
                anyhow::bail!(
                    "gitprism resolve: destination push succeeded as {new_dest}, but cleanup of authenticated operation state failed ({error:#}); remove ref {} after inspection",
                    operation.refname
                );
            }
            Ok(())
        }
        git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
            "gitprism resolve: source-to-dest resolution committed locally as {new_dest}, but dest moved; copy or save the staged resolution from {}, remove that linked worktree, rerun `gitprism resolve <branch> --direction source-to-dest` against the new destination, then reapply the resolution",
            operation.worktree.display()
        ),
    }
}

fn reject_excluded_edits(
    repo: &Repository,
    base: &git2::Tree<'_>,
    resolved: &git2::Tree<'_>,
    exclude_list: &exclude::ExcludeList,
) -> Result<()> {
    let diff = repo.diff_tree_to_tree(Some(base), Some(resolved), None)?;
    let mut excluded = Vec::new();
    for delta in diff.deltas() {
        for path_bytes in [delta.old_file().path_bytes(), delta.new_file().path_bytes()]
            .into_iter()
            .flatten()
        {
            let path = git::path_from_git_bytes(path_bytes)?;
            if exclude_list.is_excluded(path, false) {
                excluded.push(git::escape_bytes(path_bytes));
            }
        }
    }
    excluded.sort();
    excluded.dedup();
    if !excluded.is_empty() {
        anyhow::bail!(
            "gitprism resolve: human resolution edits excluded paths {:?}; restore those paths to their destination-base contents before continuing",
            excluded
        );
    }
    Ok(())
}

fn resolution_worktree_path() -> Result<PathBuf> {
    let root = fs::canonicalize(std::env::temp_dir())
        .context("canonicalizing the temporary directory for resolution worktrees")?;
    let pid = std::process::id();
    let start = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for offset in 0..1024u128 {
        let path = root.join(format!(
            "gitprism-resolve-{pid}-{}",
            start.saturating_add(offset)
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    anyhow::bail!("unable to reserve a unique gitprism resolution worktree path")
}

fn reserve_resolution_worktree_path(path: &Path) -> Result<()> {
    fs::create_dir(path).with_context(|| {
        format!(
            "reserving the signed gitprism resolution worktree path {}",
            path.display()
        )
    })
}

fn encode_worktree_locator(path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(hex_encode(path.as_os_str().as_bytes()))
    }
    #[cfg(not(unix))]
    {
        let text = path
            .to_str()
            .context("resolution worktree path is not representable as UTF-8")?;
        Ok(hex_encode(text.as_bytes()))
    }
}

fn decode_worktree_locator(raw: &str) -> Result<PathBuf> {
    let bytes = hex_decode(raw).context("authenticated resolution worktree locator is invalid")?;
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(OsStr::from_bytes(&bytes)))
    }
    #[cfg(not(unix))]
    {
        let text = std::str::from_utf8(&bytes)
            .context("authenticated resolution worktree path is not valid UTF-8")?;
        Ok(PathBuf::from(text))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn hex_decode(raw: &str) -> Option<Vec<u8>> {
    if !raw.len().is_multiple_of(2) {
        return None;
    }
    raw.as_bytes()
        .chunks_exact(2)
        .map(|pair| Some((hex_digit(pair[0])? << 4) | hex_digit(pair[1])?))
        .collect()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find_source_to_dest_operation(
    repo: &Repository,
    branch: &str,
    key: &marker::StateKey,
) -> Result<SourceToDestOperation> {
    let refname = format!("refs/gitprism/resolve/source-to-dest/{branch}");
    let state = repo
        .find_reference(&refname)
        .with_context(|| {
            format!("reading authenticated source-to-dest operation ref for {branch:?}")
        })?
        .peel_to_commit()
        .context("resolving authenticated source-to-dest operation")?;
    let Some(source_commit) = marker::verify(
        &state,
        branch,
        &[marker::Direction::ResolveSourceToDestState],
        None,
        key,
    ) else {
        anyhow::bail!(
            "gitprism resolve: source-to-dest operation ref failed authenticated verification"
        );
    };
    let message = std::str::from_utf8(state.message_bytes()).with_context(|| {
        format!(
            "authenticated source-to-dest operation state commit {} has a non-UTF-8 message",
            state.id()
        )
    })?;
    let body = marker::parse(message)
        .context("parsing source-to-dest operation state")?
        .body;
    let fields = parse_operation_state(&body)?;
    let checkout_base = fields
        .get("Resolve-Checkout-Base")
        .and_then(|value| Oid::from_str(value).ok())
        .context("source-to-dest operation has no valid checkout base")?;
    let patch = fields
        .get("Resolve-Patch-Commit")
        .and_then(|value| Oid::from_str(value).ok())
        .context("source-to-dest operation has no valid patch commit")?;
    let locator = fields
        .get("Resolve-Worktree-Path")
        .context("source-to-dest operation has no authenticated worktree locator")?;
    let worktree = decode_worktree_locator(locator)?;
    let worktree_repo = validate_registered_worktree(repo, &worktree)?;
    let head_id = {
        let head = worktree_repo
            .head()
            .and_then(|head| head.peel_to_commit())
            .context("resolving the authenticated resolution worktree HEAD")?;
        head.id()
    };
    if head_id != state.id() {
        anyhow::bail!(
            "gitprism resolve: authenticated resolution worktree HEAD does not match its operation state"
        );
    }
    Ok(SourceToDestOperation {
        worktree,
        worktree_repo,
        state_commit: state.id(),
        cherry_pick: patch,
        source_commit,
        checkout_base,
        refname,
    })
}

fn validate_registered_worktree(repo: &Repository, worktree: &Path) -> Result<Repository> {
    if !worktree.is_absolute() {
        anyhow::bail!("authenticated resolution worktree path is not absolute");
    }
    let canonical_worktree = fs::canonicalize(worktree).with_context(|| {
        format!(
            "resolving authenticated resolution worktree path {}",
            worktree.display()
        )
    })?;
    if canonical_worktree != worktree {
        anyhow::bail!(
            "authenticated resolution worktree path {} is a symlink or path substitution",
            worktree.display()
        );
    }
    let metadata = fs::symlink_metadata(worktree)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "authenticated resolution worktree {} is not a real directory",
            worktree.display()
        );
    }
    let worktree_git = worktree.join(".git");
    let git_metadata = fs::symlink_metadata(&worktree_git).with_context(|| {
        format!(
            "reading authenticated resolution worktree metadata {}",
            worktree_git.display()
        )
    })?;
    if !git_metadata.file_type().is_file() || git_metadata.file_type().is_symlink() {
        anyhow::bail!(
            "authenticated resolution worktree {} has unsafe Git metadata",
            worktree.display()
        );
    }
    let expected_common = fs::canonicalize(repo.commondir())?;
    let worktrees = expected_common.join("worktrees");
    let entries = fs::read_dir(&worktrees).with_context(|| {
        format!(
            "reading registered Git worktrees in {}",
            worktrees.display()
        )
    })?;
    for entry in entries {
        let metadata_dir = entry?.path();
        let metadata = fs::symlink_metadata(&metadata_dir)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let gitdir_path = metadata_dir.join("gitdir");
        let Ok(raw) = read_state_file(&gitdir_path, "linked-worktree gitdir") else {
            continue;
        };
        let mut gitdir = PathBuf::from(raw.trim());
        if gitdir.is_relative() {
            gitdir = metadata_dir.join(gitdir);
        }
        let Ok(gitdir) = fs::canonicalize(gitdir) else {
            continue;
        };
        if gitdir != fs::canonicalize(&worktree_git)? {
            continue;
        }
        let worktree_git_contents = read_state_file(&worktree_git, "linked worktree .git")?;
        let pointer = worktree_git_contents
            .trim()
            .strip_prefix("gitdir: ")
            .context("linked worktree .git file has no gitdir pointer")?;
        let pointer = PathBuf::from(pointer);
        let pointer = if pointer.is_absolute() {
            pointer
        } else {
            worktree.join(pointer)
        };
        if fs::canonicalize(pointer)? != fs::canonicalize(&metadata_dir)? {
            anyhow::bail!(
                "authenticated resolution worktree {} has a Git pointer that does not match its registered metadata",
                worktree.display()
            );
        }
        let worktree_repo = Repository::open(worktree)
            .with_context(|| format!("opening linked worktree {}", worktree.display()))?;
        if fs::canonicalize(worktree_repo.commondir())? != expected_common {
            anyhow::bail!(
                "authenticated resolution worktree {} is registered to a different Git repository",
                worktree.display()
            );
        }
        return Ok(worktree_repo);
    }
    anyhow::bail!(
        "authenticated resolution worktree {} is not registered by this Git repository",
        worktree.display()
    )
}

fn parse_operation_state(body: &str) -> Result<std::collections::HashMap<String, String>> {
    const KEYS: [&str; 10] = [
        "Resolve-State",
        "Resolve-Source-Tip",
        "Resolve-Dest-Tip",
        "Resolve-Dest-Base",
        "Resolve-Checkout-Base",
        "Resolve-Source-Commit",
        "Resolve-Patch-Commit",
        "Resolve-Worktree-Path",
        "Resolve-Policy-SHA256",
        "Resolve-Dest-Ref-Existed",
    ];
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() != KEYS.len() {
        anyhow::bail!(
            "source-to-dest operation state must contain exactly {} canonical fields",
            KEYS.len()
        );
    }
    let mut fields = std::collections::HashMap::new();
    for (line, key) in lines.iter().zip(KEYS.iter()) {
        let prefix = format!("{key}: ");
        let value = line
            .strip_prefix(&prefix)
            .filter(|value| !value.is_empty())
            .with_context(|| {
                format!("source-to-dest operation state field {key} is missing or out of order")
            })?;
        if fields.insert((*key).to_owned(), value.to_owned()).is_some() {
            anyhow::bail!("duplicate source-to-dest operation state field {key}");
        }
    }
    Ok(fields)
}

#[allow(clippy::too_many_arguments)]
fn validate_operation_state(
    repo: &Repository,
    branch: &str,
    operation: &SourceToDestOperation,
    state: &git2::Commit<'_>,
    checkout_base: &git2::Commit<'_>,
    dest_base: Oid,
    policy_digest: &str,
    key: &marker::StateKey,
    exclude_list: &exclude::ExcludeList,
) -> Result<()> {
    let Some(source_commit) = marker::verify(
        state,
        branch,
        &[marker::Direction::ResolveSourceToDestState],
        None,
        key,
    ) else {
        anyhow::bail!(
            "gitprism resolve: synthetic source-to-dest commit failed authenticated verification"
        );
    };
    if source_commit != operation.source_commit {
        anyhow::bail!(
            "gitprism resolve: authenticated operation source commit does not match its cherry-pick state"
        );
    }
    if marker::verify(
        &repo.find_commit(operation.cherry_pick)?,
        branch,
        &[marker::Direction::ResolveSourceToDestPatch],
        Some(operation.source_commit),
        key,
    ) != Some(operation.source_commit)
    {
        anyhow::bail!(
            "gitprism resolve: synthetic source-to-dest patch failed authenticated verification"
        );
    }
    let patch = repo.find_commit(operation.cherry_pick)?;
    let source = repo.find_commit(operation.source_commit)?;
    let expected_parent_tree = match source.parent(0) {
        Ok(parent) => filter_tree(repo, &parent.tree()?, Path::new(""), exclude_list)?,
        Err(_) => repo.treebuilder(None)?.write()?,
    };
    let expected_source_tree = filter_tree(repo, &source.tree()?, Path::new(""), exclude_list)?;
    let patch_parent = patch.parent_id(0)?;
    if repo.find_commit(patch_parent)?.tree_id() != expected_parent_tree
        || patch.tree_id() != expected_source_tree
    {
        anyhow::bail!(
            "gitprism resolve: synthetic source-to-dest patch no longer matches the filtered source commit"
        );
    }
    let message = std::str::from_utf8(state.message_bytes()).with_context(|| {
        format!(
            "authenticated source-to-dest operation state commit {} has a non-UTF-8 message",
            state.id()
        )
    })?;
    let body = marker::parse(message)
        .context("parsing authenticated source-to-dest operation state")?
        .body;
    let fields = parse_operation_state(&body)?;
    let source_commit_text = operation.source_commit.to_string();
    let checkout_base_text = checkout_base.id().to_string();
    let dest_base_text = dest_base.to_string();
    if fields.get("Resolve-State") != Some(&"v1".to_owned())
        || fields.get("Resolve-Policy-SHA256") != Some(&policy_digest.to_owned())
        || fields.get("Resolve-Source-Commit") != Some(&source_commit_text)
        || fields.get("Resolve-Checkout-Base") != Some(&checkout_base_text)
        || fields.get("Resolve-Dest-Base") != Some(&dest_base_text)
        || fields.get("Resolve-Patch-Commit") != Some(&operation.cherry_pick.to_string())
        || fields.get("Resolve-Dest-Ref-Existed") != Some(&"true".to_owned())
        || state.id() != operation.state_commit
        || state.parent_id(0)? != checkout_base.id()
        || state.tree_id() != checkout_base.tree_id()
        || checkout_base.parent_id(0)? != dest_base
        || checkout_base.tree_id() != repo.find_commit(dest_base)?.tree_id()
    {
        anyhow::bail!(
            "gitprism resolve: authenticated source-to-dest operation state is stale or uses a different policy"
        );
    }
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)?
        .get()
        .peel_to_commit()?
        .id()
        .to_string();
    if fields.get("Resolve-Source-Tip") != Some(&source_tip) {
        anyhow::bail!(
            "gitprism resolve: source branch moved since this source-to-dest resolution started"
        );
    }
    let recorded_base = Oid::from_str(
        fields
            .get("Resolve-Dest-Base")
            .map(String::as_str)
            .unwrap_or(""),
    )
    .context("parsing recorded source-to-dest destination base")?;
    if recorded_base != dest_base || checkout_base.parent_count() != 1 {
        anyhow::bail!(
            "gitprism resolve: source-to-dest operation base no longer matches its authenticated state"
        );
    }
    if checkout_base.id() != operation.checkout_base {
        anyhow::bail!(
            "gitprism resolve: resolution worktree HEAD no longer matches its authenticated checkout base"
        );
    }
    let recorded_tip = Oid::from_str(
        fields
            .get("Resolve-Dest-Tip")
            .map(String::as_str)
            .unwrap_or(""),
    )
    .context("parsing recorded source-to-dest destination tip")?;
    repo.find_commit(recorded_tip)
        .context("authenticated destination tip is no longer available")?;
    if dest_base != recorded_tip && !repo.graph_descendant_of(dest_base, recorded_tip)? {
        anyhow::bail!(
            "gitprism resolve: authenticated destination base no longer descends from its recorded destination tip"
        );
    }
    Ok(())
}

fn resolve_start(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    cherry_pick_head: &Path,
    state_key: &marker::StateKey,
) -> Result<()> {
    if cherry_pick_head.exists() {
        anyhow::bail!(
            "gitprism resolve: a cherry-pick is already in progress for {branch:?} — resolve its conflicts and run `gitprism resolve <branch> --continue`, or `git cherry-pick --abort` to cancel and start over"
        );
    }
    require_branch_checked_out(repo, branch)?;

    let dest_url = config.dest_url()?;
    git::fetch(source_root, &dest_url, branch)
        .with_context(|| format!("fetching dest branch {branch:?} from configured remote"))?;
    let dest_tip = repo
        .find_reference("FETCH_HEAD")
        .context("reading FETCH_HEAD after fetch")?
        .peel_to_commit()
        .context("resolving fetched dest branch to a commit")?
        .id();
    let source_tip = repo
        .find_branch(branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {branch:?}"))?
        .get()
        .peel_to_commit()
        .with_context(|| format!("resolving source branch {branch:?} to a commit"))?
        .id();

    let pending = pending_dest_commits(repo, source_tip, dest_tip, branch, state_key)
        .with_context(|| {
            format!("has dest branch {branch:?}'s history been rewritten outside gitprism?")
        })?;
    let Some(&dest_oid) = pending.first() else {
        anyhow::bail!(
            "gitprism resolve: {branch:?} <- {branch:?} has nothing pending from dest — nothing to resolve"
        );
    };

    let dest_commit = repo
        .find_commit(dest_oid)
        .context("resolving the dest commit to cherry-pick")?;
    let mainline = (dest_commit.parent_count() > 1).then_some(1);

    match git::cherry_pick(
        source_root,
        dest_oid,
        mainline,
        &config.committer.name,
        &config.committer.email,
    )
    .with_context(|| format!("cherry-picking dest commit {dest_oid} onto source"))?
    {
        CherryPickOutcome::Clean => {
            finish(repo, source_root, config, branch, dest_oid, state_key)?;
            Ok(())
        }
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {branch:?} <- {branch:?} hit a real conflict cherry-picking dest commit {dest_oid} — resolve the conflict markers in {conflicted_paths:?}, `git add` them, then run `gitprism resolve <branch> --continue`"
            );
        }
    }
}

fn resolve_continue(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    cherry_pick_head: &Path,
    state_key: &marker::StateKey,
) -> Result<()> {
    if !cherry_pick_head.exists() {
        anyhow::bail!(
            "gitprism resolve: no cherry-pick in progress for {branch:?} — run `gitprism resolve <branch>` first"
        );
    }
    require_branch_checked_out(repo, branch)?;

    let dest_oid_raw = read_state_file(cherry_pick_head, "CHERRY_PICK_HEAD")
        .context("reading CHERRY_PICK_HEAD")?
        .trim()
        .to_string();
    let dest_oid = Oid::from_str(&dest_oid_raw)
        .with_context(|| format!("parsing CHERRY_PICK_HEAD contents {dest_oid_raw:?}"))?;

    // CHERRY_PICK_HEAD only records *some* commit real git is mid-picking —
    // it has no idea whether that's the commit `resolve_start` actually
    // chose. Without this check, a cherry-pick a human started by hand
    // (unrelated to this pair, or the wrong one entirely) would get stamped
    // with a `Gitprism-Dest-Commit` trailer as if it were the pair's own
    // next pending commit, corrupting resume/loop-prevention for both
    // directions from then on (decisions/0003). A conflicted (or not-yet-
    // committed) cherry-pick never advances HEAD — only `CHERRY_PICK_HEAD`
    // records which commit is being picked, which is exactly why both exist
    // as separate things — so the current HEAD is still the very same
    // source_tip `resolve_start` cherry-picked onto.
    let source_tip = repo
        .head()
        .context("resolving HEAD")?
        .peel_to_commit()
        .context("resolving HEAD to a commit")?
        .id();
    let dest_url = config.dest_url()?;
    git::fetch(source_root, &dest_url, branch)
        .with_context(|| format!("fetching dest branch {branch:?} from configured remote"))?;
    let dest_tip = repo
        .find_reference("FETCH_HEAD")
        .context("reading FETCH_HEAD after fetch")?
        .peel_to_commit()
        .context("resolving fetched dest branch to a commit")?
        .id();
    let pending = pending_dest_commits(repo, source_tip, dest_tip, branch, state_key)
        .with_context(|| {
            format!("has dest branch {branch:?}'s history been rewritten outside gitprism?")
        })?;
    if pending.first() != Some(&dest_oid) {
        anyhow::bail!(
            "gitprism resolve: the in-progress cherry-pick (CHERRY_PICK_HEAD names {dest_oid}) doesn't match {branch:?}'s expected next pending dest commit ({:?}) — this doesn't look like a cherry-pick `gitprism resolve` itself started; finish or abort it manually with plain `git cherry-pick --continue`/`--abort` instead of through gitprism",
            pending.first()
        );
    }

    if repo
        .index()
        .context("reading the repo's index")?
        .has_conflicts()
    {
        let conflicted_paths = conflicted_paths(repo)?;
        anyhow::bail!(
            "gitprism resolve: {branch:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve <branch> --continue` again"
        );
    }

    match git::cherry_pick_continue(source_root, &config.committer.name, &config.committer.email)
        .context("finishing the cherry-pick")?
    {
        CherryPickOutcome::Clean => finish(repo, source_root, config, branch, dest_oid, state_key),
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {branch:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve <branch> --continue` again"
            );
        }
    }
}

/// Every path the repo's index currently reports as conflicted — used only
/// to make an error message actionable, not for any control-flow decision.
fn conflicted_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.index().context("reading the repo's index")?;
    let conflicts = index.conflicts().context("reading the index's conflicts")?;
    let mut paths = Vec::new();
    let mut conflict_count = 0;
    let mut raw_path_bytes: usize = 0;
    for conflict in conflicts {
        conflict_count += 1;
        if conflict_count > limits::MAX_CONFLICT_RECORDS {
            anyhow::bail!(
                "index conflict reporting exceeds the {} record limit",
                limits::MAX_CONFLICT_RECORDS
            );
        }
        let conflict = conflict.context("reading an index conflict entry")?;
        if let Some(entry) = conflict.ancestor.or(conflict.our).or(conflict.their) {
            raw_path_bytes = raw_path_bytes.saturating_add(entry.path.len());
            if raw_path_bytes > limits::MAX_CONFLICT_PATH_BYTES {
                anyhow::bail!(
                    "index conflict paths exceed the {} byte limit",
                    limits::MAX_CONFLICT_PATH_BYTES
                );
            }
            paths.push(git::escape_bytes(&entry.path));
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Replaces whatever commit git's own cherry-pick (clean or, after
/// [`resolve_continue`], human-resolved) just left on `branch` with
/// gitprism's own commit — same tree, but original author preserved,
/// gitprism's configured identity as committer, and the `Gitprism-Dest-Commit`
/// trailer appended (decisions/0003, 0010, 0015), via the exact same
/// `build_source_commit` `sync` itself uses. Then pushes it to source,
/// ff-only (decisions/0009) — a single attempt, not `sync`'s
/// refetch-and-recompute retry loop, since `resolve` is a rare,
/// human-supervised path (decisions/0015's "Consequences").
fn finish(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    dest_oid: Oid,
    state_key: &marker::StateKey,
) -> Result<()> {
    let dest_commit = repo
        .find_commit(dest_oid)
        .context("resolving the cherry-picked dest commit")?;
    let head_commit = repo
        .head()
        .context("resolving HEAD after the cherry-pick")?
        .peel_to_commit()
        .context("resolving HEAD to a commit after the cherry-pick")?;
    let parent = head_commit
        .parent_id(0)
        .context("resolving the cherry-pick's parent commit")?;
    let tree_oid = head_commit.tree_id();

    let new_oid = build_source_commit(
        repo,
        config,
        parent,
        &dest_commit,
        tree_oid,
        branch,
        state_key,
    )?;

    // An atomic compare-and-swap, not a blind force-write: the commit being
    // replaced is this very operation's own just-created artifact (git's
    // auto-commit from the cherry-pick), not independent work, so there's
    // nothing to lose by moving the branch to gitprism's replacement of it —
    // but only if `refname` still names that exact commit at write time.
    // `current_id: head_commit.id()` makes libgit2 itself reject the update
    // (`GIT_EMODIFIED`) if something else moved the branch in the meantime
    // (e.g. a concurrent local commit), rather than silently overwriting it.
    let refname = format!("refs/heads/{branch}");
    repo.reference_matching(
        &refname,
        new_oid,
        true,
        head_commit.id(),
        "gitprism resolve: finish",
    )
    .with_context(|| {
        format!(
            "advancing local branch {branch:?} from {} to {new_oid}",
            head_commit.id()
        )
    })?;
    let new_commit = repo
        .find_commit(new_oid)
        .context("resolving the newly built commit")?;
    repo.set_head(&refname)
        .with_context(|| format!("pointing HEAD back at {refname:?}"))?;
    repo.checkout_tree(new_commit.as_object(), None)
        .context("checking out gitprism's replacement commit")?;

    let source_url = config.source_url()?;
    match git::push(source_root, &source_url, new_oid, branch)? {
        git::PushOutcome::Accepted => Ok(()),
        git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
            "gitprism resolve: {branch:?} was resolved and committed locally, but pushing it to the configured source remote was rejected as a non-fast-forward — fetch/rebase source and push {branch:?} manually"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

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

    #[test]
    fn hostile_branch_names_are_displayed_outside_literal_operator_commands() {
        let branch = "feature-$(touch-pwned)";
        git::validate_branch_name(branch).expect("the hostile test branch remains a valid ref");
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();

        let error = require_branch_checked_out(&repo, branch)
            .expect_err("an unborn repository cannot have the requested branch checked out");
        let rendered = format!("{error:#}");

        assert!(rendered.contains(&format!("branch {branch:?}")));
        assert!(rendered.contains("`git checkout <branch>`"));
        assert!(!rendered.contains(&format!("`git checkout {branch}`")));
    }

    #[test]
    fn legacy_source_to_dest_operation_state_is_rejected_without_a_worktree_locator() {
        let legacy = "Resolve-State: v1\nResolve-Source-Tip: 0000000000000000000000000000000000000000\nResolve-Dest-Tip: 0000000000000000000000000000000000000000\nResolve-Dest-Base: 0000000000000000000000000000000000000000\nResolve-Checkout-Base: 0000000000000000000000000000000000000000\nResolve-Source-Commit: 0000000000000000000000000000000000000000\nResolve-Patch-Commit: 0000000000000000000000000000000000000000\nResolve-Policy-SHA256: 0000000000000000000000000000000000000000000000000000000000000000\nResolve-Dest-Ref-Existed: true";
        assert!(parse_operation_state(legacy).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn conflicted_paths_preserves_invalid_index_bytes() {
        use git2::{IndexEntry, IndexTime};

        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let blob = repo.blob(b"conflict").unwrap();
        let path = b"conflicted-\xff.txt";
        let mut index = repo.index().unwrap();
        for stage in 1..=3 {
            index
                .add(&IndexEntry {
                    ctime: IndexTime::new(0, 0),
                    mtime: IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: blob,
                    flags: (stage as u16) << 12,
                    flags_extended: 0,
                    path: path.to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();

        assert_eq!(
            conflicted_paths(&repo).unwrap(),
            vec!["conflicted-\\xFF.txt"]
        );
    }

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
        let signature = git2::Signature::now("Dest Author", "author@example.com").unwrap();
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

    /// Same shape as `commands::sync`'s own `source_grafted_onto` fixture —
    /// a source repo grafted onto `dest`'s tip, exactly `gitprism setup`'s
    /// output (decisions/0006), checked out on `branch`.
    fn source_grafted_onto(
        source_dir: &Path,
        branch: &str,
        dest_tip: Oid,
        dest_repo: &Repository,
    ) -> Repository {
        let repo = Repository::init(source_dir).unwrap();
        let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
        git::fetch(source_dir, &dest_repo.path().to_string_lossy(), branch).unwrap();
        let fetched_tip_id = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        {
            let fetched_tip = repo.find_commit(fetched_tip_id).unwrap();
            let signature = git2::Signature::now("gitprism", "gitprism@example.com").unwrap();
            let tree = fetched_tip.tree().unwrap();
            let message = marker::build_message(
                &format!("gitprism setup: graft ({})", source_dir.display()),
                marker::Direction::Setup,
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
        repo
    }

    /// Unlike `commands::sync`'s own `add_commit` fixture (which never needs
    /// to touch the working tree, since sync's cherry-pick/diff-apply work
    /// entirely against the object database), this one checks the new
    /// commit out too — `resolve`'s cherry-pick is a *real* `git` subprocess
    /// that refuses to run at all against a working tree/index that's stale
    /// relative to HEAD.
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
        let signature = git2::Signature::now("A Developer", "dev@example.com").unwrap();
        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "a real change",
                &tree,
                &[&tip],
            )
            .unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        oid
    }

    /// An independent change landing directly on dest, conflicting with a
    /// same-named, same-path change source already made independently.
    fn add_independent_dest_commit(dest_repo: &Repository, parent: Oid, file: (&str, &str)) -> Oid {
        add_independent_dest_commit_on(dest_repo, parent, "main", file)
    }

    fn add_independent_dest_commit_on(
        dest_repo: &Repository,
        parent: Oid,
        branch: &str,
        file: (&str, &str),
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
        let signature = git2::Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "conflicting change",
                &tree,
                &[&parent_commit],
            )
            .unwrap()
    }

    #[test]
    fn source_to_dest_resolution_accepts_a_mirror_only_branch_not_in_config() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "base")]);
        dest_repo
            .reference(
                "refs/heads/feature",
                dest_tip,
                true,
                "test mirror-only branch",
            )
            .unwrap();
        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let main_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        source_repo.branch("feature", &main_tip, false).unwrap();
        source_repo.set_head("refs/heads/feature").unwrap();
        source_repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let source_change = add_commit(&source_repo, "feature", &[("f.txt", "source")]);
        let dest_feature =
            add_independent_dest_commit_on(&dest_repo, dest_tip, "feature", ("f.txt", "dest"));
        let source_parent = source_repo.find_commit(source_change).unwrap();
        let marker_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let marker_message = marker::build_message(
            "gitprism test: dest -> source",
            marker::Direction::DestToSource,
            "feature",
            dest_feature,
            "Gitprism-Dest-Commit",
            &[source_change],
            source_parent.tree_id(),
            &marker_signature,
            &marker_signature,
            &marker::load_key().unwrap(),
        );
        source_repo
            .commit(
                Some("refs/heads/feature"),
                &marker_signature,
                &marker_signature,
                &marker_message,
                &source_parent.tree().unwrap(),
                &[&source_parent],
            )
            .unwrap();
        source_repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let config = write_config("unused", &dest_dir.path().display().to_string(), &[]);
        let error = run_with_direction(
            source_dir.path(),
            config.path(),
            "feature",
            false,
            Direction::SourceToDest,
        )
        .expect_err("configured branches must not gate source mirror-only resolution");
        let message = format!("{error:#}");
        assert!(message.contains("source-to-dest"));
        assert!(message.contains("--continue"));
        let operation =
            find_source_to_dest_operation(&source_repo, "feature", &marker::load_key().unwrap())
                .unwrap();
        git::worktree_remove(source_dir.path(), &operation.worktree).unwrap();
        source_repo
            .find_reference(&operation.refname)
            .unwrap()
            .delete()
            .unwrap();
    }

    #[test]
    fn source_to_dest_non_fast_forward_requires_restarting_resolution() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "base")]);
        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let source_change = add_commit(&source_repo, "main", &[("f.txt", "source")]);
        let dest_change = add_independent_dest_commit(&dest_repo, dest_tip, ("f.txt", "dest"));
        let source_parent = source_repo.find_commit(source_change).unwrap();
        let marker_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let marker_message = marker::build_message(
            "gitprism test: dest -> source",
            marker::Direction::DestToSource,
            "main",
            dest_change,
            "Gitprism-Dest-Commit",
            &[source_change],
            source_parent.tree_id(),
            &marker_signature,
            &marker_signature,
            &marker::load_key().unwrap(),
        );
        source_repo
            .commit(
                Some("refs/heads/main"),
                &marker_signature,
                &marker_signature,
                &marker_message,
                &source_parent.tree().unwrap(),
                &[&source_parent],
            )
            .unwrap();
        source_repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            false,
            Direction::SourceToDest,
        )
        .expect_err("the independent same-file changes must conflict");
        let operation =
            find_source_to_dest_operation(&source_repo, "main", &marker::load_key().unwrap())
                .unwrap();
        let worktree_repo = Repository::open(&operation.worktree).unwrap();
        fs::write(operation.worktree.join("f.txt"), "human resolution").unwrap();
        let mut index = worktree_repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        add_independent_dest_commit(&dest_repo, dest_change, ("race.txt", "dest moved"));
        let error = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("a destination race must preserve the staged resolution");
        let message = format!("{error:#}");
        assert!(message.contains("copy or save the staged resolution"));
        assert!(!message.contains("--continue"));

        git::worktree_remove(source_dir.path(), &operation.worktree).unwrap();
        source_repo
            .find_reference(&operation.refname)
            .unwrap()
            .delete()
            .unwrap();
    }

    /// A bare repo standing in for source's own remote, seeded at `tip` —
    /// same convention as `commands::sync`'s own fixture of the same name.
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
        assert_eq!(outcome, git::PushOutcome::Accepted);
        dir
    }

    #[test]
    fn run_fails_loudly_for_an_unconfigured_branch() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config("unused", &dest_dir.path().display().to_string(), &[]);

        let err = run(source_dir.path(), config.path(), "no-such-branch", false)
            .expect_err("an unconfigured branch must not silently succeed");
        assert!(err.to_string().contains("no configured branch"));
    }

    #[test]
    fn run_fails_loudly_when_nothing_is_pending() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path(), "main", false)
            .expect_err("nothing pending must not silently succeed as a resolution");
        assert!(err.to_string().contains("nothing to resolve"));
    }

    #[test]
    fn run_resolves_a_real_conflict_end_to_end() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Source independently changes f.txt...
        add_commit(&source_repo, "main", &[("f.txt", "from source")]);
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
        // ...and dest independently changes the very same file/line.
        let dest_conflict_tip =
            add_independent_dest_commit(&dest_repo, dest_tip, ("f.txt", "from dest"));

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        let err = run(source_dir.path(), config.path(), "main", false)
            .expect_err("a genuine conflict must not silently succeed");
        assert!(err.to_string().contains("hit a real conflict"));
        assert!(err.to_string().contains("--continue"));
        assert!(
            source_dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "a real conflict must leave CHERRY_PICK_HEAD for ordinary git UX"
        );

        // The human resolves it and stages the result.
        std::fs::write(source_dir.path().join("f.txt"), "resolved by human").unwrap();
        let mut index = source_repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        run(source_dir.path(), config.path(), "main", true)
            .expect("finishing a fully-resolved conflict should succeed");

        assert!(!source_dir.path().join(".git/CHERRY_PICK_HEAD").exists());

        let new_source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            new_source_tip.committer().email().unwrap(),
            "gitprism@example.com",
            "the finished commit must carry gitprism's committer identity, not whatever resolved it locally"
        );
        assert_eq!(
            new_source_tip.author().name().unwrap(),
            "Dest Maintainer",
            "the finished commit must preserve the original dest commit's author"
        );
        assert!(
            new_source_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_conflict_tip}"))
        );
        let tree = new_source_tip.tree().unwrap();
        let f = source_repo
            .find_blob(tree.get_name("f.txt").unwrap().id())
            .unwrap();
        assert_eq!(f.content(), b"resolved by human");

        // And it must actually have reached source's remote.
        let remote_repo = Repository::open(source_remote.path()).unwrap();
        let remote_tip = remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(remote_tip.id(), new_source_tip.id());
    }

    #[test]
    fn run_resolves_a_conflict_to_an_empty_result_and_still_creates_a_marker_commit() {
        // A human is entitled to resolve a real conflict by keeping source's
        // existing content exactly, discarding dest's incoming change
        // outright — a legitimate resolution, not a mistake. Git's own
        // `--continue` refuses to finish that without `--allow-empty` (P1),
        // but gitprism must still produce a marker commit: skipping it would
        // leave the `Gitprism-Dest-Commit` resume trailer stuck on the older
        // dest oid forever (decisions/0003), permanently blocking this dest
        // commit — and anything after it — from ever being recognized as
        // accounted for.
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("f.txt", "from source")]);
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
        let dest_conflict_tip =
            add_independent_dest_commit(&dest_repo, dest_tip, ("f.txt", "from dest"));

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        run(source_dir.path(), config.path(), "main", false)
            .expect_err("a genuine conflict must not silently succeed");

        // Resolve by keeping source's own content exactly — the merge nets
        // to no change at all versus source's current tip.
        std::fs::write(source_dir.path().join("f.txt"), "from source").unwrap();
        let mut index = source_repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        run(source_dir.path(), config.path(), "main", true)
            .expect("resolving to an empty result must still finish and succeed");

        assert!(!source_dir.path().join(".git/CHERRY_PICK_HEAD").exists());

        let new_source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            new_source_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_conflict_tip}")),
            "an empty-result resolution must still carry its own marker commit, or the resume trailer gets stuck"
        );
        assert_eq!(
            new_source_tip.committer().email().unwrap(),
            "gitprism@example.com"
        );
        let tree = new_source_tip.tree().unwrap();
        let f = source_repo
            .find_blob(tree.get_name("f.txt").unwrap().id())
            .unwrap();
        assert_eq!(f.content(), b"from source");

        let remote_repo = Repository::open(source_remote.path()).unwrap();
        let remote_tip = remote_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(remote_tip.id(), new_source_tip.id());
    }

    #[test]
    fn run_continue_rejects_a_cherry_pick_gitprism_did_not_start() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Source's own real change — cherry-picking a commit that also
        // touches f.txt onto this tip will conflict.
        add_commit(&source_repo, "main", &[("f.txt", "from source")]);
        let source_tip_before = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip_before);

        // dest has a real, genuinely pending commit for this pair — the one
        // `gitprism resolve main` itself would pick.
        let _dest_pending_tip =
            add_independent_dest_commit(&dest_repo, dest_tip, ("g.txt", "from dest"));

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        // A human starts a cherry-pick by hand — unrelated to this pair's
        // own pending dest commit entirely — and leaves it conflicted.
        let rogue = {
            let parent_commit = source_repo.find_commit(dest_tip).unwrap();
            let mut builder = source_repo
                .treebuilder(Some(&parent_commit.tree().unwrap()))
                .unwrap();
            let blob = source_repo.blob(b"rogue value").unwrap();
            builder
                .insert("f.txt", blob, git2::FileMode::Blob.into())
                .unwrap();
            let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
            let signature = git2::Signature::now("Someone Else", "someone@example.com").unwrap();
            source_repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    "an unrelated manual change",
                    &tree,
                    &[&parent_commit],
                )
                .unwrap()
        };
        assert_eq!(
            git::cherry_pick(
                source_dir.path(),
                rogue,
                None,
                "gitprism",
                "gitprism@example.com",
            )
            .unwrap(),
            git::CherryPickOutcome::Conflict,
            "the rogue pick must actually conflict for this test to mean anything"
        );

        let err = run(source_dir.path(), config.path(), "main", true).expect_err(
            "continuing a cherry-pick gitprism itself never started must not silently succeed",
        );
        assert!(err.to_string().contains("doesn't look like a cherry-pick"));
    }

    #[test]
    fn run_continue_fails_loudly_when_nothing_is_in_progress() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path(), "main", true)
            .expect_err("--continue with nothing in progress must not silently succeed");
        assert!(err.to_string().contains("no cherry-pick in progress"));
    }

    #[test]
    fn run_resolves_a_clean_pick_without_needing_a_human() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Source touches an unrelated file, so dest's independent commit
        // below cherry-picks cleanly instead of conflicting.
        add_commit(&source_repo, "main", &[("only-in-source.txt", "v1")]);
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
        let dest_conflict_tip =
            add_independent_dest_commit(&dest_repo, dest_tip, ("g.txt", "from dest"));

        let config = write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        );

        run(source_dir.path(), config.path(), "main", false)
            .expect("a clean pick should be finished immediately, no --continue needed");

        assert!(!source_dir.path().join(".git/CHERRY_PICK_HEAD").exists());
        let new_source_tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert!(
            new_source_tip
                .message()
                .unwrap()
                .contains(&format!("Gitprism-Dest-Commit: {dest_conflict_tip}"))
        );
        assert_eq!(
            new_source_tip.committer().email().unwrap(),
            "gitprism@example.com"
        );
    }

    #[test]
    fn run_fails_loudly_when_the_source_branch_is_not_checked_out() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        source_repo
            .set_head(&format!(
                "refs/heads/{}",
                "some-other-branch-that-does-not-even-exist-as-a-ref-target"
            ))
            .unwrap();
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path(), "main", false).expect_err(
            "resolving while the wrong branch is checked out must not silently succeed",
        );
        assert!(err.to_string().contains("isn't checked out"));
    }

    #[test]
    fn source_to_dest_resolution_uses_filtered_worktree_and_preserves_dest_owned_excludes() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(
            dest_dir.path(),
            "main",
            &[
                ("f.txt", "base"),
                ("dest-owned.secret", "keep this"),
                (exclude::FILENAME, "dest-owned.secret\n"),
            ],
        );
        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let source_change = add_commit(&source_repo, "main", &[("f.txt", "source")]);
        let dest_change = add_independent_dest_commit(&dest_repo, dest_tip, ("f.txt", "dest"));
        let source_parent = source_repo.find_commit(source_change).unwrap();
        let marker_signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let marker_message = marker::build_message(
            "gitprism sync: dest -> source",
            marker::Direction::DestToSource,
            "main",
            dest_change,
            "Gitprism-Dest-Commit",
            &[source_change],
            source_parent.tree_id(),
            &marker_signature,
            &marker_signature,
            &marker::load_key().unwrap(),
        );
        source_repo
            .commit(
                Some("refs/heads/main"),
                &marker_signature,
                &marker_signature,
                &marker_message,
                &source_parent.tree().unwrap(),
                &[&source_parent],
            )
            .unwrap();
        source_repo
            .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

        let error = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            false,
            Direction::SourceToDest,
        )
        .expect_err("the independent same-file changes must conflict");
        let message = format!("{error:#}");
        assert!(message.contains("source-to-dest"));
        assert!(message.contains("--continue"));

        let worktree = fs::read_dir(source_dir.path().join(".git/worktrees"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let linked_gitdir = PathBuf::from(
            fs::read_to_string(worktree.path().join("gitdir"))
                .unwrap()
                .trim(),
        );
        let worktree_path = linked_gitdir.parent().unwrap().to_path_buf();
        assert!(
            !worktree_path.join("dest-owned.secret").is_file()
                || fs::read_to_string(worktree_path.join("dest-owned.secret")).unwrap()
                    == "keep this"
        );

        let worktree_repo = Repository::open(&worktree_path).unwrap();
        let operation =
            find_source_to_dest_operation(&source_repo, "main", &marker::load_key().unwrap())
                .unwrap();
        assert_eq!(
            worktree_repo.head().unwrap().peel_to_commit().unwrap().id(),
            operation.state_commit
        );
        assert!(worktree_repo.path().join("CHERRY_PICK_HEAD").exists());

        let linked_gitdir_path = worktree.path().join("gitdir");
        let original_linked_gitdir = fs::read(&linked_gitdir_path).unwrap();
        let outside = tempdir().unwrap();
        Repository::init(outside.path()).unwrap();
        let outside_sentinel = outside.path().join("sentinel");
        fs::write(&outside_sentinel, b"must remain untouched").unwrap();
        fs::write(
            &linked_gitdir_path,
            format!("{}\n", outside.path().join(".git").display()),
        )
        .unwrap();
        let tampered = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("a tampered linked-worktree locator must fail closed");
        assert!(format!("{tampered:#}").contains("not registered"));
        assert_eq!(
            fs::read(&outside_sentinel).unwrap(),
            b"must remain untouched"
        );
        fs::write(&linked_gitdir_path, original_linked_gitdir).unwrap();

        let worktree_git_path = worktree_path.join(".git");
        let original_worktree_git = fs::read(&worktree_git_path).unwrap();
        let outside_metadata = outside.path().join("metadata");
        fs::create_dir(&outside_metadata).unwrap();
        fs::write(
            &worktree_git_path,
            format!("gitdir: {}\n", outside_metadata.display()),
        )
        .unwrap();
        let dual_tampered = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("both linked-worktree pointers must be authenticated");
        assert!(format!("{dual_tampered:#}").contains("Git pointer"));
        assert_eq!(
            fs::read(&outside_sentinel).unwrap(),
            b"must remain untouched"
        );
        fs::write(&worktree_git_path, original_worktree_git).unwrap();

        fs::write(worktree_path.join("dest-owned.secret"), "human edit").unwrap();
        fs::write(worktree_path.join("f.txt"), "human resolution").unwrap();
        let mut index = worktree_repo.index().unwrap();
        index.add_path(Path::new("dest-owned.secret")).unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        let rejected = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("excluded resolution edits must not be silently discarded");
        assert!(format!("{rejected:#}").contains("excluded paths"));

        fs::remove_file(worktree_repo.path().join("CHERRY_PICK_HEAD")).unwrap();
        let missing_head = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("continuation without the operation marker must be rejected");
        assert!(format!("{missing_head:#}").contains("still be active"));
        write_source_to_dest_cherry_pick_head(&worktree_path, operation.cherry_pick).unwrap();

        let original_config = fs::read(config.path()).unwrap();
        let mut changed_config = original_config.clone();
        changed_config.extend_from_slice(b"\n# changed during resolution\n");
        fs::write(config.path(), changed_config).unwrap();
        let stale_policy = run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect_err("continuation must reject a changed policy");
        assert!(format!("{stale_policy:#}").contains("different policy"));
        fs::write(config.path(), original_config).unwrap();

        fs::write(worktree_path.join("dest-owned.secret"), "keep this").unwrap();
        fs::write(worktree_path.join("f.txt"), "human resolution").unwrap();
        let mut index = worktree_repo.index().unwrap();
        index.add_path(Path::new("dest-owned.secret")).unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        run_with_direction(
            source_dir.path(),
            config.path(),
            "main",
            true,
            Direction::SourceToDest,
        )
        .expect("the staged human resolution should finish");
        assert!(
            !source_dir.path().join(".git/worktrees").exists()
                || fs::read_dir(source_dir.path().join(".git/worktrees"))
                    .unwrap()
                    .next()
                    .is_none()
        );

        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let dest_tree = dest_tip.tree().unwrap();
        let secret = dest_tree.get_name("dest-owned.secret").unwrap();
        assert_eq!(
            dest_repo.find_blob(secret.id()).unwrap().content(),
            b"keep this"
        );
        let resolved = dest_tree.get_name("f.txt").unwrap();
        assert_eq!(
            dest_repo.find_blob(resolved.id()).unwrap().content(),
            b"human resolution"
        );
    }
}
