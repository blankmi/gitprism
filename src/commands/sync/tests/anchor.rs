use super::*;

#[test]
fn dest_resume_point_resumes_from_the_newest_gitprism_commit_in_dests_history() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let x1 = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);

    // Stands in for gitprism's own source→dest push having landed on
    // dest.
    let gitprism_push = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("notes.txt", "line1\n"),
        &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {x1}\n"),
    );

    // A second, genuinely independent dest commit landing after it.
    let d = add_independent_dest_commit(
        &dest_repo,
        gitprism_push,
        ("dest-only.txt", "from a merged PR\n"),
        "an independent, unrelated dest-side change",
    );

    // `dest_resume_point` is always called against a freshly fetched
    // dest tip in real use (`sync_pair_to_dest` fetches right before
    // calling it) — do the same here so `d` actually exists in this
    // repo's odb.
    git::fetch(
        source_dir.path(),
        &dest_repo.path().to_string_lossy(),
        "main",
    )
    .unwrap();

    // A marker commit on source naming `d` — same tree as its parent,
    // dest→source's own commit shape (copied from
    // `sync_pair_to_dest_hard_stops_on_a_real_conflict`).
    add_dest_marker_commit(&source_repo, "main", x1, d);

    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    assert_eq!(
        dest_resume_point(&source_repo, source_tip, d).unwrap(),
        Some(x1)
    );
}

#[cfg(unix)]
#[test]
fn mirror_only_rewrite_detected_propagates_a_real_lookup_failure_instead_of_guessing() {
    use std::os::unix::fs::PermissionsExt;

    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // A real commit, present in the odb — not a missing one, decisions/0039's
    // addendum case — so `find_commit` must fail for an unrelated reason:
    // its own loose object file is made unreadable below.
    let tree = source_repo.find_commit(source_tip).unwrap().tree().unwrap();
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let boundary = source_repo
        .commit(None, &signature, &signature, "boundary", &tree, &[])
        .unwrap();
    drop(tree);

    let hex = boundary.to_string();
    let object_path = source_dir
        .path()
        .join(".git/objects")
        .join(&hex[..2])
        .join(&hex[2..]);
    assert!(
        object_path.exists(),
        "the boundary commit must be a real loose object"
    );
    fs::set_permissions(&object_path, fs::Permissions::from_mode(0o000)).unwrap();

    let dest_marker_tip = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("feature.txt", "v1\n"),
        &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {boundary}\n"),
    );
    git::fetch(
        source_dir.path(),
        &dest_repo.path().to_string_lossy(),
        "main",
    )
    .unwrap();
    let fetched_dest_tip = source_repo
        .find_reference("FETCH_HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(fetched_dest_tip, dest_marker_tip);

    let key = marker::load_key().unwrap();
    let error =
        mirror_only_rewrite_detected(&source_repo, source_tip, fetched_dest_tip, "main", &key)
            .expect_err(
                "a real lookup failure on the boundary object must propagate as Err, \
                 not be guessed as Ok(true)/Ok(false)",
            );
    assert!(
        error.to_string().contains(&boundary.to_string()),
        "unexpected error: {error}"
    );

    fs::set_permissions(&object_path, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_a_rebase() {
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "original\n")]);

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
    let original_mirror_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // A rebase-shaped rewrite: feature-x is reset back to the graft and
    // given a brand-new commit off it — the same parent the original
    // commit had, but not a descendant of it.
    source_repo
        .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
        .unwrap();
    add_commit(&source_repo, "feature-x", &[("feature.txt", "rebased\n")]);

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        rebuilt.id(),
        original_mirror_tip,
        "the pre-rewrite mirror history must be replaced, not built upon"
    );
    let tree = rebuilt.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"rebased\n");
}

#[test]
fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_rewritten_by_an_amend() {
    let (_dest_dir, dest_repo, source_dir, repo, graft, config) =
        mirror_only_feature_branch_synced_once();
    let reporter = Reporter::new(1, std::iter::empty());
    let original_mirror_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // An amend-shaped rewrite: the branch's only commit is replaced by a
    // new commit object with the same parent — exactly what `git commit
    // --amend` produces at the plumbing level.
    repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
        .unwrap();
    add_commit_with_message(
        &repo,
        "feature-x",
        &[("feature.txt", "amended\n")],
        "add feature (amended)",
    );

    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        rebuilt.id(),
        original_mirror_tip,
        "the pre-amend mirror history must be replaced, not built upon"
    );
    let tree = rebuilt.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"amended\n");
}

#[test]
// decisions/0039's addendum-to-the-addendum (2026-08-27, review finding
// F-01): a missing boundary object is never positive rewrite evidence,
// regardless of `Repository::is_shallow()` — a stale (behind) but
// non-shallow clone looks exactly like a real rewrite from inside
// `mirror_only_rewrite_detected`, and the old special case force-pushed
// dest backwards from it (`stale_nonshallow_clone_...` below reproduces
// the end-to-end race). The amend test above rewrites `source_repo` in
// place, so the pre-amend commit never actually leaves its object
// database and `find_commit(boundary)` trivially succeeds — it never
// exercises the missing-object path a real, freshly fetched CI clone
// hits. This test runs the sync against a genuinely separate clone
// instead.
fn sync_pair_to_dest_refuses_a_mirror_only_branch_when_the_boundary_object_is_missing_even_from_a_non_shallow_clone()
 {
    let (dest_dir, dest_repo, _source_dir, repo, graft, config) =
        mirror_only_feature_branch_synced_once();
    let reporter = Reporter::new(1, std::iter::empty());

    // Amend, in place, exactly like the test above — but this time the
    // sync that follows runs against a *separate* clone that fetched
    // `feature-x` only after the amend, so it never had the pre-amend
    // tip the dest marker names.
    repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
        .unwrap();
    add_commit_with_message(
        &repo,
        "feature-x",
        &[("feature.txt", "amended\n")],
        "add feature (amended)",
    );

    let (fresh_dir, fresh_repo) = fresh_clone_of_branch(&repo, "feature-x", false);
    assert!(
        !fresh_repo.is_shallow(),
        "a plain fetch must not produce a shallow clone"
    );

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &fresh_repo,
        fresh_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect(
        "a missing boundary object must halt this branch, not be guessed at as a \
         confirmed rewrite just because this clone happens to be non-shallow",
    );
    assert!(
        halted,
        "a mirror-only branch that can't be safely built on must halt, not silently push"
    );

    let untouched = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = untouched.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        blob.content(),
        b"original\n",
        "dest must be left exactly as the first sync produced it — a stale clone must \
         never force-push dest backwards just because it isn't shallow"
    );
    drop(dest_dir);
}

#[test]
// The shallow counterpart of the test above: the ambiguity ("rewritten,
// or just an incomplete clone?") is real on a shallow clone too — same
// halt, same reasoning, decisions/0039's addendum-to-the-addendum no
// longer distinguishes them by shallowness.
fn sync_pair_to_dest_still_refuses_a_mirror_only_branch_when_the_boundary_object_is_missing_from_a_shallow_clone()
 {
    let (dest_dir, dest_repo, _source_dir, repo, graft, config) =
        mirror_only_feature_branch_synced_once();
    let reporter = Reporter::new(1, std::iter::empty());

    repo.branch("feature-x", &repo.find_commit(graft).unwrap(), true)
        .unwrap();
    add_commit_with_message(
        &repo,
        "feature-x",
        &[("feature.txt", "amended\n")],
        "add feature (amended)",
    );

    let (fresh_dir, fresh_repo) = fresh_clone_of_branch(&repo, "feature-x", true);
    assert!(
        fresh_repo.is_shallow(),
        "a --depth=1 fetch must produce a shallow clone"
    );

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &fresh_repo,
        fresh_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a shallow clone can't tell a rewrite from an incomplete fetch — must halt, not error the whole invocation");
    assert!(
        halted,
        "a mirror-only branch that can't be safely built on must halt, not silently push"
    );

    let untouched = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = untouched.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        blob.content(),
        b"original\n",
        "dest must be left exactly as the first sync produced it"
    );
    drop(dest_dir);
}

#[test]
fn sync_pair_to_dest_refuses_to_force_push_dest_backwards_from_a_stale_non_shallow_clone() {
    // decisions/0039's addendum-to-the-addendum (review finding F-01,
    // reproduced end-to-end): two CI clones for the same mirror-only
    // branch finishing out of order, both non-shallow. Clone A syncs at
    // T1, then advances the branch to T2 (an ordinary fast-forward, no
    // rewrite) and syncs again. A stale clone B — a full, non-shallow
    // clone taken back when the branch was still at T1 — must never
    // force-push dest's tip back down to a chain built from T1: a
    // missing boundary object on a non-shallow clone looks exactly like
    // a genuine rewrite from inside `mirror_only_rewrite_detected`, and
    // treating it as positive rewrite evidence let a stale clone's
    // passing `--force-with-lease` (the lease only guards against
    // *concurrent* dest movement, not stale source knowledge) rebuild
    // and force dest backwards.
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
        .branch("task", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "task", &[("t1.txt", "1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // Clone A syncs task at T1.
    sync_pair_to_dest(
        &source_repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut RunCache::default(),
    )
    .unwrap();
    let dest_tip_at_t1 = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Clone B: a full (non-shallow) clone of source taken now, at T1.
    let (b_dir, b_repo) = fresh_clone_of_branch(&source_repo, "task", false);
    assert!(
        !b_repo.is_shallow(),
        "a plain fetch must not produce a shallow clone"
    );

    // Clone A advances task to T2 (fast-forward, no rewrite) and syncs.
    add_commit(&source_repo, "task", &[("t2.txt", "2\n")]);
    sync_pair_to_dest(
        &source_repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut RunCache::default(),
    )
    .unwrap();
    let dest_tip_at_t2 = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_ne!(dest_tip_at_t1, dest_tip_at_t2);

    // Stale clone B — still at T1, never rewrote anything — syncs last.
    let halted = sync_pair_to_dest(
        &b_repo,
        b_dir.path(),
        &config,
        "task",
        &reporter,
        &mut RunCache::default(),
    )
    .expect("a stale clone must halt this branch, not error the whole invocation");
    assert!(halted, "a stale clone must halt, never force-push");

    let dest_tip_after_stale_clone = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_tip_after_stale_clone, dest_tip_at_t2,
        "a stale clone must never move dest's task ref backwards"
    );
}

#[test]
fn sync_pair_to_dest_rebuilds_a_mirror_only_branch_reset_to_an_earlier_commit_plus_a_new_commit() {
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
    let earlier = add_commit(&source_repo, "feature-x", &[("feature.txt", "a\n")]);
    add_commit(
        &source_repo,
        "feature-x",
        &[("feature.txt", "a\n"), ("extra.txt", "b\n")],
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
    .expect("first sync should mirror both commits to dest");
    let original_mirror_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // A hard reset to an earlier, already-synced commit, plus a genuinely
    // new commit off it — dest was last synced through the second
    // commit, but source's tip no longer descends from that.
    source_repo
        .branch(
            "feature-x",
            &source_repo.find_commit(earlier).unwrap(),
            true,
        )
        .unwrap();
    add_commit(
        &source_repo,
        "feature-x",
        &[("feature.txt", "a\n"), ("different.txt", "c\n")],
    );

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewritten mirror-only branch must rebuild its projection, not refuse");

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        rebuilt.id(),
        original_mirror_tip,
        "the pre-reset mirror history must be replaced, not built upon"
    );
    let tree = rebuilt.tree().unwrap();
    assert!(tree.get_name("different.txt").is_some());
    assert!(
        tree.get_name("extra.txt").is_none(),
        "content only reachable through the discarded branch state must not survive"
    );
}

#[test]
fn sync_pair_to_dest_rewinds_a_mirror_only_branch_reset_all_the_way_back_to_the_shared_graft() {
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "one\n")]);
    add_commit(&source_repo, "feature-x", &[("feature.txt", "two\n")]);

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
    .expect("first sync should mirror both commits to dest");
    assert!(
        dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some()
    );

    // The commonest rewrite shape: `git reset --hard` straight back to a
    // commit that already carries a Gitprism-Dest-Commit trailer (here,
    // the shared graft itself), with no new commit of its own. Source's
    // own graft-derived rebuild boundary now equals source's own tip, so
    // `pending_commits` finds nothing to build — the bug this test
    // guards against is `build_pending_dest_tip` reporting `new_tip:
    // None` and the caller then pushing nothing at all, leaving dest
    // silently holding the discarded history forever.
    source_repo
        .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
        .unwrap();

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewrite that rebuilds to the shared base must still be pushed");

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        rebuilt.id(),
        dest_tip,
        "dest must be rewound to the graft-derived rebuild base even though no commit was constructed"
    );
    assert!(
        rebuilt.tree().unwrap().get_name("feature.txt").is_none(),
        "the discarded commits' content must not survive on dest"
    );
}

#[test]
fn sync_pair_to_dest_rewinds_a_mirror_only_branch_when_the_rewrite_filters_to_no_changes() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(
        dest_dir.path(),
        "main",
        &[("shared.txt", "v1"), (exclude::FILENAME, "secret.txt\n")],
    );

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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "original\n")]);

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
    .expect("first sync should mirror feature.txt to dest");
    assert!(
        dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some()
    );

    // Reset back to the shared graft and replace the discarded commit
    // with one that only touches an already-excluded path. `pending` is
    // non-empty this time, but the one pending commit filters to no
    // change against the rebuild base (requirements/0001's "must not
    // push an empty commit"), so `build_pending_dest_tip` still
    // constructs no commit — a second, distinct way to reach `new_tip:
    // None` from the first test's.
    source_repo
        .branch("feature-x", &source_repo.find_commit(graft).unwrap(), true)
        .unwrap();
    add_commit(&source_repo, "feature-x", &[("secret.txt", "ignored\n")]);

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewrite whose replacement commits all filter to no changes must still rewind dest");

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        rebuilt.id(),
        dest_tip,
        "dest must be rewound to the graft-derived rebuild base even though no commit was constructed"
    );
    assert!(rebuilt.tree().unwrap().get_name("feature.txt").is_none());
    assert!(
        rebuilt.tree().unwrap().get_name("secret.txt").is_none(),
        "an excluded path must never reach dest"
    );
}

#[test]
fn sync_pair_to_dest_pushes_nothing_for_a_round_tripped_branch_already_up_to_date() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let _source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // A round-tripped branch, no rewrite involved at all — this is
    // `FastForwardOnly`'s own `(!dest_ref_exists).then_some(dest_tip)`
    // fallback, which must stay byte-identical: `dest_ref_exists` is
    // `true` here, so nothing pending must mean nothing pushed.
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a fresh graft with no source-side commits of its own has nothing to sync");

    let still = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still.id(),
        dest_tip,
        "a round-tripped branch with nothing pending must not have its dest ref touched"
    );
}

#[test]
fn sync_pair_to_dest_incorporates_a_benign_race_on_a_mirror_only_branch_via_recompute_not_force() {
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "s1\n")]);

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
    let m1 = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let s2 = add_commit(&source_repo, "feature-x", &[("feature.txt", "s2\n")]);

    // Stands in for another clone legitimately completing this exact
    // sync step first: the SourceToDest marker it writes is gitprism's
    // own shape, naming s2 exactly, so this clone's own recompute must
    // recognize and build on it rather than treating it as unaccounted
    // for — and, crucially, without needing to detect (or fire) a
    // rewrite to do so, since source_tip still equals this exact marker.
    let key = marker::load_key().unwrap();
    let exclude_list = ExcludeList::from_contents("").unwrap();
    let s2_commit = repo.find_commit(s2).unwrap();
    let filtered_tree = filter_tree(
        &repo,
        &s2_commit.tree().unwrap(),
        Path::new(""),
        &exclude_list,
    )
    .unwrap();
    let m2 = build_dest_commit(
        &repo,
        &config,
        m1,
        &s2_commit,
        filtered_tree,
        "feature-x",
        &key,
    )
    .unwrap();
    let outcome = git::push(
        source_dir.path(),
        &dest_dir.path().display().to_string(),
        m2,
        "feature-x",
        PushMode::FastForwardOnly,
    )
    .unwrap();
    assert_eq!(outcome, git::PushOutcome::Accepted);

    add_commit(&source_repo, "feature-x", &[("feature.txt", "s3\n")]);

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect(
        "a benign, gitprism-shaped dest advance must be incorporated, not refused or forced over",
    );

    let m3 = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        m3.parent_id(0).unwrap(),
        m2,
        "the concurrently-added m2 must survive as m3's parent — a force rebuild would have \
         replaced it with a fresh chain off the graft instead"
    );
    let tree = m3.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"s3\n");
}

#[test]
fn sync_pair_to_dest_recovers_from_a_stale_no_dest_ref_cache_entry_on_a_race_retry() {
    // decisions/0043 step 2's dest-ref cache is a per-run performance
    // optimization, not license to skip re-detecting a real race: if
    // the cache already (wrongly) believes `feature-x` has no dest ref
    // yet — stands in for an earlier branch's own anchor search having
    // looked it up this run, before another writer concurrently
    // completed feature-x's very first mirror — the ordinary
    // `RejectedRefMoved` retry path must still discover and build on
    // that real dest ref, not keep retrying the stale "doesn't exist"
    // assumption until retries are exhausted.
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
    let s1 = add_commit(&source_repo, "feature-x", &[("feature.txt", "s1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // Another writer completes feature-x's very first mirror
    // concurrently, landing before this run's own retry ever queries
    // dest for real.
    let key = marker::load_key().unwrap();
    let exclude_list = ExcludeList::from_contents("").unwrap();
    let s1_commit = repo.find_commit(s1).unwrap();
    let filtered_tree = filter_tree(
        &repo,
        &s1_commit.tree().unwrap(),
        Path::new(""),
        &exclude_list,
    )
    .unwrap();
    let concurrent_dest_commit = build_dest_commit(
        &repo,
        &config,
        graft,
        &s1_commit,
        filtered_tree,
        "feature-x",
        &key,
    )
    .unwrap();
    let outcome = git::push(
        source_dir.path(),
        &dest_dir.path().display().to_string(),
        concurrent_dest_commit,
        "feature-x",
        PushMode::FastForwardOnly,
    )
    .unwrap();
    assert_eq!(outcome, git::PushOutcome::Accepted);

    // Stale on purpose: stands in for a lookup this run already made
    // for feature-x before the concurrent push above landed.
    let mut dest_ref_cache = RunCache::default();
    dest_ref_cache
        .dest_ref_exists
        .insert("feature-x".to_string(), false);

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect(
        "a race-retry must recover from a stale \"no dest ref\" cache entry, not exhaust \
         retries against it",
    );

    let dest_tip_after = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_tip_after.id(),
        concurrent_dest_commit,
        "the concurrently-created dest ref must be recognized as already up to date, not \
         silently replaced or fought over after the stale cache entry is corrected"
    );
}

#[test]
fn sync_pair_to_dest_discards_a_mirror_only_branchs_content_naming_an_unrelated_source_commit() {
    let (_dest_dir, dest_repo, source_dir, their_mirror, config) =
        authority_invariant_fixture(false);
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
    .expect(
        "a mirror-only branch's unrecognized dest content must be discarded and rebuilt \
         — decisions/0039's authority invariant, licensed by config.branches absence alone",
    );

    let rebuilt = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        rebuilt.id(),
        their_mirror,
        "the sibling clone's unrecognized mirror must be replaced"
    );
    let tree = rebuilt.tree().unwrap();
    assert!(
        tree.get_name("ours.txt").is_some(),
        "the rebuild must reflect this clone's own source content"
    );
    assert!(
        tree.get_name("theirs.txt").is_none(),
        "the sibling clone's discarded content must not survive the rebuild"
    );
}

#[test]
fn sync_pair_to_dest_stops_a_round_tripped_branchs_content_naming_an_unrelated_source_commit_instead_of_discarding_it()
 {
    // Identical dest-side state to the mirror-only test above — only
    // `feature-x`'s presence in `config.branches` differs — proving the
    // authority invariant's discard is licensed by that membership
    // alone, not by anything else about the dest-side content.
    let (_dest_dir, dest_repo, source_dir, their_mirror, config) =
        authority_invariant_fixture(true);
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let err = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-x",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect_err(
        "a round-tripped branch must stop instead of discarding dest's unrecognized content",
    );
    let message = format!("{err:#}");
    assert!(
        !message.to_lowercase().contains("force"),
        "a round-tripped branch's refusal must never mention forcing"
    );

    let still = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still.id(),
        their_mirror,
        "a stopped sync must not touch dest's branch at all"
    );
}

#[test]
fn run_halts_only_a_dest_native_branch_colliding_with_a_same_named_discovered_branch() {
    // decisions/0045 (review finding F-05): dest developers create a
    // branch that happens to share a name with a branch source
    // independently created too — genuinely unrelated history, dest-ref
    // already exists on both sides. Before this decision,
    // `graft_point`'s merge-base failure propagated as a fatal
    // `anyhow::bail!` from inside `dest_tip_accounted_for`, aborting the
    // whole run before any later-sorted branch (alphabetically after
    // "hotfix") ever got its own turn.
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

    // A genuinely unrelated, orphan dest-native branch "hotfix" — no
    // ancestry shared with anything `gitprism setup` ever grafted.
    let hotfix_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    let blob = dest_repo.blob(b"hotfix content").unwrap();
    let mut builder = dest_repo.treebuilder(None).unwrap();
    builder
        .insert("hotfix.txt", blob, git2::FileMode::Blob.into())
        .unwrap();
    let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
    dest_repo
        .commit(
            Some("refs/heads/hotfix"),
            &hotfix_signature,
            &hotfix_signature,
            "an orphan dest-native hotfix, unrelated to anything gitprism ever grafted",
            &tree,
            &[],
        )
        .unwrap();

    // Source independently has its own, entirely unrelated "hotfix" —
    // sorts before "main" alphabetically, so a fatal bail here would
    // have starved main's own sync too.
    source_repo
        .branch("hotfix", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(
        &source_repo,
        "hotfix",
        &[("source-hotfix.txt", "content\n")],
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path())
        .expect_err("a halted branch must still fail the overall run (decisions/0037)");
    assert!(
        format!("{err:#}").to_lowercase().contains("halted"),
        "the run's own error should mention a halted branch: {err:#}"
    );

    // "main" sorts after "hotfix" — it must still have been processed
    // in the same run, not starved by hotfix's halt.
    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_main_tip_after, dest_tip,
        "main must still be processed (a no-op here) when hotfix halts"
    );

    // dest's own hotfix content must be untouched — nothing pushed for
    // the halted branch.
    let dest_hotfix_tip_after = dest_repo
        .find_branch("hotfix", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let hotfix_tree = dest_hotfix_tip_after.tree().unwrap();
    assert!(
        hotfix_tree.get_name("hotfix.txt").is_some(),
        "dest's own hotfix content must be untouched"
    );
    assert!(
        hotfix_tree.get_name("source-hotfix.txt").is_none(),
        "source's unrelated hotfix content must never have been pushed"
    );
}

#[test]
fn ambiguous_anchor_message_names_every_candidate_and_its_merge_base() {
    let oid_a = Oid::from_str("4eb55376359199a3a77cd9f2e3aad225b78ea671").unwrap();
    let oid_b = Oid::from_str("7beb3580803dc21f883872ca2d9e010ff0078638").unwrap();
    let message = ambiguous_anchor_message(
        "task",
        &[
            ("feature-a".to_string(), oid_a),
            ("feature-b".to_string(), oid_b),
        ],
    );
    assert!(
        message.contains("\"task\""),
        "must name the branch: {message}"
    );
    assert!(
        message.contains("\"feature-a\"") && message.contains(&oid_a.to_string()),
        "must name feature-a and its merge-base: {message}"
    );
    assert!(
        message.contains("\"feature-b\"") && message.contains(&oid_b.to_string()),
        "must name feature-b and its merge-base: {message}"
    );
}

#[test]
fn sync_pair_to_dest_anchors_a_task_branch_on_its_mirror_only_parent_feature_branch() {
    // task branched from mirror-only feature branched from round-tripped
    // main: task's dest chain must anchor on feature's own dest tip, not
    // on main's original graft — decisions/0043's central case.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
    let feature_tip = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

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
        "feature",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("feature must mirror to dest first");
    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("task must mirror to dest, anchored on feature's own dest tip");
    let dest_task_tip = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .expect("task must be mirrored to dest")
        .get()
        .peel_to_commit()
        .unwrap();

    // Real shared ancestry: task's dest commit is built directly onto
    // feature's own dest tip, not re-flattened onto main's graft.
    assert_eq!(
        dest_task_tip.parent_id(0).unwrap(),
        dest_feature_tip.id(),
        "task's dest commit must be built directly onto feature's own dest tip, \
         not re-mirrored from main's graft"
    );

    // A PR-shaped diff: exactly one new commit beyond feature's own
    // tip — task's own task.txt commit, not a second copy of feature's
    // history.
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task_tip.id()).unwrap();
    walk.hide(dest_feature_tip.id()).unwrap();
    let commits_since_feature: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_feature.len(),
        1,
        "task's dest chain must add exactly one commit onto feature's own dest tip"
    );

    let task_tree = dest_task_tip.tree().unwrap();
    assert!(
        task_tree.get_name("feature.txt").is_some(),
        "task's dest tree must still carry feature.txt, inherited from feature's own tip"
    );
    assert!(task_tree.get_name("task.txt").is_some());
}

#[test]
fn run_task_forked_from_main_after_a_dest_native_import_does_not_duplicate_it() {
    // Repository review F-04: a dest-native commit X, committed directly
    // to dest's round-tripped main, gets imported into source's main as
    // a `DestToSource` marker M (dest→source). `task`, forked from
    // main's tip *after* that import, inherits M as an ancestor. M is
    // scoped to "main", not "task" — `task`'s own resume-boundary scan
    // and decisions/0043's sibling search both correctly don't accept it
    // as *task's own* marker, but before the fix, loop prevention didn't
    // accept it either, so `task`'s sync re-replayed M's filtered
    // content as a brand-new commit — X duplicated onto dest's task.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("s1.txt", "s1\n")]);
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
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).unwrap();

    // A dest-native commit X, landing directly on dest main.
    let dest_main_before_x = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    add_independent_dest_commit(
        &dest_repo,
        dest_main_before_x,
        ("x.txt", "x\n"),
        "dest native X",
    );
    run(source_dir.path(), config.path()).unwrap(); // imports X into source main as M
    let dest_main_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let main_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    source_repo.branch("task", &main_tip, false).unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "t\n")]);

    run(source_dir.path(), config.path()).unwrap();

    let dest_task = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task.id()).unwrap();
    walk.hide(dest_main_tip).unwrap();
    let commits_since_main: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_main.len(),
        1,
        "task's dest chain must add only its own commit onto dest main — M must not be \
         re-replayed as a duplicate of the already-imported X"
    );
}

#[test]
fn build_pending_dest_tip_loop_prevents_a_dest_to_source_marker_scoped_to_another_branch() {
    // decisions/0043's addendum (F-04), isolating loop prevention from
    // the anchor-search fix above: "main" is deliberately kept out of
    // `task`'s sibling candidates (decisions/0043's own accepted
    // "wrong order" ordering hazard, forced here via the cache instead
    // of real timing), so `task`'s first sync falls all the way back to
    // the original graft — M ends up inside `pending` regardless of how
    // precise the anchor search is. Only loop prevention recognizing
    // M's own `DestToSource` marker (scoped to "main", not "task") can
    // stop it from being replayed as a duplicate of the already-
    // imported X.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("s1.txt", "s1\n")]);
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

    let config = Config::load(
        write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main"],
        )
        .path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut run_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut run_cache,
    )
    .expect("main must mirror s1.txt to dest first");

    let dest_main_before_x = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    add_independent_dest_commit(
        &dest_repo,
        dest_main_before_x,
        ("x.txt", "x\n"),
        "dest native X",
    );
    sync_pair_from_dest(&repo, source_dir.path(), &config, "main", &reporter)
        .expect("main must import X into source as M");

    let main_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    source_repo.branch("task", &main_tip, false).unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "t\n")]);

    // Forces the baseline fallback for `task`'s first sync: "main" is
    // excluded from the sibling search's candidates even though its
    // dest ref really does exist (decisions/0043 step 2's cache, seeded
    // by hand rather than by real cross-run timing).
    let mut task_run_cache = RunCache::default();
    task_run_cache
        .dest_ref_exists
        .insert("main".to_string(), false);
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut task_run_cache,
    )
    .expect("task must mirror, falling back to the graft since main is hidden from the search");

    let dest_task = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task.id()).unwrap();
    walk.hide(dest_tip).unwrap();
    let commits_since_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_graft.len(),
        2,
        "task's flattened chain must replay s1.txt and its own commit only — M must be loop-\
         prevented even though it's scoped to \"main\", not \"task\""
    );
}

#[test]
fn dest_anchor_for_branch_is_stateless_and_finds_a_sibling_mirrored_since_its_last_call() {
    // The same task/feature/main topology, processed in the "wrong"
    // order: task's own anchor search runs before feature has a dest
    // ref, and must fall back to the coarser baseline (main's graft).
    // The identical search, called again once feature has since been
    // mirrored, finds feature as the more specific anchor.
    //
    // This proves `dest_anchor_for_branch` itself is stateless and
    // order-independent — not that an ordinary resync of an
    // already-mirrored `task` re-invokes it. Decisions/0043 is explicit
    // that it does not: once `task` has any dest ref, only a positively
    // detected rewrite of `task`'s own source history (decisions/0039)
    // ever calls this search again. This test exercises the primitive
    // directly for exactly that reason — it is what a rewrite-triggered
    // rebuild relies on to actually pick up the improved anchor once
    // re-invoked, not a demonstration of automatic self-correction on
    // an unrelated branch's ordinary resync.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
    let feature_tip = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);
    let task_tip = source_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let dest_url = dest_dir.path().display().to_string();
    let key = marker::load_key().unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();

    // feature has no dest ref yet — task's search falls back to the
    // baseline (main's graft) exactly. A fresh, empty cache each call
    // below — the cache is purely a dest-ref-lookup accuracy
    // optimization (decisions/0043 step 2), not the "memory between
    // calls" this test disproves, so every call starts knowing nothing
    // and must still arrive at the right answer from real repo state.
    let mut anchor_cache_1 = RunCache::default();
    let anchor = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache_1,
    )
    .unwrap();
    assert_eq!(
        anchor,
        DestAnchor::Resolved(graft, dest_tip),
        "with feature not yet mirrored, task's anchor must fall back to the baseline"
    );

    // feature is mirrored now — a clean, independent sync with no other
    // sibling holding a dest ref yet to interact with.
    let config = Config::load(write_config("unused", &dest_url, &["main"]).path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut sync_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut sync_cache,
    )
    .expect("feature must mirror to dest");
    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // The identical search, called again for task with another fresh,
    // empty cache, now finds feature as the more specific anchor —
    // proving the search itself is stateless, not that anything
    // re-invokes it automatically for an already-mirrored task (it does
    // not; see decisions/0043).
    let mut anchor_cache_2 = RunCache::default();
    let anchor = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache_2,
    )
    .unwrap();
    assert_eq!(
        anchor,
        DestAnchor::Resolved(feature_tip, dest_feature_tip),
        "once feature is mirrored, a fresh call to the search must find its own dest tip"
    );
}

#[test]
fn run_hard_fails_only_the_branch_with_two_incomparable_mirrored_ancestors() {
    // Two mirror-only branches, feature-a and feature-b, both branched
    // directly from the graft and mirrored independently — neither an
    // ancestor of the other. task is a real two-parent merge of both:
    // decisions/0043's anchor search finds two equally specific,
    // incomparable candidates and must hard-fail — but only task's own
    // line, not the whole run (decisions/0024's per-branch precedent).
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
        .branch("feature-a", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature-a", &[("a.txt", "line1\n")]);
    let feature_a_tip = source_repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    source_repo
        .branch("feature-b", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature-b", &[("b.txt", "line1\n")]);
    let feature_b_tip = source_repo
        .find_branch("feature-b", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    // task: a real two-parent merge of feature-a and feature-b.
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let merge_oid = source_repo
        .commit(
            None,
            &signature,
            &signature,
            "merge feature-a and feature-b",
            &feature_a_tip.tree().unwrap(),
            &[&feature_a_tip, &feature_b_tip],
        )
        .unwrap();
    source_repo
        .branch("task", &source_repo.find_commit(merge_oid).unwrap(), false)
        .unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path())
        .expect_err("an ambiguous dest anchor must fail the overall run");
    // The run's own aggregate message stays deliberately generic
    // (decisions/0037's own precedent — the per-branch reporter line,
    // asserted via `ambiguous_anchor_message`'s own unit test below,
    // carries the specific candidate names and merge-base oids).
    assert!(
        format!("{err:#}").to_lowercase().contains("ambiguous"),
        "the run's own error should mention the ambiguous anchor: {err:#}"
    );

    // Other branches were unaffected: both siblings still mirrored.
    assert!(
        dest_repo
            .find_branch("feature-a", git2::BranchType::Local)
            .is_ok()
    );
    assert!(
        dest_repo
            .find_branch("feature-b", git2::BranchType::Local)
            .is_ok()
    );
    let dest_main_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_main_tip, dest_tip,
        "main's own sync must proceed normally alongside the halted branch"
    );

    // task itself was never pushed to dest.
    assert!(
        dest_repo
            .find_branch("task", git2::BranchType::Local)
            .is_err(),
        "a branch with an ambiguous dest anchor must never be pushed to dest"
    );
}

#[test]
fn sync_pair_to_dest_agrees_with_the_baseline_when_no_sibling_candidate_is_more_specific() {
    // Regression check: a mirror-only branch forked straight from
    // round-tripped main, with no other mirrored sibling more specific
    // than main's own graft — the new search must agree with the
    // baseline exactly, not produce anything different.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);

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
        "feature",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("feature must mirror to dest");

    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_feature_tip.parent_id(0).unwrap(),
        dest_tip,
        "with no sibling candidate more specific than main's own graft, the refined \
         search must agree with the baseline exactly"
    );
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_feature_tip.id()).unwrap();
    walk.hide(dest_tip).unwrap();
    let commits_since_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(commits_since_graft.len(), 1);
}

#[test]
fn sync_pair_to_dest_rewrite_rebuild_anchors_on_a_sibling_mirror_only_branch_too() {
    // decisions/0039's rewrite-rebuild arm shares dest_anchor_for_branch
    // with the brand-new-branch arm — a rewritten mirror-only branch
    // with a more specific mirrored sibling available must rebuild onto
    // that sibling, not onto main's graft, with no second
    // implementation.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
    let feature_tip = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // feature mirrors first, then task correctly anchors on it.
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("feature must mirror to dest");
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("task must mirror, anchored on feature's dest tip");
    let dest_task_tip_before = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    // Rewrite task itself (amend-shaped): reset to feature's tip and
    // give it a brand-new commit, triggering decisions/0039's
    // rewrite-rebuild arm on the next sync.
    source_repo
        .branch("task", &source_repo.find_commit(feature_tip).unwrap(), true)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "rewritten\n")]);

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rewritten mirror-only branch must rebuild, anchored on its sibling");

    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let dest_task_tip_after = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    assert_ne!(
        dest_task_tip_after.id(),
        dest_task_tip_before.id(),
        "the pre-rewrite mirror history must be replaced, not built upon"
    );
    assert_eq!(
        dest_task_tip_after.parent_id(0).unwrap(),
        dest_feature_tip.id(),
        "the rebuild must anchor on feature's own dest tip, not on main's graft, \
         confirming the shared call site benefits with no second implementation"
    );
    let tree = dest_task_tip_after.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("task.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"rewritten\n");
}

#[test]
fn dest_anchor_for_branch_equal_cbase_siblings_are_resolved_not_ambiguous() {
    // feature-a and feature-b both diverge from the exact same shared
    // commit, and task also branches from that same commit: task's
    // merge-base against feature-a and against feature-b is literally
    // the same oid, not merely two candidates that each tie the
    // baseline independently. Equal is the opposite of incomparable —
    // this must resolve, not read as ambiguous.
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
        .branch("shared", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "shared", &[("shared_feature.txt", "line1\n")]);
    let shared_tip = source_repo
        .find_branch("shared", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "feature-a",
            &source_repo.find_commit(shared_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(
        &source_repo,
        "feature-a",
        &[("feature_a_only.txt", "line1\n")],
    );
    source_repo
        .branch(
            "feature-b",
            &source_repo.find_commit(shared_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(
        &source_repo,
        "feature-b",
        &[("feature_b_only.txt", "line1\n")],
    );

    source_repo
        .branch("task", &source_repo.find_commit(shared_tip).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);
    let task_tip = source_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

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
        "feature-a",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("feature-a must mirror to dest");
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature-b",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("feature-b must mirror to dest");

    // feature-a's mirror replays two commits onto the graft (shared's
    // own commit, then feature-a's own) — its dest tip's parent is the
    // commit that actually carries the `Gitprism-Source-Commit` trailer
    // naming `shared_tip` exactly, which is the real dest-space anchor
    // decisions/0043 step 5 resolves to, not feature-a's live tip.
    let dest_feature_a_shared_commit = dest_repo
        .find_branch("feature-a", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .parent_id(0)
        .unwrap();

    let dest_url = dest_dir.path().display().to_string();
    let key = marker::load_key().unwrap();

    let mut anchor_cache = RunCache::default();
    let anchor = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache,
    )
    .unwrap();
    assert_eq!(
        anchor,
        DestAnchor::Resolved(shared_tip, dest_feature_a_shared_commit),
        "two siblings with the exact same merge-base must resolve, picking the \
         lexicographically smallest name (\"feature-a\"), not read as ambiguous"
    );

    // Deterministic: re-running the identical search from scratch
    // produces the same result every time.
    let mut anchor_cache_2 = RunCache::default();
    let anchor_again = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache_2,
    )
    .unwrap();
    assert_eq!(
        anchor, anchor_again,
        "the equal-cbase tie-break must be deterministic across repeated calls"
    );
}

#[test]
fn dest_anchor_for_branch_tries_every_equal_cbase_candidate_not_just_the_alphabetically_first() {
    // z-feature mirrors first at shared commit S. a-feature mirrors
    // second, and — since z-feature already has a dest ref by
    // then — a-feature's own anchor search finds z-feature as its own
    // more specific anchor and builds directly onto it, rather than
    // re-projecting S independently. So a-feature's own dest history
    // carries no branch-scoped marker naming S at all; only
    // z-feature's does. A naive fix that collapses equal-cbase
    // candidates to one representative *before* checking each one's own
    // dest history — picking "a-feature" for being alphabetically
    // first — would try only the one candidate with nothing to find,
    // and silently fall back to the coarser baseline even though
    // z-feature's real, more specific anchor is right there. This is
    // the exact shape reported in review of the first fix.
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
        .branch("shared", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "shared", &[("shared_feature.txt", "line1\n")]);
    let shared_tip = source_repo
        .find_branch("shared", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "z-feature",
            &source_repo.find_commit(shared_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "z-feature", &[("z_only.txt", "line1\n")]);

    source_repo
        .branch(
            "a-feature",
            &source_repo.find_commit(shared_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "a-feature", &[("a_only.txt", "line1\n")]);

    source_repo
        .branch("task", &source_repo.find_commit(shared_tip).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);
    let task_tip = source_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();

    // z-feature mirrors first — its own dest chain independently
    // projects shared_tip, branch-scoped to "z-feature".
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "z-feature",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("z-feature must mirror to dest");
    let dest_z_feature_shared_commit = dest_repo
        .find_branch("z-feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .parent_id(0)
        .unwrap();

    // a-feature mirrors second, with z-feature already mirrored — its
    // own anchor search must find z-feature and build directly onto it.
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "a-feature",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a-feature must mirror to dest, anchored on z-feature");
    let dest_a_feature_tip = dest_repo
        .find_branch("a-feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_a_feature_tip.parent_id(0).unwrap(),
        dest_z_feature_shared_commit,
        "a-feature must build directly onto z-feature's own dest tip, not re-project \
         shared_tip independently — confirming a-feature's own dest history has no \
         branch-scoped marker naming shared_tip for the test below to matter"
    );

    let dest_url = dest_dir.path().display().to_string();
    let key = marker::load_key().unwrap();
    let mut anchor_cache = RunCache::default();
    let anchor = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache,
    )
    .unwrap();
    assert_eq!(
        anchor,
        DestAnchor::Resolved(shared_tip, dest_z_feature_shared_commit),
        "task must anchor on z-feature's real, more specific projection of shared_tip, \
         not silently fall back to the coarser baseline just because a-feature — the \
         alphabetically first equal-cbase candidate — has no marker of its own to find"
    );
}

#[test]
fn dest_anchor_for_branch_hard_fails_when_equal_cbase_candidates_resolve_differently() {
    // Two branches independently, separately given their own valid
    // SourceToDest projection of the exact same source commit S — not
    // through gitprism's own recursive anchoring (which would make one
    // defer to the other, as in the test above), but as if each was
    // mirrored in total isolation from the other, e.g. by two clones
    // that never saw each other's dest ref. Both are equally specific,
    // genuinely different dest-space anchors for the same S — gitprism
    // must hard-fail this, not silently pick one.
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
        .branch("shared", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let s = add_commit(&source_repo, "shared", &[("shared_feature.txt", "line1\n")]);

    source_repo
        .branch("feature-x", &source_repo.find_commit(s).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature-x", &[("x_only.txt", "line1\n")]);
    source_repo
        .branch("feature-y", &source_repo.find_commit(s).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature-y", &[("y_only.txt", "line1\n")]);

    source_repo
        .branch("task", &source_repo.find_commit(s).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);
    let task_tip = source_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let key = marker::load_key().unwrap();
    let exclude_list = ExcludeList::from_contents("").unwrap();
    let s_commit = repo.find_commit(s).unwrap();
    let filtered_tree = filter_tree(
        &repo,
        &s_commit.tree().unwrap(),
        Path::new(""),
        &exclude_list,
    )
    .unwrap();

    // Two independent, differently-branded dest projections of the
    // exact same source commit `s` — the only way to construct this
    // without one deferring to the other via gitprism's own recursive
    // anchoring.
    let dest_x_at_s = build_dest_commit(
        &repo,
        &config,
        graft,
        &s_commit,
        filtered_tree,
        "feature-x",
        &key,
    )
    .unwrap();
    git::push(
        source_dir.path(),
        &dest_dir.path().display().to_string(),
        dest_x_at_s,
        "feature-x",
        PushMode::FastForwardOnly,
    )
    .unwrap();
    let dest_y_at_s = build_dest_commit(
        &repo,
        &config,
        graft,
        &s_commit,
        filtered_tree,
        "feature-y",
        &key,
    )
    .unwrap();
    git::push(
        source_dir.path(),
        &dest_dir.path().display().to_string(),
        dest_y_at_s,
        "feature-y",
        PushMode::FastForwardOnly,
    )
    .unwrap();

    let dest_url = dest_dir.path().display().to_string();
    let mut anchor_cache = RunCache::default();
    let anchor = dest_anchor_for_branch(
        &repo,
        source_dir.path(),
        &dest_url,
        "task",
        task_tip,
        &key,
        &mut anchor_cache,
    )
    .unwrap();
    match anchor {
        DestAnchor::AmbiguousResolution(candidates) => {
            let names: Vec<&str> = candidates.iter().map(|(n, _)| n.as_str()).collect();
            assert!(
                names.contains(&"feature-x"),
                "must name feature-x: {names:?}"
            );
            assert!(
                names.contains(&"feature-y"),
                "must name feature-y: {names:?}"
            );
            let message = ambiguous_resolution_message("task", &candidates);
            assert!(message.contains("\"task\""));
            assert!(message.contains(&dest_x_at_s.to_string()));
            assert!(message.contains(&dest_y_at_s.to_string()));
        }
        other => panic!("expected AmbiguousResolution, got {other:?}"),
    }
}

#[test]
fn fetch_dest_tip_cached_hits_the_cache_without_fetching_again() {
    // decisions/0043 step 5: trying every member of an equal-cbase
    // group (see the tests above) must not cost a fresh `git fetch` per
    // member per branch searched — the exact regression a review of the
    // first fix for this caught: reintroducing O(branch count²) network
    // calls via `fetch` instead of `ls-remote`. Proven directly, not
    // just by absence of a slowdown: the second call is pointed at a
    // deliberately broken remote and must still succeed, returning the
    // identical oid — which is only possible if it never touched the
    // remote at all.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut run_cache = RunCache::default();

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut run_cache,
    )
    .expect("feature must mirror to dest");

    // sync_pair_to_dest's own push-accept path already cached this tip
    // as a side effect — remove it so the first call below is a genuine
    // miss, not a hit disguised as one.
    run_cache.dest_tip.remove("feature");

    let real_dest_url = dest_dir.path().display().to_string();
    let first = fetch_dest_tip_cached(
        &repo,
        source_dir.path(),
        &real_dest_url,
        "feature",
        &mut run_cache,
    )
    .expect("the first call must really fetch");

    // A URL that cannot possibly be fetched from — if the second call
    // hits the cache as required, this is never touched.
    let broken_dest_url = dest_dir.path().join("does-not-exist").display().to_string();
    let second = fetch_dest_tip_cached(
        &repo,
        source_dir.path(),
        &broken_dest_url,
        "feature",
        &mut run_cache,
    )
    .expect(
        "a cache hit must succeed even against an unreachable remote — proof it never \
         fetched again",
    );

    assert_eq!(
        first, second,
        "the cached oid must be returned unchanged on the second call"
    );
}

#[test]
fn sync_pair_to_dest_wrong_order_task_does_not_self_correct_on_an_ordinary_resync() {
    // task mirrored before feature has a dest ref falls back to the
    // coarser baseline (main's graft) for that run. decisions/0043 is
    // explicit that an ordinary LATER resync of task — unchanged, not
    // rewritten — does not retry the anchor search: the direct
    // behavioral proof of that now-corrected claim.
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
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
    let feature_tip = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // "This run": task discovered before feature has a dest ref, then
    // feature mirrors — one shared cache, matching one run's
    // per-branch loop (decisions/0043 step 2).
    let mut run_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut run_cache,
    )
    .expect("task must mirror, falling back to the baseline since feature has no dest ref yet");
    let dest_task_tip_first = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    // Wrong order, baseline fallback: task's mirror flattens both
    // feature's own commit and task's own commit directly onto main's
    // graft (decisions/0043's documented ordering hazard) — two
    // commits beyond `dest_tip`, not a chain built onto feature's own
    // dest tip.
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task_tip_first.id()).unwrap();
    walk.hide(dest_tip).unwrap();
    let commits_since_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_graft.len(),
        2,
        "wrong order: task must fall back to main's graft, replaying feature's and \
         task's own commits as its own flattened chain"
    );

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut run_cache,
    )
    .expect("feature must mirror to dest");

    // The shared cache accurately reflects both branches processed
    // this run — decisions/0043 step 2's read-through/update contract.
    assert_eq!(run_cache.dest_ref_exists.get("task"), Some(&true));
    assert_eq!(run_cache.dest_ref_exists.get("feature"), Some(&true));

    // "A later resync": task is completely unchanged, no rewrite — a
    // fresh cache, since decisions/0043 is explicit this is an
    // ordinary later resync, not part of the run above.
    let mut later_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut later_cache,
    )
    .expect("an unchanged resync of task must still succeed as a no-op");

    let dest_task_tip_second = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_task_tip_second.id(),
        dest_task_tip_first.id(),
        "an ordinary resync of an unchanged task must not self-correct onto feature's dest tip"
    );
}

#[test]
fn sync_pair_to_dest_wrong_order_rewrite_of_task_picks_up_feature_as_the_anchor() {
    // Same wrong-order topology as the no-self-correction test above,
    // but task itself is then genuinely rewritten (decisions/0039's
    // four conditions) — the documented operator workaround, proven to
    // actually rebuild task's dest chain onto feature's dest tip.
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

    // feature gets two commits; task's *original* fork point is only
    // feature's first one, not its eventual tip — so task's wrong-order
    // flattened mirror below never happens to replay feature's exact
    // tip commit. Otherwise feature's own later first mirror would tie
    // exactly onto a commit task's chain already built (a real commit,
    // correct content, but branded for "task" in its trailer, not
    // "feature" — decisions/0043 step 5's branch-scoped marker lookup
    // could then never recognize it as feature's own again), pushing no
    // new commit and leaving feature with no genuinely "feature"-branded
    // dest commit for the rewrite below to find.
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature_step1.txt", "line1\n")]);
    let feature_step1 = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    add_commit(&source_repo, "feature", &[("feature_step2.txt", "line1\n")]);
    let feature_tip = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_step1).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "line1\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    // Wrong order, same as above: task mirrors first and falls back to
    // the baseline, then feature mirrors.
    let mut run_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut run_cache,
    )
    .expect("task must mirror, falling back to the baseline since feature has no dest ref yet");
    let dest_task_tip_before = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    // Wrong order, baseline fallback (same shape as the no-self-correct
    // test above): two flattened commits beyond `dest_tip`.
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task_tip_before.id()).unwrap();
    walk.hide(dest_tip).unwrap();
    let commits_since_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_graft.len(),
        2,
        "wrong order: task must fall back to main's graft, replaying feature's and \
         task's own commits as its own flattened chain"
    );

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut run_cache,
    )
    .expect("feature must mirror to dest");
    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    // The operator workaround: rewrite task itself (amend-shaped) —
    // reset to feature's tip and give it a brand-new commit, triggering
    // decisions/0039's rewrite-rebuild arm on the next sync.
    source_repo
        .branch("task", &source_repo.find_commit(feature_tip).unwrap(), true)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "rewritten\n")]);

    let mut later_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut later_cache,
    )
    .expect("a rewritten mirror-only branch must rebuild, anchored on its sibling");

    let dest_task_tip_after = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    assert_ne!(
        dest_task_tip_after.id(),
        dest_task_tip_before.id(),
        "the pre-rewrite mirror history must be replaced, not built upon"
    );
    assert_eq!(
        dest_task_tip_after.parent_id(0).unwrap(),
        dest_feature_tip.id(),
        "the rebuild must pick up feature as the more specific anchor, proving the \
         documented operator workaround actually works after the wrong-order case"
    );
    let tree = dest_task_tip_after.tree().unwrap();
    let blob = dest_repo
        .find_blob(tree.get_name("task.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"rewritten\n");
}

#[test]
fn run_reproduces_the_round_tripped_feature_rebase_anchor_ambiguity() {
    // Production topology: develop is round-tripped, a round-tripped
    // feature receives develop's change on dest and brings it back to source,
    // a sibling task is mirrored, and a mirror-only task is then rebased onto
    // the feature. The task's resulting graph has the feature as its first
    // parent and the round-tripped develop commit as a second parent, so the
    // old global merge-base search sees two incomparable candidates.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip =
        bare_repo_with_a_commit_on(dest_dir.path(), "develop", &[("shared.txt", "v1\n")]);
    dest_repo
        .reference(
            "refs/heads/feat/supplier-specific-accounting-data",
            dest_tip,
            true,
            "seed feature",
        )
        .unwrap();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "develop", dest_tip, &dest_repo);
    let graft = source_repo
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    source_repo
        .branch(
            "feat/supplier-specific-accounting-data",
            &source_repo.find_commit(graft).unwrap(),
            false,
        )
        .unwrap();
    add_commit(
        &source_repo,
        "feat/supplier-specific-accounting-data",
        &[("feature.txt", "feature base\n")],
    );
    source_repo
        .branch(
            "tasks/supplier-specific-accounting-data/add_tax_tables",
            &source_repo.find_commit(graft).unwrap(),
            false,
        )
        .unwrap();
    add_commit(
        &source_repo,
        "tasks/supplier-specific-accounting-data/add_tax_tables",
        &[("task.txt", "before rebase\n")],
    );
    let develop_change = add_commit(
        &source_repo,
        "develop",
        &[("develop.txt", "develop change\n")],
    );

    let source_remote_dir = tempdir().unwrap();
    Repository::init_bare(source_remote_dir.path()).unwrap();
    let source_url = source_remote_dir.path().display().to_string();
    for branch in ["develop", "feat/supplier-specific-accounting-data"] {
        let tip = source_repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            git::push(
                source_dir.path(),
                &source_url,
                tip,
                branch,
                PushMode::FastForwardOnly,
            )
            .unwrap(),
            git::PushOutcome::Accepted
        );
    }

    let config = write_config(
        &source_url,
        &dest_dir.path().display().to_string(),
        &["develop", "feat/supplier-specific-accounting-data"],
    );
    run(source_dir.path(), config.path()).expect("initial round-trip branches must sync");

    // The feature branch lands an independent merge of develop on dest. The
    // next full run reflects that merge back into source as a DestToSource
    // marker, which deliberately has feature's branch scope and is therefore
    // not accepted as the target task's own anchor marker.
    let dest_develop_tip = dest_repo
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let dest_feature_tip = dest_repo
        .find_branch(
            "feat/supplier-specific-accounting-data",
            git2::BranchType::Local,
        )
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let dest_feature_merge = {
        let feature_commit = dest_repo.find_commit(dest_feature_tip).unwrap();
        let mut tree_builder = dest_repo
            .treebuilder(Some(&feature_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(b"develop change\n").unwrap();
        tree_builder
            .insert("develop.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(tree_builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some("refs/heads/feat/supplier-specific-accounting-data"),
                &signature,
                &signature,
                "Merge develop into feature",
                &tree,
                &[
                    &feature_commit,
                    &dest_repo.find_commit(dest_develop_tip).unwrap(),
                ],
            )
            .unwrap()
    };
    run(source_dir.path(), config.path())
        .expect("the dest-side feature merge must round-trip into source");
    let source_feature_tip = source_repo
        .find_branch(
            "feat/supplier-specific-accounting-data",
            git2::BranchType::Local,
        )
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert!(
        marker::verify(
            &source_repo.find_commit(source_feature_tip).unwrap(),
            "feat/supplier-specific-accounting-data",
            &[MarkerDirection::DestToSource],
            Some(dest_feature_merge),
            &marker::load_key().unwrap(),
        )
        .is_some(),
        "the feature merge must be represented by its authenticated source marker"
    );

    let sibling = "tasks/supplier-specific-accounting-data/unmapping_zfsglobus";
    source_repo
        .branch(
            sibling,
            &source_repo.find_commit(source_feature_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, sibling, &[("sibling.txt", "sibling\n")]);
    run(source_dir.path(), config.path()).expect("the sibling task must mirror");
    assert!(
        dest_repo
            .find_branch(sibling, git2::BranchType::Local)
            .is_ok(),
        "the sibling task must have a dest projection before the rewrite"
    );

    // Rebase the mirror-only task onto feature while retaining develop as a
    // second parent in the resulting graph. This is the graph shape that
    // makes the old algorithm's develop and feature merge bases incomparable.
    let task = "tasks/supplier-specific-accounting-data/add_tax_tables";
    source_repo
        .branch(
            task,
            &source_repo.find_commit(source_feature_tip).unwrap(),
            true,
        )
        .unwrap();
    let feature_commit = source_repo.find_commit(source_feature_tip).unwrap();
    let develop_commit = source_repo.find_commit(develop_change).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some(&format!("refs/heads/{task}")),
            &signature,
            &signature,
            "Rebase task onto feature",
            &feature_commit.tree().unwrap(),
            &[&feature_commit, &develop_commit],
        )
        .unwrap();
    add_commit(&source_repo, task, &[("task.txt", "after rebase\n")]);

    run(source_dir.path(), config.path())
        .expect("a routine mirror-only rebase must not halt on global merge-base ambiguity");

    let rebuilt_task_tip = dest_repo
        .find_branch(task, git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        rebuilt_task_tip.parent_id(0).unwrap(),
        dest_feature_merge,
        "the rebased task must rebuild from the feature projection"
    );
    let tree = rebuilt_task_tip.tree().unwrap();
    let task_blob = dest_repo
        .find_blob(tree.get_name("task.txt").unwrap().id())
        .unwrap();
    assert_eq!(task_blob.content(), b"after rebase\n");
}
