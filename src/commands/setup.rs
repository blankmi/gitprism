//! `gitprism setup` — see design/decisions/0006-setup-uses-real-shared-history.md,
//! design/decisions/0012-config-versioned-in-source.md, and
//! design/decisions/0011-exclude-list-is-gitignore-syntax.md.
//!
//! For every configured branch pair (decisions/0005), independently: fetch
//! dest's tip for that pair's `dest_branch` (real `git` subprocess, per
//! decisions/0002), then create source's `source_branch` as a brand-new
//! commit — dest's tree plus this same `.gitprism.toml` and
//! `.gitprismignore` — parented directly on dest's tip commit.
//!
//! That graft commit also carries a `Gitprism-Dest-Commit` trailer
//! (decisions/0003) naming dest's tip, so `sync`'s dest→source resume-scan
//! recognizes dest's state as of setup as already reflected into source,
//! without needing a special-cased first run.
//!
//! Like `git` itself, gitprism takes no source-location config: `cwd` is a
//! discovery starting point (walked upward exactly like `git` does from a
//! subdirectory), and setup requires a real, already-`git init`'d — but
//! still completely empty — repo there. It doesn't create one, and it
//! refuses to touch one that already has history (decisions/0012's
//! "Consequences" now says so explicitly). Nothing here decides *how* dest
//! is reached; that stays entirely in `.gitprism.toml`'s `[dest] url`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use git2::{Repository, Signature};

use crate::config::Config;
use crate::exclude::{self, ExcludeList};
use crate::git;

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

    // setup is a one-time graft onto an empty, freshly-initialized repo
    // (decisions/0006, decisions/0012) — never onto one with existing
    // history. Checking both branches and HEAD: a detached HEAD with no
    // branches, or branches that HEAD doesn't currently point at, would
    // each dodge just one of the two checks alone.
    let has_existing_branches = repo
        .branches(Some(git2::BranchType::Local))
        .context("listing source repo's existing branches")?
        .next()
        .is_some();
    if has_existing_branches || repo.head().is_ok() {
        anyhow::bail!(
            "gitprism setup: source repo already has commits and/or branches — setup is a one-time graft onto an empty, freshly-initialized repo, not something to run against existing history"
        );
    }

    // A fresh `git init` already leaves HEAD symbolically pointing at some
    // default branch (commonly "main") while still unborn. If that name
    // collides with a configured pair, rollback below needs to move HEAD
    // off of it before it can delete that branch — captured now so it can
    // be restored to exactly this afterward.
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
    let config_raw = fs::read_to_string(&config_path)
        .with_context(|| format!("reading config at {}", config_path.display()))?;
    let config = Config::parse(&config_raw, &config_path)?;
    if config.pairs.is_empty() {
        anyhow::bail!(
            "gitprism setup: no branch pairs configured in {} — nothing to graft",
            config_path.display()
        );
    }

    // Mirrors `.gitprism.toml`'s own bootstrap handling (decisions/0012): the
    // user prepares both control files locally, uncommitted, before running
    // `setup`. A missing `.gitprismignore` is not an error — it just means
    // nothing is excluded yet.
    let ignore_path = source_root.join(exclude::FILENAME);
    let ignore_raw = match fs::read_to_string(&ignore_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", ignore_path.display()));
        }
    };
    // Fail loudly now rather than committing an exclude-list that can never
    // actually be parsed once sync tries to use it.
    ExcludeList::from_contents(&ignore_raw)
        .with_context(|| format!("parsing {}", ignore_path.display()))?;

    // Fetch every pair's dest tip before writing anything, so a fetch
    // failure partway through never leaves some branches grafted and
    // others not.
    let dest_url = config.dest_url()?;
    let mut dest_tips = Vec::with_capacity(config.pairs.len());
    for pair in &config.pairs {
        git::fetch(&source_root, &dest_url, &pair.dest_branch).with_context(|| {
            format!(
                "fetching dest branch {:?} from {dest_url:?}",
                pair.dest_branch
            )
        })?;
        // FETCH_HEAD gets overwritten by the next fetch, so resolve it to a
        // concrete oid right away rather than re-reading it later.
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();
        dest_tips.push(dest_tip);
    }

    // Commit phase: every precondition above already held, so failure here
    // should be rare — but if one pair still fails partway (e.g. an invalid
    // branch name), roll back this run's already-created branches rather
    // than leaving a half-grafted repo behind.
    let mut created_branches = Vec::with_capacity(config.pairs.len());
    for (pair, dest_tip) in config.pairs.iter().zip(&dest_tips) {
        match graft_pair(&repo, &config, &config_raw, &ignore_raw, pair, *dest_tip) {
            Ok(()) => created_branches.push(pair.source_branch.as_str()),
            Err(err) => {
                rollback_branches(&repo, &created_branches, original_head.as_deref());
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

    // Materialize the graft: point HEAD at the first configured pair's
    // branch and check its tree out into the working directory, same as a
    // fresh `git clone` leaves you on a real, populated checkout rather than
    // an unborn HEAD with content that only exists as unreachable objects.
    // A checkout conflict here is treated the same as a commit-phase
    // failure — roll back every branch this run created rather than leaving
    // grafted branches behind that HEAD never actually landed on.
    if let Some(first) = config.pairs.first()
        && let Err(err) = checkout_branch(&repo, &first.source_branch)
    {
        // The control files were just deleted above to let checkout land
        // them cleanly; a failed checkout must not leave the user without
        // their own bootstrap `.gitprism.toml` — restore them from the exact
        // bytes already read, same as rollback restores the branch refs.
        restore_control_files(&source_root, &config_raw, &ignore_raw);
        rollback_branches(&repo, &created_branches, original_head.as_deref());
        return Err(err);
    }

    Ok(())
}

fn restore_control_files(source_root: &Path, config_raw: &str, ignore_raw: &str) {
    let _ = fs::write(source_root.join(crate::config::FILENAME), config_raw);
    let _ = fs::write(source_root.join(exclude::FILENAME), ignore_raw);
}

/// Deletes `names`' branches, restoring HEAD to `original_head` afterward —
/// used when a later pair fails partway through the commit phase, so a
/// failed run leaves the repo exactly as it found it.
///
/// HEAD has to be moved off of any branch being deleted first: libgit2
/// refuses to delete the branch HEAD symbolically points at even while
/// still unborn, which a fresh `git init`'s default branch name (commonly
/// "main") can easily collide with.
fn rollback_branches(repo: &Repository, names: &[&str], original_head: Option<&str>) {
    let _ = repo.set_head("refs/heads/gitprism-setup-rollback-scratch");
    for name in names {
        if let Ok(mut branch) = repo.find_branch(name, git2::BranchType::Local) {
            let _ = branch.delete();
        }
    }
    if let Some(target) = original_head {
        let _ = repo.set_head(target);
    }
}

fn checkout_branch(repo: &Repository, branch: &str) -> Result<()> {
    let refname = format!("refs/heads/{branch}");
    repo.set_head(&refname)
        .with_context(|| format!("setting HEAD to {refname}"))?;
    // Deliberately not forced: source's repo is required to be empty of
    // history above, but its working directory isn't guarded the same way,
    // so a stray local file that collides with dest's content should surface
    // as a checkout conflict, not get silently overwritten.
    repo.checkout_head(None)
        .with_context(|| format!("checking out {refname} into the working directory"))?;
    Ok(())
}

fn graft_pair(
    repo: &Repository,
    config: &Config,
    config_raw: &str,
    ignore_raw: &str,
    pair: &crate::config::BranchPair,
    dest_tip: git2::Oid,
) -> Result<()> {
    let dest_tip = repo
        .find_commit(dest_tip)
        .context("resolving dest's fetched tip commit")?;

    let mut tree_builder = repo
        .treebuilder(Some(&dest_tip.tree().context("reading dest tip's tree")?))
        .context("seeding tree builder from dest's tree")?;
    let config_blob = repo
        .blob(config_raw.as_bytes())
        .context("writing .gitprism.toml blob")?;
    let ignore_blob = repo
        .blob(ignore_raw.as_bytes())
        .context("writing .gitprismignore blob")?;
    tree_builder
        .insert(
            crate::config::FILENAME,
            config_blob,
            git2::FileMode::Blob.into(),
        )
        .context("inserting .gitprism.toml into the graft tree")?;
    tree_builder
        .insert(exclude::FILENAME, ignore_blob, git2::FileMode::Blob.into())
        .context("inserting .gitprismignore into the graft tree")?;
    let tree_oid = tree_builder.write().context("writing the graft tree")?;
    let tree = repo.find_tree(tree_oid).context("reading the graft tree")?;

    let signature = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = format!(
        "gitprism setup: graft {:?} onto dest {:?}@{}\n\nGitprism-Dest-Commit: {}\n",
        pair.source_branch,
        pair.dest_branch,
        dest_tip.id(),
        dest_tip.id()
    );

    repo.commit(
        Some(&format!("refs/heads/{}", pair.source_branch)),
        &signature,
        &signature,
        &message,
        &tree,
        &[&dest_tip],
    )
    .with_context(|| format!("creating graft commit for {:?}", pair.source_branch))?;

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

    fn write_config(dest_url: &str, pairs: &[(&str, &str)]) -> NamedTempFile {
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

            [dest]
            url = "{dest_url}"

            {pairs_toml}
            "#,
        )
        .unwrap();
        file
    }

    #[test]
    fn run_grafts_every_configured_pair_onto_dests_tip() {
        let dest_dir = tempdir().unwrap();
        let main_tip = repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);
        let release_tip = repo_with_a_commit_on(dest_dir.path(), "release-2.0", &[("b.txt", "b")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        fs::write(source_dir.path().join(exclude::FILENAME), "ignored.txt\n").unwrap();

        let config = write_config(
            &dest_dir.path().display().to_string(),
            &[("main", "main"), ("release-2.0", "release-2.0")],
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

        // The first configured pair's branch — "main" — must be materialized:
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
    fn run_fails_loudly_against_a_non_empty_source_repo() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        // Simulate a prior setup run (or any other pre-existing history) —
        // not necessarily even the same branch name as a configured pair.
        repo_with_a_commit_on(source_dir.path(), "unrelated", &[("existing.txt", "x")]);

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);

        let err = run(source_dir.path(), config.path())
            .expect_err("re-running setup over an existing history must not succeed");

        assert!(err.to_string().contains("already has commits"));
    }

    #[test]
    fn run_resolves_a_relative_default_config_against_source_root_not_cwd() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let config_toml = format!(
            "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"{}\"\n\n[[pairs]]\nsource_branch = \"main\"\ndest_branch = \"main\"\n",
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

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);

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
            "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"{}\"\n\n[[pairs]]\nsource_branch = \"main\"\ndest_branch = \"main\"\n",
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
    fn run_fails_loudly_on_an_empty_pairs_list() {
        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let config_toml = "[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"unused\"\n";
        fs::write(source_dir.path().join(crate::config::FILENAME), config_toml).unwrap();

        let err = run(
            source_dir.path(),
            &source_dir.path().join(crate::config::FILENAME),
        )
        .expect_err("an empty pairs list must not silently succeed");

        assert!(err.to_string().contains("no branch pairs configured"));
        assert_eq!(
            fs::read_to_string(source_dir.path().join(crate::config::FILENAME)).unwrap(),
            config_toml,
            "rejecting an empty pairs list must not touch the user's config file"
        );
    }

    #[test]
    fn run_treats_a_missing_local_gitprismignore_as_empty() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        // No .gitprismignore written here at all.
        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);

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
        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);

        let err = run(not_a_repo.path(), config.path())
            .expect_err("setup must not silently create a repo that was never git-init'd");

        assert!(err.to_string().contains("git repository"));
    }

    #[test]
    fn run_rolls_back_created_branches_when_a_later_pair_fails_to_commit() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);
        repo_with_a_commit_on(dest_dir.path(), "broken", &[("b.txt", "b")]);

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        // "invalid..name" is not a legal git ref name (two consecutive dots)
        // — a real failure mode, not a contrived one — so its commit fails
        // after "main" already succeeded.
        let config = write_config(
            &dest_dir.path().display().to_string(),
            &[("main", "main"), ("invalid..name", "broken")],
        );

        run(source_dir.path(), config.path())
            .expect_err("an invalid branch name for a later pair must fail the whole run");

        let repo = Repository::open(source_dir.path()).unwrap();
        assert!(
            repo.find_branch("main", git2::BranchType::Local).is_err(),
            "the earlier pair's branch must be rolled back, not left behind"
        );
    }
}
