//! Test-only fixtures shared across `commands::sync`'s and
//! `commands::resolve`'s test modules — the ones that were duplicated
//! byte-for-byte, or near enough, between the two.
//!
//! Deliberately NOT here: a fixture that only *looks* similar.
//! `commands::resolve`'s own `add_commit` does a real, forced checkout
//! (plus decisions/0034's control-file restore) after each commit, because
//! its cherry-pick is a real `git` subprocess that refuses to run against a
//! stale working tree; `commands::sync`'s own commit fixtures never need
//! that (its merge/cherry-pick work happens entirely against the object
//! database) but instead refresh the checkout opportunistically via their
//! own `refresh_checked_out_branch`, and one of them writes `.gitprismignore`
//! straight to the working tree regardless of checkout state. The two
//! families solve genuinely different problems, so each stays local to its
//! own test module rather than being forced onto a shared shape here.
//! `commands::setup`'s own commit/config fixtures stay local for the same
//! reason: its "dest" fixture is deliberately non-bare (`setup` only ever
//! fetches from dest, never pushes to it — unlike `sync`/`resolve`, which
//! push and therefore need a bare dest to avoid `receive.denyCurrentBranch`),
//! and its config fixture has no `[source]` section at all, since `setup`
//! never reads one.

use std::io::Write;
use std::path::Path;

use git2::{Oid, Repository, Signature};
use tempfile::NamedTempFile;

use crate::git::{self, PushMode};
use crate::marker::{self, Direction as MarkerDirection};
use crate::policy;

/// A bare repo with one commit on `branch` — dest is always reached over a
/// remote URL in real use (`sync` pushes to it directly; `resolve`'s
/// source-to-dest path pushes a finalized patch through it too), so its
/// fixture is bare here, unlike `setup`'s own (`setup` only ever fetches
/// from dest, never pushes to it).
pub(crate) fn bare_repo_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> Oid {
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
    // `Repository::init_bare` points HEAD at libgit2's environment default
    // branch, which need not be `branch` (e.g. it's "master" in CI).
    // Repoint it so a test's own push/fetch against this repo resolves
    // correctly regardless of that default.
    repo.set_head(&format!("refs/heads/{branch}")).unwrap();
    oid
}

/// `source_url` only actually gets dereferenced (fetched from or pushed to)
/// when a pair has something dest→source needs to push — plenty of tests
/// never reach that path and pass `"unused"`, same convention `setup`'s own
/// tests use for an irrelevant `[dest].url`.
pub(crate) fn write_config(source_url: &str, dest_url: &str, branches: &[&str]) -> NamedTempFile {
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

/// A bare repo seeded at `tip` on `branch`, standing in for source's own
/// real remote — the push target dest→source (and resolve's own
/// finalization step) uses (decisions/0013). The local `source_repo`
/// fixtures elsewhere are non-bare working checkouts, so pushing into them
/// directly would hit git's own `receive.denyCurrentBranch` guard; a
/// separate bare "upstream" avoids that entirely, matching how a real CI
/// checkout's `origin` is a different (bare, hosted) repo from the checkout
/// itself.
pub(crate) fn bare_source_remote_seeded_at(
    source_repo: &Repository,
    branch: &str,
    tip: Oid,
) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    Repository::init_bare(dir.path()).unwrap();
    let outcome = git::push(
        source_repo.workdir().unwrap(),
        &dir.path().display().to_string(),
        tip,
        branch,
        PushMode::FastForwardOnly,
    )
    .unwrap();
    assert_eq!(
        outcome,
        git::PushOutcome::Accepted,
        "seeding the fake source remote at its own graft tip must succeed"
    );
    dir
}

/// Forced checkout of HEAD followed by decisions/0034's byte-exact
/// control-file restore — what a real `setup`/`sync` leaves on disk.
/// Without it, a host with `core.autocrlf=true` (Windows CI) checks out
/// `.gitprismignore` with CRLF and decisions/0037's policy check sees the
/// pinned bytes disagree with an identical blob.
pub(crate) fn checkout_head_exact(repo: &Repository) {
    repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
    policy::restore_control_files_exact(repo, &head_tree).unwrap();
}

/// Sets up a source repo already grafted onto `dest`'s tip — exactly
/// `gitprism setup`'s output shape (decisions/0006) — without depending on
/// `commands::setup` itself, so `sync`'s and `resolve`'s own tests can
/// exercise their command in isolation.
pub(crate) fn source_grafted_onto(
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
        let tree = fetched_tip.tree().unwrap();
        let message = marker::build_message(
            &format!("gitprism setup: graft ({})", source_dir.display()),
            MarkerDirection::Setup,
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
    // decisions/0034: a real `setup` run restores the two control files
    // byte-exact right after this same checkout, so a graft this helper
    // fabricates must too — otherwise a host with `core.autocrlf=true`
    // (Windows CI's default) mangles a checked-out `.gitprismignore`'s line
    // endings, and a later test assertion sees that mangled copy disagree
    // with the identical blob it inherited, for a reason that has nothing to
    // do with an actual content change.
    checkout_head_exact(&repo);
    repo
}
