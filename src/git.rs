//! Push/fetch go through a real `git` subprocess, not `git2-rs` — see
//! design/decisions/0002-hybrid-git-backend.md. This is the fast-forward-only
//! network path, so it should inherit the same credential helpers/SSH
//! agent/`GIT_ASKPASS` handling a human running `git` would get, rather than
//! a library reimplementation of it.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Fetch `refspec` from `url` into `repo_dir`'s local object database,
/// landing at `FETCH_HEAD` — same as running `git fetch <url> <refspec>` by
/// hand inside `repo_dir`.
pub fn fetch(repo_dir: &Path, url: &str, refspec: &str) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("fetch")
        .arg(url)
        .arg(refspec)
        .status()
        .with_context(|| format!("running git fetch {url} {refspec}"))?;

    if !status.success() {
        anyhow::bail!("git fetch {url} {refspec} failed ({status})");
    }

    Ok(())
}

/// What happened to a [`push`] attempt: either it landed, or it was
/// rejected specifically for being a non-fast-forward update — the one
/// failure decisions/0009 says is worth refetching dest and recomputing for.
/// Every other failure (auth, hooks, network, ...) is a plain `Err`.
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    RejectedNotFastForward,
}

/// Push `commit` (a local oid already in `repo_dir`'s object database) to
/// `dest_branch` on `url` — same as `git push <url> <commit>:<dest_branch>`
/// by hand. Deliberately no `--force`: git's own default refuses a
/// non-fast-forward update, which is exactly the fast-forward-only
/// constraint source→dest sync requires (requirements/0001, decisions/0009).
pub fn push(
    repo_dir: &Path,
    url: &str,
    commit: git2::Oid,
    dest_branch: &str,
) -> Result<PushOutcome> {
    let refspec = format!("{commit}:refs/heads/{dest_branch}");
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("push")
        .arg(url)
        .arg(&refspec)
        .output()
        .with_context(|| format!("running git push {url} {refspec}"))?;

    if output.status.success() {
        return Ok(PushOutcome::Accepted);
    }

    // git's own wording for "the ref moved since we last looked" — the only
    // case decisions/0009 wants recomputed and retried. Everything else
    // (bad credentials, a rejecting pre-receive hook, a dropped connection,
    // ...) must surface immediately instead of being silently retried.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("[rejected]")
        && (stderr.contains("fetch first") || stderr.contains("non-fast-forward"))
    {
        return Ok(PushOutcome::RejectedNotFastForward);
    }

    anyhow::bail!(
        "git push {refspec} to {url} failed ({}): {stderr}",
        output.status
    );
}

/// What a real `git cherry-pick` subprocess attempt (decisions/0015) came
/// back with. `Clean` covers both "applied with no conflicts" and, for
/// [`cherry_pick_continue`], "the human's resolution is now complete" —
/// either way there's a new commit on the checked-out branch ready for
/// `gitprism resolve` to rebuild with the right identity/trailer. `Conflict`
/// means real, unresolved conflict markers are sitting in the working tree
/// (or still are, for `cherry_pick_continue`) — a human, not a bug.
#[derive(Debug, PartialEq, Eq)]
pub enum CherryPickOutcome {
    Clean,
    Conflict,
}

/// Cherry-picks `commit` onto whatever's currently checked out in `repo_dir`
/// via a real `git cherry-pick` subprocess — deliberately not `git2`, unlike
/// every other cherry-pick in this codebase (`commands::sync`'s is entirely
/// in the object database and never touches the working tree). `resolve`
/// needs the opposite: a real conflict must leave real working-tree conflict
/// markers and `CHERRY_PICK_HEAD` for the human to resolve with ordinary git
/// (decisions/0008, decisions/0015).
///
/// `mainline` is the 1-based parent number for a merge commit (git's own
/// `-m` convention), `None` for an ordinary commit. `GIT_EDITOR=true` keeps
/// even a clean apply from ever blocking on an interactive commit-message
/// prompt — the resulting commit's author/committer/message all get replaced
/// by `gitprism resolve` regardless of whether it applied cleanly or a human
/// had to intervene (decisions/0003, 0010, 0015).
///
/// Told apart from a genuine, unrelated failure (bad sha, dirty working
/// tree, ...) by git's own exit-code convention: `0` clean, `1` a real
/// conflict needing resolution, anything else a plain `Err`.
pub fn cherry_pick(
    repo_dir: &Path,
    commit: git2::Oid,
    mainline: Option<u32>,
) -> Result<CherryPickOutcome> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo_dir).arg("cherry-pick");
    if let Some(mainline) = mainline {
        cmd.arg("-m").arg(mainline.to_string());
    }
    cmd.arg(commit.to_string());
    cmd.env("GIT_EDITOR", "true");

    let output = cmd
        .output()
        .with_context(|| format!("running git cherry-pick {commit}"))?;
    cherry_pick_outcome(output, || format!("git cherry-pick {commit}"))
}

/// Finishes a cherry-pick already in progress in `repo_dir` (started by
/// [`cherry_pick`]) after the human has resolved its conflicts and `git
/// add`ed them — the real `git cherry-pick --continue`, so it shares its
/// exit-code convention and `GIT_EDITOR` handling with [`cherry_pick`].
pub fn cherry_pick_continue(repo_dir: &Path) -> Result<CherryPickOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .arg("cherry-pick")
        .arg("--continue")
        .env("GIT_EDITOR", "true")
        .output()
        .context("running git cherry-pick --continue")?;
    cherry_pick_outcome(output, || "git cherry-pick --continue".to_string())
}

fn cherry_pick_outcome(
    output: std::process::Output,
    describe: impl FnOnce() -> String,
) -> Result<CherryPickOutcome> {
    match output.status.code() {
        Some(0) => Ok(CherryPickOutcome::Clean),
        // git's own convention: 1 is a real conflict needing resolution,
        // distinct from 128 (a fatal, unrelated usage error) or any other
        // unexpected code.
        Some(1) => Ok(CherryPickOutcome::Conflict),
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("{} failed ({}): {stderr}", describe(), output.status)
        }
    }
}

#[cfg(test)]
mod tests {
    use git2::Repository;
    use tempfile::tempdir;

    use super::*;

    /// A repo with one commit (an empty tree) on `branch`, suitable as a
    /// fetch source in tests — no working directory needed.
    fn repo_with_a_commit_on(dir: &Path, branch: &str) -> git2::Oid {
        let repo = Repository::init(dir).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();

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

    #[test]
    fn fetch_lands_the_remote_branch_at_fetch_head() {
        let dest_dir = tempdir().unwrap();
        let expected = repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .expect("fetching an existing branch should succeed");

        let source_repo = Repository::open(source_dir.path()).unwrap();
        let fetched = source_repo
            .find_reference("FETCH_HEAD")
            .expect("FETCH_HEAD should exist after a successful fetch")
            .peel_to_commit()
            .unwrap();

        assert_eq!(fetched.id(), expected);
    }

    #[test]
    fn fetch_fails_loudly_on_an_unknown_ref() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        let err = fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "no-such-branch",
        )
        .expect_err("fetching a branch that doesn't exist must not silently succeed");

        assert!(err.to_string().contains("git fetch"));
    }

    /// A commit built directly in `repo`'s object database, without updating
    /// any ref — the shape `sync` actually pushes (a bare oid, not a local
    /// branch).
    fn commit_on(repo: &Repository, parent: git2::Oid, file: (&str, &str)) -> git2::Oid {
        let parent_commit = repo.find_commit(parent).unwrap();
        let mut builder = repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = repo.blob(file.1.as_bytes()).unwrap();
        builder
            .insert(file.0, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();

        repo.commit(
            None,
            &signature,
            &signature,
            "second",
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    #[test]
    fn push_fast_forwards_a_bare_dest_branch() {
        let dest_dir = tempdir().unwrap();
        Repository::init_bare(dest_dir.path()).unwrap();

        let source_dir = tempdir().unwrap();
        let base = repo_with_a_commit_on(source_dir.path(), "work");
        let source_repo = Repository::open(source_dir.path()).unwrap();
        let child = commit_on(&source_repo, base, ("b.txt", "b"));

        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            child,
            "main",
        )
        .expect("pushing a fresh branch to an empty bare repo should succeed");
        assert_eq!(outcome, PushOutcome::Accepted);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(dest_tip.id(), child);
    }

    #[test]
    fn push_reports_a_rejection_instead_of_erroring_on_a_lost_fast_forward_race() {
        let dest_dir = tempdir().unwrap();
        let dest_tip = repo_with_a_commit_on(dest_dir.path(), "main");
        // Make dest's checked-out branch something else, so pushing to
        // "main" isn't rejected merely for being the checked-out branch.
        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        dest_repo.set_head("refs/heads/unrelated").unwrap();

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let source_repo = Repository::open(source_dir.path()).unwrap();
        // A commit unrelated to dest's actual tip — pushing it as "main"
        // would discard dest_tip, which a non-forced push must refuse.
        let tree_oid = source_repo.treebuilder(None).unwrap().write().unwrap();
        let tree = source_repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let unrelated = source_repo
            .commit(None, &signature, &signature, "unrelated", &tree, &[])
            .unwrap();

        // decisions/0009: this is the one failure sync should treat as
        // retryable (refetch and recompute), so it comes back as a reported
        // outcome, not an `Err` — that distinction is what lets sync tell it
        // apart from a genuine, non-retryable failure below.
        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            unrelated,
            "main",
        )
        .expect("a lost fast-forward race is a reported outcome, not an error");
        assert_eq!(outcome, PushOutcome::RejectedNotFastForward);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let still = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still.id(),
            dest_tip,
            "a rejected push must not move dest's branch"
        );
    }

    #[test]
    fn push_fails_loudly_on_an_unrelated_error_instead_of_reporting_a_rejection() {
        let source_dir = tempdir().unwrap();
        let commit = repo_with_a_commit_on(source_dir.path(), "main");

        // Not a fast-forward race at all — there's no dest repository here
        // to race with. Decisions/0009 only calls for retrying a lost
        // fast-forward race; every other failure (this one included) must
        // surface as an `Err`, not get misreported as a retryable rejection.
        let err = push(
            source_dir.path(),
            "/nonexistent/not-a-remote",
            commit,
            "main",
        )
        .expect_err("an unreachable remote must not silently succeed");

        assert!(err.to_string().contains("git push"));
    }

    /// A non-bare repo with one commit on `branch` containing `files`,
    /// checked out — `cherry_pick`/`cherry_pick_continue` operate on a real
    /// working tree, unlike every other fixture in this module.
    fn checkout_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> git2::Oid {
        let repo = Repository::init(dir).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
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
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(None).unwrap();
        oid
    }

    /// A commit on top of `parent`, moving `branch` to it — does *not* check
    /// it out, so tests can build a commit meant to be cherry-picked onto a
    /// different, already-checked-out branch.
    fn commit_on_branch(
        repo: &Repository,
        branch: &str,
        parent: git2::Oid,
        files: &[(&str, &str)],
    ) -> git2::Oid {
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
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a change",
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    #[test]
    fn cherry_pick_reports_clean_and_creates_a_commit_when_it_applies_without_conflict() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        // A commit that touches a different file entirely — applies cleanly
        // onto "main", which is still checked out at `base`.
        let to_pick = commit_on_branch(&repo, "topic", base, &[("g.txt", "from topic")]);

        let outcome =
            cherry_pick(dir.path(), to_pick, None).expect("a non-conflicting pick should succeed");
        assert_eq!(outcome, CherryPickOutcome::Clean);

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_id(0).unwrap(), base);
        assert!(dir.path().join("g.txt").exists());
    }

    #[test]
    fn cherry_pick_reports_conflict_and_leaves_real_conflict_markers() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "from topic")]);
        // main itself diverges on the very same file/line, so picking
        // `to_pick` onto it is a real conflict.
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(base).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "diverging change",
            &tree,
            &[&parent],
        )
        .unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        let outcome =
            cherry_pick(dir.path(), to_pick, None).expect("a real conflict is a reported outcome");
        assert_eq!(outcome, CherryPickOutcome::Conflict);

        assert!(
            dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "a real git conflict must leave CHERRY_PICK_HEAD for ordinary git UX to find"
        );
        let contents = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(
            contents.contains("<<<<<<<"),
            "a real conflict must leave real conflict markers in the working tree, not just an in-memory index conflict"
        );
    }

    #[test]
    fn cherry_pick_continue_finishes_once_the_human_resolves_and_adds() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "from topic")]);
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("f.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.find_commit(base).unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "diverging change",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        assert_eq!(
            cherry_pick(dir.path(), to_pick, None).unwrap(),
            CherryPickOutcome::Conflict
        );

        // The human resolves the conflict and stages it.
        std::fs::write(dir.path().join("f.txt"), "resolved").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        let outcome = cherry_pick_continue(dir.path())
            .expect("finishing a fully-resolved cherry-pick should succeed");
        assert_eq!(outcome, CherryPickOutcome::Clean);
        assert!(
            !dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "a completed cherry-pick must not leave CHERRY_PICK_HEAD behind"
        );
        let contents = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(contents, "resolved");
    }

    #[test]
    fn cherry_pick_continue_fails_loudly_when_nothing_is_in_progress() {
        let dir = tempdir().unwrap();
        checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);

        let err = cherry_pick_continue(dir.path())
            .expect_err("continuing with no cherry-pick in progress must not silently succeed");

        assert!(err.to_string().contains("cherry-pick --continue"));
    }
}
