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
//! there. It doesn't create one. That repo must be either completely empty
//! or already a clean, unmodified clone of dest (decisions/0021): every
//! existing local branch's name must be one of `config.branches`, and its
//! tip must be identical to a fresh fetch of dest's own current tip for that
//! name — anything else, including a detached HEAD or a previous `gitprism
//! setup` run's own graft commits, is real independent history and still
//! hard-fails (decisions/0012's "Consequences" originally said so
//! unconditionally; decisions/0021 narrows that to this per-branch check).
//! Nothing here decides *how* dest is reached; that stays entirely in
//! `.gitprism.toml`'s `[dest] url`.

use std::collections::HashMap;
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
    let config_raw = fs::read_to_string(&config_path)
        .with_context(|| format!("reading config at {}", config_path.display()))?;
    // decisions/0021 needs config.branches available before the precondition
    // check below runs (to know which existing local branch names are
    // expected), so parsing config has to move ahead of that check —
    // reordered from where it originally sat in this function.
    let config = Config::parse(&config_raw, &config_path)?;
    if config.branches.is_empty() {
        anyhow::bail!(
            "gitprism setup: no branches configured in {} — nothing to graft",
            config_path.display()
        );
    }

    // setup is a one-time graft onto an empty, freshly-initialized repo
    // (decisions/0006, decisions/0012) — or, per decisions/0021, onto a
    // repo that's already a clean, unmodified clone of dest. Never onto one
    // with real independent history. A detached HEAD is unconditionally
    // unrecognized (a clone always leaves HEAD attached to a branch), and
    // any existing local branch whose name isn't in `config.branches` is
    // unrecognized too — both hard-fail here exactly as the old
    // completely-empty-only check did. A branch that *is* in
    // `config.branches` is allowed to exist for now; its tip is checked
    // against dest's freshly-fetched tip further down, folded into the
    // fetch loop rather than a second pass over the repo.
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
            anyhow::bail!(
                "gitprism setup: source repo already has commits and/or branches — setup is a one-time graft onto an empty, freshly-initialized repo, not something to run against existing history"
            );
        }
        let oid = branch
            .get()
            .peel_to_commit()
            .with_context(|| format!("resolving existing branch {name:?} to a commit"))?
            .id();
        pre_existing_branches.insert(name, oid);
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

    // Fetch every branch's dest tip before writing anything, so a fetch
    // failure partway through never leaves some branches grafted and
    // others not. Also, per decisions/0021, this is where a pre-existing
    // local branch (already confirmed above to be one of config.branches)
    // gets checked against dest's own tip — folded into this same pass
    // rather than a second fetch loop.
    let dest_url = config.dest_url()?;
    let mut branch_plans = Vec::with_capacity(config.branches.len());
    for branch in &config.branches {
        git::fetch(&source_root, &dest_url, branch)
            .with_context(|| format!("fetching dest branch {branch:?} from {dest_url:?}"))?;
        // FETCH_HEAD gets overwritten by the next fetch, so resolve it to a
        // concrete oid right away rather than re-reading it later.
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();
        // `original_oid` is `Some` exactly when this branch already existed
        // locally before this run and matched dest's tip byte-for-byte — the
        // "clean clone of dest" case decisions/0021 recognizes as safe. Any
        // mismatch (a previous `gitprism setup` run's own graft commit
        // included, since that always sits one commit ahead of dest's raw
        // tip) is real independent history and hard-fails here rather than
        // being silently grafted over.
        let original_oid = match pre_existing_branches.get(branch) {
            Some(&existing_oid) if existing_oid == dest_tip => Some(existing_oid),
            Some(_) => anyhow::bail!(
                "gitprism setup: source's local branch {branch:?} already has content that doesn't match dest's current tip — refusing to graft over independent history"
            ),
            None => None,
        };
        branch_plans.push((dest_tip, original_oid));
    }

    // Commit phase: every precondition above already held, so failure here
    // should be rare — but if one branch still fails partway (e.g. an
    // invalid branch name), roll back this run's already-touched branches
    // rather than leaving a half-grafted repo behind.
    let mut touched_branches: Vec<TouchedBranch> = Vec::with_capacity(config.branches.len());
    for (branch, (dest_tip, original_oid)) in config.branches.iter().zip(&branch_plans) {
        match graft_branch(&repo, &config, &config_raw, &ignore_raw, branch, *dest_tip) {
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
        && let Err(err) = checkout_branch(&repo, first)
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

/// One branch this run touched, and what rolling it back means.
///
/// `original_oid: None` — setup created this branch fresh; rollback deletes
/// it. `original_oid: Some(oid)` — this branch already existed before this
/// run and matched dest's tip (decisions/0021's "clean clone of dest" case);
/// rollback resets it back to `oid` instead of deleting it, since deleting a
/// branch the user's own `git clone` produced would be a worse outcome than
/// the failure rollback is guarding against.
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

fn graft_branch(
    repo: &Repository,
    config: &Config,
    config_raw: &str,
    ignore_raw: &str,
    branch: &str,
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
        "gitprism setup: graft {branch:?} onto dest's tip {}\n\nGitprism-Dest-Commit: {}\n",
        dest_tip.id(),
        dest_tip.id()
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
    fn run_fails_loudly_against_a_non_empty_source_repo() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        // Simulate a prior setup run (or any other pre-existing history) —
        // not necessarily even the same branch name as a configured branch.
        repo_with_a_commit_on(source_dir.path(), "unrelated", &[("existing.txt", "x")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

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
    fn run_fails_loudly_when_a_pre_existing_branch_diverges_from_dests_tip() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main", &[("a.txt", "a")]);

        let source_dir = tempdir().unwrap();
        // "main" already exists locally, and is a configured branch name —
        // but its tip is real, independent history, not dest's tip and not
        // an empty placeholder either.
        repo_with_a_commit_on(source_dir.path(), "main", &[("independent.txt", "x")]);

        let config = write_config(&dest_dir.path().display().to_string(), &["main"]);

        let err = run(source_dir.path(), config.path()).expect_err(
            "a pre-existing branch that diverges from dest's tip must not be grafted over",
        );

        let message = err.to_string();
        assert!(message.contains("\"main\""), "message was: {message}");
        assert!(
            message.contains("doesn't match dest's current tip"),
            "message was: {message}"
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
