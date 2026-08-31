//! Shared fixtures for `sync`'s own test submodules — helpers used by more
//! than one of them. Each submodule below groups tests by theme; a fixture
//! used by only one theme still lives here rather than being duplicated,
//! since `use super::*;` already reaches it from every submodule for free.
//!
//! `bare_repo_with_a_commit_on`, `write_config`, `bare_source_remote_seeded_at`,
//! and `source_grafted_onto` are NOT here — they moved to `crate::testutil`,
//! shared with `commands::resolve`'s own test module too.

use std::fs;
use std::io::Write;

use tempfile::{NamedTempFile, tempdir};

use super::anchor::*;
use super::local_advance::git_paths_conflict;
use super::policy_check::*;
use super::*;

use crate::testutil::{
    bare_repo_with_a_commit_on, bare_source_remote_seeded_at, source_grafted_onto, write_config,
};

mod anchor;
mod dest_to_source;
mod filter;
// Named `limit_tests`, not `limits`, so it doesn't shadow `crate::limits`
// (glob-forwarded into this module and every submodule via `use super::*;`)
// for every submodule that references `limits::MAX_*` unqualified.
mod limit_tests;
mod local_advance;
mod policy_check;
mod run_entrypoint;
mod scheduling;
mod source_to_dest;

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

fn add_dest_marker_commit(repo: &Repository, branch: &str, parent: Oid, counterpart: Oid) -> Oid {
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

/// Pushes `branch`'s pending commits from source to a same-named branch on
/// dest, filtered, one branch at a time — `branch` is discovered on source at
/// run time by [`run`], not read from config (decisions/0017). Recomputes
/// from scratch (refetch, rebuild, retry) on a lost fast-forward race rather
/// than rebasing what it already built (decisions/0009). A test-only
/// convenience over `sync_pair_to_dest_with_key` — every real caller (`run`)
/// already has the state key, the current exclude-list, and the raw
/// `.gitprismignore` bytes in hand, but deriving them here saves every test
/// below from doing so itself.
fn sync_pair_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    branch: &str,
    reporter: &Reporter,
    run_cache: &mut RunCache,
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
    // decisions/0046: `run` builds the mapping index exactly once, from the
    // current repo/dest state, before any branch is processed — the one
    // init site F-B moved the old lazy-rebuild-on-`None` into. This test-only
    // wrapper calls a single branch at a time rather than scheduling a whole
    // run, so it stands in for that init site itself, rebuilding fresh from
    // the current state on every call. A test that needs the "built once,
    // carried across several branches in one run" invariant under test calls
    // `run` directly instead (see `tests::anchor`'s scheduling tests).
    //
    // A reconstruction failure (e.g. a deliberately shallow/incomplete test
    // clone whose own source history can't be walked past its fetch
    // boundary) is swallowed here rather than propagated: this branch's own
    // sync may never actually consult the index at all — the same as before
    // this wrapper started seeding it explicitly — and a real `run` still
    // propagates its own single reconstruction's errors untouched.
    let dest_url = config.dest_url()?;
    let (source_branches, _skipped) = list_source_branches(repo)?;
    if let Ok(mapping_index) = reconstruct_mapping_index(
        repo,
        source_root,
        &dest_url,
        &source_branches,
        &key,
        run_cache,
    ) {
        run_cache.mapping_index = mapping_index;
    }
    sync_pair_to_dest_with_key(
        repo,
        source_root,
        config,
        branch,
        reporter,
        &key,
        &exclude_list,
        &ignore_raw,
        run_cache,
    )
}

/// [`sync_pair_from_dest_with_key`], deriving the state key — a test-only
/// convenience the same way [`sync_pair_to_dest`] is.
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
    let mut command = std::process::Command::new("git");
    crate::git::isolate_test_git_command(&mut command);
    let mut child = command
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

    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("first sync should mirror feature-x to dest");

    (dest_dir, dest_repo, source_dir, repo, graft, config)
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
    let source_repo = source_grafted_onto(source_dir.path(), "main", original_dest_tip, &dest_repo);
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
