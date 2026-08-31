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
    // The dest-ref cache is a per-run performance
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
fn sync_pair_to_dest_anchors_a_task_branch_on_its_mirror_only_parent_feature_branch() {
    // task branched from mirror-only feature branched from round-tripped
    // main: task's dest chain must anchor on feature's exact mapped commit,
    // not on main's original graft — decisions/0046's branch-local case.
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
    let feature_first = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    add_commit(
        &source_repo,
        "feature",
        &[("feature-second.txt", "line2\n")],
    );

    source_repo
        .branch(
            "task",
            &source_repo.find_commit(feature_first).unwrap(),
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
    let dest_feature_first = dest_repo
        .find_commit(dest_feature_tip.parent_id(0).unwrap())
        .unwrap();

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("task must mirror to dest, anchored on feature's exact mapping");
    let dest_task_tip = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .expect("task must be mirrored to dest")
        .get()
        .peel_to_commit()
        .unwrap();

    // Real shared ancestry: task's dest commit is built directly onto the
    // exact feature commit where task forked, not re-flattened onto main's
    // graft or feature's later commit.
    assert_eq!(
        dest_task_tip.parent_id(0).unwrap(),
        dest_feature_first.id(),
        "task's dest commit must use the intermediate feature mapping, not \
         re-mirror feature's earlier commit from main's graft"
    );

    // A PR-shaped diff: exactly one new commit beyond the feature commit
    // where task forked — task's own task.txt commit.
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task_tip.id()).unwrap();
    walk.hide(dest_feature_first.id()).unwrap();
    let commits_since_feature: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_feature.len(),
        1,
        "task's dest chain must add exactly one commit onto its feature fork point"
    );

    let task_tree = dest_task_tip.tree().unwrap();
    assert!(
        task_tree.get_name("feature.txt").is_some(),
        "task's dest tree must still carry feature.txt, inherited from feature's own tip"
    );
    assert!(
        task_tree.get_name("feature-second.txt").is_none(),
        "the child must anchor at the intermediate mapping, before feature's later commit"
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
    // and the old sibling search both failed to use it as task's anchor.
    // The exact mapping index now anchors task at M, while loop prevention
    // still prevents any inherited marker from being replayed.
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
fn sync_pair_to_dest_anchors_a_freshly_forked_branch_on_the_exact_imported_destination_mapping() {
    // A dest-to-source marker M, scoped to "main", is inherited by "task"
    // (forked from main's tip after M landed). Exercised through the
    // test-only `sync_pair_to_dest` wrapper, which rebuilds the mapping
    // index from scratch on every call (unlike a real `run`, which builds
    // it once up front) — proving the exact-mapping anchor (decisions/0046)
    // finds M and lands directly on it even from that from-scratch state,
    // with no dependency on any other branch's sync having run first in the
    // same call. Because the anchor lands exactly on M, M never enters
    // `pending_commits`' range at all; this does not exercise
    // `build_pending_dest_tip`'s `loop_prevented` skip (see
    // `loop_prevented_skips_an_inherited_cross_branch_marker_when_the_branchs_own_dest_ref_predates_it`
    // for that).
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

    // A fresh, empty cache: nothing from main's own two calls above is
    // carried over, so task's anchor has to come from this call's own
    // from-scratch mapping reconstruction.
    let mut task_run_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut task_run_cache,
    )
    .expect("task must mirror, anchored on the exact mapping its own reconstruction finds for M");

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
        3,
        "task's chain must reuse the exact imported destination mapping, retaining the\
         existing s1 and X projections and adding only task's own commit"
    );
}

#[test]
fn loop_prevented_skips_an_inherited_cross_branch_marker_when_the_branchs_own_dest_ref_predates_it()
{
    // decisions/0046's exact-mapping anchor only replaces boundary search
    // for a branch with no dest ref yet, or a detected mirror-only rewrite
    // (`dest_anchor_for_branch`'s two call sites). An already-synced
    // branch's ordinary resync still resolves its boundary via
    // `dest_resume_point_for_branch`, scanning that branch's OWN dest
    // history — never the mapping index. So a `DestToSource` marker M this
    // branch inherits from a *different* branch's dest→source import can
    // still land strictly inside `pending_commits`, and
    // `build_pending_dest_tip`'s `loop_prevented` must recognize and skip
    // it via the marker's own recorded branch — the case
    // `build_pending_dest_tip_loop_prevents_a_dest_to_source_marker_scoped_to_another_branch`
    // covered before decisions/0046 gave a brand-new branch's own first
    // sync an exact-mapping anchor that lands directly on the marker
    // instead (see the rewritten test above this one).
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
        .unwrap();
    // "release" forks from the graft before main advances at all, and is
    // never added to `config.branches` — a plain mirror-only branch
    // (decisions/0017) that gets its own dest ref from its first sync
    // below, independent of anything that happens to "main" afterward.
    source_repo.branch("release", &graft, false).unwrap();

    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft.id());
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    run(source_dir.path(), config.path())
        .expect("release's first mirror must create its own dest ref at the graft");
    let release_dest_tip_before = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    add_commit(&source_repo, "main", &[("s1.txt", "s1\n")]);
    run(source_dir.path(), config.path()).expect("main must mirror s1.txt to dest");

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
    run(source_dir.path(), config.path())
        .expect("dest-native X must import into source main as marker M");

    // "release" now inherits M as an ordinary first-parent ancestor, but its
    // own dest ref (still sitting untouched at the graft) predates it —
    // this run's boundary for "release" comes from scanning release's own
    // dest history, not the mapping index, so M lands strictly inside
    // release's own `pending_commits`.
    let main_with_m = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    source_repo.branch("release", &main_with_m, true).unwrap();
    add_commit(&source_repo, "release", &[("release.txt", "r1\n")]);

    run(source_dir.path(), config.path())
        .expect("release must mirror its own commit, skipping the inherited marker M");

    let dest_release_tip = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_release_tip.id()).unwrap();
    walk.hide(release_dest_tip_before).unwrap();
    let commits_since_release_graft: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_release_graft.len(),
        2,
        "release's dest chain must add only s1 and its own commit — the inherited marker M, \
         scoped to \"main\", must be loop-prevented rather than replayed a second time"
    );

    let tree = dest_release_tip.tree().unwrap();
    assert!(
        tree.get_name("x.txt").is_none(),
        "release's dest tree must not carry x.txt — that content is main's own dest-native \
         import, not release's, and must not be reintroduced by replaying M"
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
fn fetch_dest_tip_cached_hits_the_cache_without_fetching_again() {
    // A fetched destination tip is cached for the whole run. Proven
    // directly, not just by absence of a slowdown: the second call is
    // pointed at a deliberately broken remote and must still succeed,
    // returning the identical oid — which is only possible if it never
    // touched the remote at all.
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
fn run_schedules_a_new_parent_before_a_lexically_earlier_new_child() {
    // Both branches start without destination refs. The child sorts first,
    // but its first-parent path has one extra unmapped commit, so distance
    // scheduling must project the parent before the child.
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
        .branch("z-feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let feature_source_tip = add_commit(&source_repo, "z-feature", &[("feature.txt", "feature\n")]);
    source_repo
        .branch(
            "a-task",
            &source_repo.find_commit(feature_source_tip).unwrap(),
            false,
        )
        .unwrap();
    add_commit(&source_repo, "a-task", &[("task.txt", "task\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("distance scheduling must project both branches");

    let dest_feature_tip = dest_repo
        .find_branch("z-feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let dest_task_tip = dest_repo
        .find_branch("a-task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_task_tip.parent_id(0).unwrap(),
        dest_feature_tip.id(),
        "the lexically earlier child must anchor on the parent projection created earlier in this run"
    );
    assert!(
        dest_task_tip
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some()
    );
    assert!(dest_task_tip.tree().unwrap().get_name("task.txt").is_some());
}

#[test]
fn run_does_not_resurrect_an_orphaned_dest_commit_after_a_same_run_amend() {
    // F-A's confirmed failure: mirror-only "feature" already has a two-commit
    // dest chain D1(S1), D2(S2). In the same run, "feature" gets amended
    // (S2 -> S2') and a new "task" branch — forked from the pre-amend S2 —
    // appears for the first time. Distance scheduling processes "feature"
    // (alphabetically first) before "task", so the ForceMirrorOnly rebuild
    // that replaces feature's dest history and orphans D2 must also
    // invalidate the run's in-memory index entry for S2 -> D2 — otherwise
    // "task" anchors on that entry and resurrects D2 as an ancestor of a
    // brand-new dest ref.
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
    add_commit(&source_repo, "feature", &[("feature.txt", "v1\n")]);
    let s2 = add_commit(&source_repo, "feature", &[("feature.txt", "v2\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("the initial two-commit mirror must sync");

    let d2 = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let d1 = dest_repo.find_commit(d2).unwrap().parent_id(0).unwrap();

    // Fork "task" from the pre-amend S2 before amending "feature" — its own
    // first-parent history still runs through S2.
    source_repo
        .branch("task", &source_repo.find_commit(s2).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "task\n")]);

    // Amend feature's second commit in place of S2.
    source_repo
        .branch(
            "feature",
            &source_repo.find_commit(s2).unwrap().parent(0).unwrap(),
            true,
        )
        .unwrap();
    add_commit_with_message(
        &source_repo,
        "feature",
        &[("feature.txt", "v2-amended\n")],
        "add feature v2 (amended)",
    );

    run(source_dir.path(), config.path())
        .expect("the amend and the new sibling task must both sync in the same run");

    let new_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        new_feature_tip.id(),
        d2,
        "the amend must rebuild feature's dest chain, not reuse it"
    );
    assert_eq!(
        new_feature_tip.parent_id(0).unwrap(),
        d1,
        "the rebuild must anchor on the still-valid S1 -> D1 mapping"
    );

    let task_tip = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        !dest_repo.graph_descendant_of(task_tip.id(), d2).unwrap(),
        "task must not be built on the orphaned pre-amend dest commit D2"
    );
    assert_ne!(
        task_tip.parent_id(0).unwrap(),
        d2,
        "task's immediate dest parent must not be the orphaned D2"
    );
    let task_tree = task_tip.tree().unwrap();
    let feature_blob = dest_repo
        .find_blob(task_tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        feature_blob.content(),
        b"v2\n",
        "task must still carry the pre-amend feature content it actually forked from"
    );
    assert!(task_tree.get_name("task.txt").is_some());
}

#[test]
fn run_reproduces_the_round_tripped_feature_rebase_anchor_ambiguity() {
    // Production topology: develop is round-tripped, a round-tripped
    // feature receives develop's change on dest and brings it back to source,
    // a sibling task is mirrored, and a mirror-only task is then rebased onto
    // the feature. The task's resulting graph has the feature as its first
    // parent and the round-tripped develop commit as a second parent, so
    // the superseded global merge-base search sees two incomparable
    // candidates.
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
        .expect("a routine mirror-only rebase must use the exact branch-local mapping");

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

#[test]
fn run_keeps_a_surviving_childs_dest_native_content_after_its_parent_branch_is_deleted() {
    // F-A: "feature" (round-tripped) imports dest-native X as a
    // DestToSource marker M branded "feature". "task" forks from feature's
    // tip after that import, inheriting M as an ordinary ancestor. Feature
    // is then merged and cleaned up — its local source branch deleted and
    // dropped from the round-tripped list — the routine post-merge
    // cleanup decisions/0018 Case 2 already expects. "feature" no longer
    // has its own entry in source_heads, so M is only reachable through
    // task's own first-parent scan; the mapping index must self-verify M
    // against its own recorded branch ("feature") rather than "task", or
    // task's anchor silently lands before M and X is dropped from task's
    // own projection.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip =
        bare_repo_with_a_commit_on(dest_dir.path(), "feature", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "feature", dest_tip, &dest_repo);
    let graft = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "feature", graft);
    let source_url = source_remote.path().display().to_string();

    let config = write_config(
        &source_url,
        &dest_dir.path().display().to_string(),
        &["feature"],
    );
    run(source_dir.path(), config.path()).expect("initial round-trip of feature must succeed");

    // Dest-native X, landing directly on dest's feature branch.
    let dest_feature_before_x = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    add_independent_dest_commit_on(
        &dest_repo,
        "feature",
        dest_feature_before_x,
        ("x.txt", "x\n"),
        "dest native X",
    );
    run(source_dir.path(), config.path()).expect("importing X into source feature must succeed");
    let dest_feature_tip_with_x = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // task forks from feature's tip, which now includes M.
    let feature_tip_with_m = source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    source_repo
        .branch("task", &feature_tip_with_m, false)
        .unwrap();
    add_commit(&source_repo, "task", &[("task.txt", "t\n")]);

    // Feature is merged and cleaned up: its local source branch is deleted
    // and it is dropped from the round-tripped list. HEAD must move off
    // feature first, or libgit2 refuses to delete the checked-out branch.
    source_repo.set_head("refs/heads/task").unwrap();
    source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();
    let config = write_config(&source_url, &dest_dir.path().display().to_string(), &[]);

    run(source_dir.path(), config.path())
        .expect("task must still mirror once feature's source branch is gone");

    let dest_task = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_task.tree().unwrap();
    assert!(
        tree.get_name("x.txt").is_some(),
        "X must still be present in task's projected base — dropped if the anchor walk \
         couldn't self-verify M against feature's own recorded branch"
    );

    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_task.id()).unwrap();
    walk.hide(dest_feature_tip_with_x).unwrap();
    let commits_since_feature_with_x: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_feature_with_x.len(),
        1,
        "task's dest chain must add only its own commit onto feature's dest tip that \
         already carries X — M must not be re-replayed now that feature's ref is gone"
    );
}

#[test]
fn run_anchors_a_sibling_on_a_deleted_mirror_only_branchs_own_dest_ref() {
    // F-A, second half: "feature" (mirror-only, never round-tripped) mirrors
    // its own commit F1 to dest as D_F1, branded "feature" via a
    // SourceToDest marker. "other" forks from F1 and gains a commit of its
    // own. Feature's local source branch is then deleted — routine
    // post-merge cleanup — so `source_branches` no longer names it and
    // `reconstruct_mapping_index` must still discover feature's own dest
    // ref directly rather than skip it, or "other" falls back to the
    // coarser graft and replays F1's content a second time as a duplicate
    // of D_F1.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip =
        bare_repo_with_a_commit_on(dest_dir.path(), "feature", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "feature", dest_tip, &dest_repo);

    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &[]);
    run(source_dir.path(), config.path()).expect("feature must mirror to dest");
    let dest_f1 = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch("other", &source_repo.find_commit(f1).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "other", &[("other.txt", "o1\n")]);

    source_repo.set_head("refs/heads/other").unwrap();
    source_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    run(source_dir.path(), config.path())
        .expect("other must still mirror once feature's source branch is gone");

    let dest_other = dest_repo
        .find_branch("other", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_other.parent_id(0).unwrap(),
        dest_f1,
        "other must anchor directly on feature's own dest commit for F1, found by scanning \
         feature's dest ref even though feature no longer has a local source branch"
    );
    let mut walk = dest_repo.revwalk().unwrap();
    walk.push(dest_other.id()).unwrap();
    walk.hide(dest_f1).unwrap();
    let commits_since_f1: Vec<_> = walk.collect::<std::result::Result<_, _>>().unwrap();
    assert_eq!(
        commits_since_f1.len(),
        1,
        "other's dest chain must add only its own commit onto D_F1 — F1 must not be \
         replayed a second time now that feature's source branch is gone"
    );
}

#[test]
fn run_halts_instead_of_losing_content_when_a_missing_mapping_would_be_loop_prevented() {
    // A missing DestToSource counterpart must not be treated as absent while
    // searching for a rewrite anchor.  Otherwise the search walks back to
    // the older setup mapping, replay later skips this marker as a loop, and
    // the accepted replacement silently loses the marker's imported tree.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let source_url = source_remote.path().display().to_string();
    let config = write_config(&source_url, &dest_dir.path().display().to_string(), &[]);

    // Establish an older exact mapping on dest, then rewrite source from the
    // graft so the next sync must use the mapping-index anchor path.
    add_commit(&source_repo, "main", &[("old.txt", "old\n")]);
    run(source_dir.path(), config.path()).expect("the initial mirror must succeed");
    let old_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .reference(
            "refs/heads/main",
            graft,
            true,
            "rewrite for missing mapping test",
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let parent = source_repo.find_commit(graft).unwrap();
    let mut tree_builder = source_repo
        .treebuilder(Some(&parent.tree().unwrap()))
        .unwrap();
    let imported_blob = source_repo.blob(b"imported\n").unwrap();
    tree_builder
        .insert("imported.txt", imported_blob, git2::FileMode::Blob.into())
        .unwrap();
    let tree = source_repo
        .find_tree(tree_builder.write().unwrap())
        .unwrap();
    let missing_dest = Oid::from_bytes(&[6; 20]).unwrap();
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let message = marker::build_message(
        "gitprism sync: dest -> source",
        MarkerDirection::DestToSource,
        "main",
        missing_dest,
        "Gitprism-Dest-Commit",
        &[graft],
        tree.id(),
        &signature,
        &signature,
        &marker::load_key().unwrap(),
    );
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            &message,
            &tree,
            &[&parent],
        )
        .unwrap();
    refresh_checked_out_branch(&source_repo, "main");
    add_commit(&source_repo, "main", &[("new.txt", "new\n")]);

    let error = run(source_dir.path(), config.path())
        .expect_err("the missing exact destination mapping must halt this rewrite branch");
    assert!(
        error.to_string().contains("one or more branches halted"),
        "the missing mapping must halt the branch rather than aborting in the lookup: {error:#}"
    );
    let dest_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_after, old_dest_tip,
        "a missing mapping must not fall back to the older anchor and push a projection that loses imported content"
    );
}

#[test]
fn mapping_distance_for_branch_does_not_abort_scheduling_when_a_mapping_is_missing_locally() {
    // F-B: `run`'s own scheduling loop calls this once per remaining
    // branch every iteration via a bare `?` — a raw `graph_descendant_of`
    // error on a dest object this clone never fetched previously
    // propagated straight through that `?`, aborting the whole run instead
    // of leaving just this one branch to its own later per-branch halt.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let main_tip = add_commit(&source_repo, "main", &[("m.txt", "m\n")]);

    let mut run_cache = RunCache::default();
    let missing = Oid::from_bytes(&[9; 20]).unwrap();
    // Two provenances for the identical exact source commit: one names a
    // dest object actually present in this clone, the other names one that
    // isn't — exactly the canonicalization shape that needs
    // `graph_descendant_of` to compare the two, which can't be done safely
    // against an object that was never fetched.
    run_cache
        .mapping_index
        .record_built_mapping("main", main_tip, dest_tip)
        .unwrap();
    run_cache
        .mapping_index
        .record_built_mapping("other", main_tip, missing)
        .unwrap();

    let distance = mapping_distance_for_branch(&source_repo, "main", &run_cache);
    assert!(
        distance.is_ok(),
        "a missing mapped dest object during scheduling must degrade to a per-branch \
         refusal, not raw-error and abort the whole run: {distance:?}"
    );
    assert_eq!(
        distance.unwrap(),
        Some(0),
        "the branch is still selectable this run — its own later per-branch halt happens \
         when its anchor is actually resolved, not during scheduling"
    );
}

#[test]
fn sync_pair_to_dest_wrong_order_task_does_not_self_correct_on_an_ordinary_resync() {
    // Ported from main (pre-decisions/0046): task mirrored before feature
    // has a dest ref falls back to the coarser baseline (main's graft) for
    // that run. An ordinary LATER resync of task — unchanged, not
    // rewritten — does not retry the anchor search.
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
    // feature mirrors — one shared cache, matching one run's per-branch
    // loop, forced here since `run`'s own scheduling would otherwise
    // process feature first.
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
    // graft — two commits beyond `dest_tip`, not a chain built onto
    // feature's own dest tip.
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

    assert_eq!(run_cache.dest_ref_exists.get("task"), Some(&true));
    assert_eq!(run_cache.dest_ref_exists.get("feature"), Some(&true));

    // "A later resync": task is completely unchanged, no rewrite — a fresh
    // cache and a fresh index reconstruction, since this is an ordinary
    // later resync, not part of the run above.
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
    // Ported from main (pre-decisions/0046): same wrong-order topology as
    // the no-self-correction test above, but task itself is then genuinely
    // rewritten (decisions/0039's four conditions) — the documented
    // operator workaround, proven to actually rebuild task's dest chain
    // onto feature's dest tip.
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
    // feature's first one, not its eventual tip.
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

    // Wrong order, same as above: task mirrors first and falls back to the
    // baseline, then feature mirrors.
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

    // The operator workaround: rewrite task itself (amend-shaped) — reset
    // to feature's tip and give it a brand-new commit, triggering
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
