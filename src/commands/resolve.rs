//! `gitprism resolve` — see design/decisions/0007-conflict-policy-hard-stop.md,
//! design/decisions/0008-ship-resolve-helper.md, and
//! design/decisions/0015-resolve-real-git-cherry-pick-explicit-continue.md.
//!
//! Dest→source only for now (decisions/0015's "Consequences" — source→dest's
//! diff-apply conflict, decisions/0014, isn't wired in here yet).
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
//! Either way, "finishing" never trusts git's own auto-committed result: it
//! rebuilds the commit via `commands::sync::build_source_commit`, the exact
//! function `sync` itself uses, so a human's hands touching one commit never
//! produces a different shape than gitprism's own automatic ones
//! (decisions/0003, 0010) — then pushes it to source, ff-only
//! (decisions/0009).

use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::commands::sync::{build_source_commit, pending_dest_commits};
use crate::config::Config;
use crate::git::{self, CherryPickOutcome};
use crate::marker;

pub fn run(cwd: &Path, config_path: &Path, branch: &str, r#continue: bool) -> Result<()> {
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
    let config = Config::load(&config_path)?;
    git::validate_branch_name(branch)
        .with_context(|| format!("validating requested branch {branch:?}"))?;

    let branch = config
        .branches
        .iter()
        .find(|b| b.as_str() == branch)
        .with_context(|| format!("gitprism resolve {branch}: no configured branch {branch:?}"))?;

    let cherry_pick_head = repo.path().join("CHERRY_PICK_HEAD");

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

/// Verifies `branch` is the one actually checked out — both `resolve_start`
/// (cherry-pick operates on the working tree, so the wrong branch checked
/// out would silently pick onto the wrong history) and `resolve_continue`
/// (finishing needs to read/move the same branch) depend on this.
fn require_branch_checked_out(repo: &Repository, branch: &str) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    let head_points_here = repo
        .head()
        .ok()
        .and_then(|head_ref| head_ref.name().ok().map(str::to_owned))
        .is_some_and(|name| name == refname);

    if !head_points_here {
        anyhow::bail!(
            "gitprism resolve: branch {branch:?} isn't checked out — check it out first (`git checkout {branch}`)"
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
            "gitprism resolve: a cherry-pick is already in progress for {branch:?} — resolve its conflicts and run `gitprism resolve {branch} --continue`, or `git cherry-pick --abort` to cancel and start over"
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

    match git::cherry_pick(source_root, dest_oid, mainline)
        .with_context(|| format!("cherry-picking dest commit {dest_oid} onto source"))?
    {
        CherryPickOutcome::Clean => {
            finish(repo, source_root, config, branch, dest_oid, state_key)?;
            Ok(())
        }
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {branch:?} <- {branch:?} hit a real conflict cherry-picking dest commit {dest_oid} — resolve the conflict markers in {conflicted_paths:?}, `git add` them, then run `gitprism resolve {branch} --continue`"
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
            "gitprism resolve: no cherry-pick in progress for {branch:?} — run `gitprism resolve {branch}` first"
        );
    }
    require_branch_checked_out(repo, branch)?;

    let dest_oid_raw = std::fs::read_to_string(cherry_pick_head)
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
            "gitprism resolve: {branch:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve {branch} --continue` again"
        );
    }

    match git::cherry_pick_continue(source_root).context("finishing the cherry-pick")? {
        CherryPickOutcome::Clean => finish(repo, source_root, config, branch, dest_oid, state_key),
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {branch:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve {branch} --continue` again"
            );
        }
    }
}

/// Every path the repo's index currently reports as conflicted — used only
/// to make an error message actionable, not for any control-flow decision.
fn conflicted_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.index().context("reading the repo's index")?;
    let mut paths: Vec<String> = index
        .conflicts()
        .context("reading the index's conflicts")?
        .filter_map(|c| c.ok())
        .filter_map(|c| {
            c.ancestor
                .or(c.our)
                .or(c.their)
                .and_then(|entry| String::from_utf8(entry.path).ok())
        })
        .collect();
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
            url = "{source_url}"

            [dest]
            url = "{dest_url}"
            "#,
        )
        .unwrap();
        file
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
                Some("refs/heads/main"),
                &signature,
                &signature,
                "conflicting change",
                &tree,
                &[&parent_commit],
            )
            .unwrap()
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
            git::cherry_pick(source_dir.path(), rogue, None).unwrap(),
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
}
