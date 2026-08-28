use super::*;

#[test]
fn run_carries_a_real_two_parent_merge_on_dest_into_source_as_one_net_change() {
    // decisions/0035: pending_commits is shared by both directions —
    // dest's own two-parent merges must collapse to their first
    // parent's diff on source too, not replay the merged-in branch's
    // own commit before the merge restores it. No equivalent coverage
    // existed for dest→source before this decision.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
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

    let a1 = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("main.txt", "m1\n"),
        "an independent main-side change",
    );
    let f1 = add_independent_dest_commit_on(
        &dest_repo,
        "feature",
        dest_tip,
        ("feature.txt", "f1\n"),
        "an independent feature-side change",
    );

    // A real two-parent merge on dest: main stays first parent,
    // feature's own commit is second — its own tree carries shared.txt,
    // main.txt, and feature.txt.
    let a1_commit = dest_repo.find_commit(a1).unwrap();
    let f1_commit = dest_repo.find_commit(f1).unwrap();
    let mut builder = dest_repo
        .treebuilder(Some(&a1_commit.tree().unwrap()))
        .unwrap();
    let feature_entry = f1_commit
        .tree()
        .unwrap()
        .get_name("feature.txt")
        .unwrap()
        .id();
    builder
        .insert("feature.txt", feature_entry, git2::FileMode::Blob.into())
        .unwrap();
    let merge_tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    dest_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature' into 'main'",
            &merge_tree,
            &[&a1_commit, &f1_commit],
        )
        .unwrap();

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("dest's real merge should reach source");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_source_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = new_source_tip.tree().unwrap();
    assert!(tree.get_name("main.txt").is_some());
    assert!(
        tree.get_name("feature.txt").is_some(),
        "the merge's own content must reach source even though feature's own commit is \
         only reachable via the merge's second parent"
    );

    let mut revwalk = source_remote_repo.revwalk().unwrap();
    revwalk.push(new_source_tip.id()).unwrap();
    revwalk.hide(dest_tip).unwrap();
    assert_eq!(
        revwalk.count(),
        3,
        "the graft commit, one commit for a1, and one for the merge — feature's own dest \
         commit must never be applied to source on its own"
    );
}

#[test]
fn run_dest_to_source_is_a_no_op_on_the_second_run() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
    let dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);

    add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("dest-only.txt", "from a merged PR"),
        "an independent dest-side change",
    );

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("first run should succeed");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let tip_after_first = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Nothing new landed on dest and nothing new landed on source since
    // the first run — a second run must be a true no-op, not re-walk
    // back to the graft and re-cherry-pick the same content again.
    run(source_dir.path(), config.path()).expect("second, no-op run should succeed");

    let tip_after_second = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_after_second, tip_after_first,
        "the no-op run must not add another commit to source"
    );
}

#[test]
fn mirrored_commit_rejects_non_utf8_messages_without_advancing_refs() {
    let (dir, repo, first, _) = repository_with_commits();
    let invalid = commit_with_raw_message(&repo, first, b"bad\xff message\n");
    let config = Config::load(write_config("unused", "unused", &["main"]).path()).unwrap();
    let tree = repo.find_commit(invalid).unwrap().tree_id();
    let key = marker::load_key().unwrap();

    let dest_error = build_dest_commit(
        &repo,
        &config,
        first,
        &repo.find_commit(invalid).unwrap(),
        tree,
        "main",
        &key,
    )
    .unwrap_err();
    assert!(dest_error.to_string().contains(&invalid.to_string()));

    let source_error = build_source_commit(
        &repo,
        &config,
        first,
        &repo.find_commit(invalid).unwrap(),
        tree,
        "main",
        &key,
    )
    .unwrap_err();
    assert!(source_error.to_string().contains(&invalid.to_string()));
    assert_eq!(
        repo.find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .target(),
        Some(first),
        "rejecting a malformed message must not advance the branch"
    );
    drop(dir);
}

#[test]
fn mirrored_commit_preserves_valid_unicode_messages() {
    let (dir, repo, first, _) = repository_with_commits();
    let unicode = commit_with_raw_message(&repo, first, "héllö 世界\n".as_bytes());
    let config = Config::load(write_config("unused", "unused", &["main"]).path()).unwrap();
    let tree = repo.find_commit(unicode).unwrap().tree_id();
    let key = marker::load_key().unwrap();
    let built = build_dest_commit(
        &repo,
        &config,
        first,
        &repo.find_commit(unicode).unwrap(),
        tree,
        "main",
        &key,
    )
    .unwrap();
    assert!(
        repo.find_commit(built)
            .unwrap()
            .message()
            .unwrap()
            .contains("héllö 世界")
    );
    drop(dir);
}

#[test]
fn sync_pair_from_dest_does_not_reflect_a_source_originated_commit_back_to_source() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
    let dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);

    // Simulate source→dest having already pushed a commit onto dest —
    // carries Gitprism-Source-Commit, gitprism's own trailer for that
    // direction. The fixture helper turns this legacy message spelling
    // into the authenticated commit shape production code writes.
    let looped_dest_tip = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "v2"),
        &format!("gitprism sync: source -> dest\n\nGitprism-Source-Commit: {graft_tip}\n"),
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
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());
    sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
        .expect("a loop-prevented sync is still a successful no-op");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        tip.id(),
        graft_tip,
        "a dest commit carrying Gitprism-Source-Commit must not be cherry-picked back onto source"
    );
    let _ = looped_dest_tip; // the authenticated marker, not the tip identity, is under test
}

#[test]
fn sync_pair_from_dest_fetches_the_local_branch_when_absent_from_the_checkout() {
    // Reproduces a real GitLab CI shape: a pipeline triggered by a push
    // to some other branch checks out only that branch, so a
    // round-tripped `config.branches` entry (here "develop") has no
    // local `refs/heads/develop` in this checkout at all, even though it
    // exists on both dest and source's own remote.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    bare_repo_with_a_commit_on(dest_dir.path(), "develop", &[("shared.txt", "v1")]);
    let dest_tip = dest_repo
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "develop", dest_tip, &dest_repo);
    let graft_tip = source_repo
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    // Stands in for source's own hosted remote — already grafted, as if
    // `gitprism setup` had run and pushed this branch there previously.
    let source_remote = bare_source_remote_seeded_at(&source_repo, "develop", graft_tip);

    // A merged dest PR, independent of anything gitprism has reflected
    // into source yet.
    add_independent_dest_commit_on(
        &dest_repo,
        "develop",
        dest_tip,
        ("shared.txt", "merged PR content"),
        "merged PR on dest",
    );

    // Simulate this checkout not having "develop" locally: detach HEAD
    // (git2 refuses to delete the branch HEAD currently points at) and
    // delete the local branch, without touching the object database — a
    // fresh single-ref CI clone would never have created this ref in the
    // first place, but the effect on `find_branch` is the same either
    // way.
    source_repo.set_head_detached(graft_tip).unwrap();
    source_repo
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .delete()
        .unwrap();

    let config = Config::load(
        write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["develop"],
        )
        .path(),
    )
    .unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    sync_pair_from_dest(&source_repo, source_dir.path(), &config, "develop", &reporter)
        .expect("a missing local branch should be fetched from source's own remote, not treated as a hard failure");

    let local_tip = source_repo
        .find_branch("develop", git2::BranchType::Local)
        .expect("the local branch should have been created from the fetched tip")
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_ne!(
        local_tip, graft_tip,
        "dest's independent commit should have been merged onto the newly created local branch"
    );

    let upstream = Repository::open(source_remote.path()).unwrap();
    let upstream_tip = upstream
        .find_branch("develop", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        upstream_tip.id(),
        local_tip,
        "the pushed remote tip and the newly created local branch must agree"
    );
    let blob = upstream
        .find_blob(
            upstream_tip
                .tree()
                .unwrap()
                .get_name("shared.txt")
                .unwrap()
                .id(),
        )
        .unwrap();
    assert_eq!(blob.content(), b"merged PR content");
}

#[test]
fn sync_pair_from_dest_does_not_trust_a_dest_authored_mapping_trailer() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft_tip);
    add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "dest change"),
        &format!("ordinary dest commit\n\nGitprism-Source-Commit: {graft_tip}\n"),
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
    let reporter = Reporter::new(1, std::iter::empty());
    sync_pair_from_dest(&source_repo, source_dir.path(), &config, "main", &reporter)
        .expect("a forged trailer is ordinary user text");

    let upstream = Repository::open(source_remote.path()).unwrap();
    let tip = upstream
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let blob = upstream
        .find_blob(tip.tree().unwrap().get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"dest change");
}

#[test]
fn sync_pair_from_dest_hard_stops_on_a_real_conflict() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "line one\n")]);
    let dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    // Source independently changes the same file, differently from dest
    // below — the two sides now genuinely disagree.
    add_commit(
        &source_repo,
        "main",
        &[("shared.txt", "line one changed by source\n")],
    );
    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

    let conflicting_dest_commit = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "line one changed by dest\n"),
        "an independent, conflicting dest-side change",
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
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());

    let err = sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
        .expect_err("a real same-file conflict must hard-stop, not silently resolve either side");
    let message = format!("{err:#}");
    assert!(message.contains(&conflicting_dest_commit.to_string()));
    assert!(message.contains("resolve"));
    assert!(message.contains("shared.txt"));

    // Nothing must have been pushed to source's own remote — there was
    // exactly one pending dest commit, and it conflicted.
    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        tip.id(),
        source_tip,
        "a conflicting dest commit must not be partially applied or pushed"
    );
}

#[test]
fn sync_pair_from_dest_still_marks_a_dest_commit_that_cherry_picks_to_a_no_op() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
    let dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    // Source already has note.txt="unchanged" before dest ever touches it.
    add_commit(&source_repo, "main", &[("note.txt", "unchanged")]);
    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

    // Dest commit A: a real, distinct change (cherry-picks with an
    // actual diff).
    let dest_a = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "v2"),
        "a real independent change",
    );
    // Dest commit B: adds note.txt="unchanged" too — coincidentally
    // identical to what source already has, so once cherry-picked onto
    // source (which already has it), the net diff is nothing.
    let dest_b = add_independent_dest_commit(
        &dest_repo,
        dest_a,
        ("note.txt", "unchanged"),
        "a no-op once merged onto source",
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
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());
    sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
        .expect("dest→source should succeed even when the last commit is a no-op");

    // The newest commit on source must still name dest_b exactly, even
    // though cherry-picking it changed nothing — otherwise the resume
    // boundary stays stuck on dest_a forever and source→dest can never
    // recognize dest_b's tip as accounted for.
    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        new_tip
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {dest_b}")),
        "a cherry-pick that changes nothing must still get its own marker commit"
    );

    // With that marker in place, source→dest must actually recognize
    // dest_b's tip as accounted for and proceed normally, not refuse.
    let repo = Repository::open(source_dir.path()).unwrap();
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        branch,
        &reporter,
        &mut dest_ref_cache,
    )
    .expect(
        "source→dest must recognize a dest tip whose only marker is a no-op commit, not refuse it",
    );
}

#[test]
fn sync_pair_from_dest_carries_dests_edit_onto_a_file_source_renamed() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(
        dest_dir.path(),
        "main",
        &[("old.txt", "line one\nline two\nline three\n")],
    );

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit_removing(
        &source_repo,
        "main",
        &["old.txt"],
        &[("new.txt", "line one\nline two\nline three\n")],
    );
    // `add_commit_removing` only moves the branch ref, the same as every
    // other low-level fixture in this module — but this test, unlike
    // most, goes on to call `sync_pair_from_dest` directly, which checks
    // the merge result out onto the working tree
    // (`advance_local_source_branch`). A real checkout compares against
    // the actual on-disk index, so it has to be brought into step with
    // the rename first, or an unrelated fixture artifact (not gitprism's
    // own merge) would surface as a checkout conflict.
    source_repo
        .checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
        .unwrap();
    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

    let dest_edit_tip = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("old.txt", "line one\nline two\nline three edited by dest\n"),
        "an independent dest-side edit",
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
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());

    sync_pair_from_dest(&repo, source_dir.path(), &config, branch, &reporter)
        .expect("dest's edit to a file source renamed must carry across cleanly");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = new_tip.tree().unwrap();
    assert!(
        tree.get_name("old.txt").is_none(),
        "the renamed-away path must not survive on source"
    );
    let new_blob = source_remote_repo
        .find_blob(tree.get_name("new.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(new_blob.content()),
        "line one\nline two\nline three edited by dest\n"
    );
    assert!(
        new_tip
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {dest_edit_tip}"))
    );
}
