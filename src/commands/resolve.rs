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
//! - `gitprism resolve <pair>` — identifies the oldest still-pending dest
//!   commit for `pair` (recomputing exactly what `sync` would build next, see
//!   `commands::sync::pending_dest_commits`) and drives a real `git
//!   cherry-pick` subprocess against it. Clean: gitprism finishes it
//!   immediately, no human needed. Conflict: real conflict markers are left
//!   in the working tree for the human to resolve with ordinary git.
//! - `gitprism resolve <pair> --continue` — finishes a cherry-pick already
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
use crate::config::{BranchPair, Config};
use crate::git::{self, CherryPickOutcome};

pub fn run(cwd: &Path, config_path: &Path, pair: &str, r#continue: bool) -> Result<()> {
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

    let branch_pair = config
        .pairs
        .iter()
        .find(|p| p.source_branch == pair)
        .with_context(|| {
            format!("gitprism resolve {pair}: no configured pair has source_branch {pair:?}")
        })?;

    let cherry_pick_head = repo.path().join("CHERRY_PICK_HEAD");

    if r#continue {
        resolve_continue(&repo, &source_root, &config, branch_pair, &cherry_pick_head)
    } else {
        resolve_start(&repo, &source_root, &config, branch_pair, &cherry_pick_head)
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
    pair: &BranchPair,
    cherry_pick_head: &Path,
) -> Result<()> {
    if cherry_pick_head.exists() {
        anyhow::bail!(
            "gitprism resolve: a cherry-pick is already in progress for {:?} — resolve its conflicts and run `gitprism resolve {} --continue`, or `git cherry-pick --abort` to cancel and start over",
            pair.source_branch,
            pair.source_branch
        );
    }
    require_branch_checked_out(repo, &pair.source_branch)?;

    let dest_url = config.dest_url()?;
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

    let pending = pending_dest_commits(repo, source_tip, dest_tip).with_context(|| {
        format!(
            "has dest branch {:?}'s history been rewritten outside gitprism?",
            pair.dest_branch
        )
    })?;
    let Some(&dest_oid) = pending.first() else {
        anyhow::bail!(
            "gitprism resolve: {:?} <- {:?} has nothing pending from dest — nothing to resolve",
            pair.source_branch,
            pair.dest_branch
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
            finish(repo, source_root, config, pair, dest_oid)?;
            Ok(())
        }
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {:?} <- {:?} hit a real conflict cherry-picking dest commit {dest_oid} — resolve the conflict markers in {conflicted_paths:?}, `git add` them, then run `gitprism resolve {} --continue`",
                pair.source_branch,
                pair.dest_branch,
                pair.source_branch
            );
        }
    }
}

fn resolve_continue(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    pair: &BranchPair,
    cherry_pick_head: &Path,
) -> Result<()> {
    if !cherry_pick_head.exists() {
        anyhow::bail!(
            "gitprism resolve: no cherry-pick in progress for {:?} — run `gitprism resolve {}` first",
            pair.source_branch,
            pair.source_branch
        );
    }
    require_branch_checked_out(repo, &pair.source_branch)?;

    let dest_oid_raw = std::fs::read_to_string(cherry_pick_head)
        .context("reading CHERRY_PICK_HEAD")?
        .trim()
        .to_string();
    let dest_oid = Oid::from_str(&dest_oid_raw)
        .with_context(|| format!("parsing CHERRY_PICK_HEAD contents {dest_oid_raw:?}"))?;

    if repo
        .index()
        .context("reading the repo's index")?
        .has_conflicts()
    {
        let conflicted_paths = conflicted_paths(repo)?;
        anyhow::bail!(
            "gitprism resolve: {:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve {} --continue` again",
            pair.source_branch,
            pair.source_branch
        );
    }

    match git::cherry_pick_continue(source_root).context("finishing the cherry-pick")? {
        CherryPickOutcome::Clean => finish(repo, source_root, config, pair, dest_oid),
        CherryPickOutcome::Conflict => {
            let conflicted_paths = conflicted_paths(repo)?;
            anyhow::bail!(
                "gitprism resolve: {:?} still has unresolved conflicts in {conflicted_paths:?} — resolve them and `git add` before running `gitprism resolve {} --continue` again",
                pair.source_branch,
                pair.source_branch
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
/// [`resolve_continue`], human-resolved) just left on `pair.source_branch`
/// with gitprism's own commit — same tree, but original author preserved,
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
    pair: &BranchPair,
    dest_oid: Oid,
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

    let new_oid = build_source_commit(repo, config, parent, &dest_commit, tree_oid)?;

    // A plain, non-forced local update: the commit being replaced is this
    // very operation's own just-created artifact (git's auto-commit from the
    // cherry-pick), not independent work, so there's nothing to lose by
    // moving the branch to gitprism's replacement of it.
    let refname = format!("refs/heads/{}", pair.source_branch);
    repo.reference(&refname, new_oid, true, "gitprism resolve: finish")
        .with_context(|| {
            format!(
                "advancing local branch {:?} to {new_oid}",
                pair.source_branch
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
    match git::push(source_root, &source_url, new_oid, &pair.source_branch)? {
        git::PushOutcome::Accepted => Ok(()),
        git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
            "gitprism resolve: {:?} was resolved and committed locally, but pushing it to source was rejected as a non-fast-forward — fetch/rebase source and push {:?} to {source_url:?} manually",
            pair.source_branch,
            pair.source_branch
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

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
            repo.commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &format!(
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
    fn run_fails_loudly_for_an_unconfigured_pair() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config("unused", &dest_dir.path().display().to_string(), &[]);

        let err = run(source_dir.path(), config.path(), "no-such-pair", false)
            .expect_err("an unconfigured pair must not silently succeed");
        assert!(err.to_string().contains("no configured pair"));
    }

    #[test]
    fn run_fails_loudly_when_nothing_is_pending() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );

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
            &[("main", "main")],
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
    fn run_continue_fails_loudly_when_nothing_is_in_progress() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);
        let source_dir = tempdir().unwrap();
        source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );

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
            &[("main", "main")],
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
        let config = write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &[("main", "main")],
        );

        let err = run(source_dir.path(), config.path(), "main", false).expect_err(
            "resolving while the wrong branch is checked out must not silently succeed",
        );
        assert!(err.to_string().contains("isn't checked out"));
    }
}
