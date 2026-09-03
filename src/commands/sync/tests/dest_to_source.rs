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

// decisions/0048 (pending): the dest→source boundary must resume from
// whichever of source's own inherited marker (B1) or a self-authenticated
// SourceToDest marker reachable on dest's own line (B2) is newest — not B1
// alone. The tests below are `docs/plans/2026-09-02/CODE-001-dest-to-source-
// boundary.md`'s step 2: scenario 1 is the review's own repro, adopted
// verbatim; scenarios 2, 3 and 5 fail against today's B1-only boundary for
// the reason the plan predicts (a phantom conflict or an extra/missing
// phantom commit — CODE-001, `docs/2026-09-02_REPOSITORY_REVIEW.md` §3);
// scenarios 4 and 6 pin behavior the fix must *not* change (today's code
// already gets these right) and are expected to pass already.

#[test]
fn mirror_only_branch_later_added_to_config_branches_only_reflects_dest_native_commits() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    // Two source commits on main touching the same line.
    add_commit_with_message(&source_repo, "main", &[("shared.txt", "v2\n")], "s1");
    let s2 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v3\n")], "s2");

    let config_main_only = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config_main_only.path()).expect("first sync");

    // Cut a release branch from main AFTER setup; mirror it (mirror-only).
    source_repo
        .branch("release", &source_repo.find_commit(s2).unwrap(), false)
        .unwrap();
    run(source_dir.path(), config_main_only.path()).expect("second sync mirrors release");
    let dest_release_tip = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Customer merges a PR into dest's release.
    add_independent_dest_commit_on(
        &dest_repo,
        "release",
        dest_release_tip,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );

    // Seed source's remote with release, then promote release to round-tripped.
    git::push(
        source_repo.workdir().unwrap(),
        &source_remote.path().display().to_string(),
        s2,
        "release",
        PushMode::FastForwardOnly,
    )
    .unwrap();
    let config_both = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main", "release"],
    );
    run(source_dir.path(), config_both.path())
        .expect("promoting a mirrored branch must not replay main's own mirrored commits");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let release_tip = source_remote_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut revwalk = source_remote_repo.revwalk().unwrap();
    revwalk.push(release_tip.id()).unwrap();
    revwalk.hide(s2).unwrap();
    assert_eq!(
        revwalk.count(),
        1,
        "only the customer's commit should be reflected into source's release"
    );
}

#[test]
fn dest_to_source_resumes_from_a_marker_inherited_from_a_sibling_branch() {
    // Scenario 2: a customer cuts dest `hotfix` from dest `main` right at
    // main's own newest mirror; a developer independently cuts source
    // `hotfix` from the same source commit. Neither side has anything of
    // its own yet — the boundary must be `main`'s own mirror marker at
    // hotfix's exact tip (a B2 match, inherited from a sibling branch), not
    // the Setup graft, or main's own already-mirrored history gets replayed
    // as phantom pending commits onto hotfix and hits the same kind of
    // same-line conflict CODE-001 reports.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);

    add_commit_with_message(&source_repo, "main", &[("shared.txt", "v2\n")], "s1");
    let s2 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v3\n")], "s2");

    // Mirror main up through s2 before hotfix exists at all.
    let config_main = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config_main,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("main should mirror up through s2");
    let dest_s2 = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Customer cuts dest hotfix from dest main at D(s2); developer cuts
    // source hotfix at s2 — no mirror of hotfix itself has happened yet.
    dest_repo
        .branch("hotfix", &dest_repo.find_commit(dest_s2).unwrap(), false)
        .unwrap();
    source_repo
        .branch("hotfix", &source_repo.find_commit(s2).unwrap(), false)
        .unwrap();

    let config = Config::load(
        write_config(
            "unused",
            &dest_dir.path().display().to_string(),
            &["main", "hotfix"],
        )
        .path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let hotfix_before = source_repo
        .find_branch("hotfix", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    sync_pair_from_dest(&repo, source_dir.path(), &config, "hotfix", &reporter).expect(
        "hotfix's own boundary is main's mirror marker at its own tip — nothing is pending",
    );

    let hotfix_after = source_repo
        .find_branch("hotfix", git2::BranchType::Local)
        .unwrap()
        .get()
        .target()
        .unwrap();
    assert_eq!(
        hotfix_after, hotfix_before,
        "no phantom commits from main's own mirrored history should be replayed onto hotfix"
    );
}

#[test]
fn dest_main_fast_forwarded_to_a_mirrored_release_branch_only_reflects_content_since_its_own_mirror()
 {
    // Scenario 3: dest main gets fast-forwarded onto dest release's own
    // history (e.g. a customer merges release into main on dest). Source
    // main never had release's own commit (s3) — the boundary must stop at
    // release's own earlier mirror (E, whose counterpart s2 *is* an
    // ancestor of source main), not at D(s3) (whose counterpart s3 isn't),
    // and not at the Setup graft either (which would replay E itself as a
    // phantom commit, since E's own marker names branch "release", not
    // "main" — CODE-001's branch-scoped loop prevention never recognizes
    // it). Both D(s3) and the customer's own commit must still reach
    // source main — dropping them would lose release's content that main
    // just inherited.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);

    let s2 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v2\n")], "s2");
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", s2);

    // release cut from s2; one release-only commit, s3.
    source_repo
        .branch("release", &source_repo.find_commit(s2).unwrap(), false)
        .unwrap();
    add_commit_with_message(&source_repo, "release", &[("release.txt", "r3\n")], "s3");

    // Mirror release to dest for the first time — decisions/0044's
    // mirror-only path: E carries the release marker for s2, D(s3) for s3.
    let config_release = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config_release,
        "release",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("first sync should mirror release to dest");
    let d_s3 = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Customer merges a PR directly onto dest release.
    let c = add_independent_dest_commit_on(
        &dest_repo,
        "release",
        d_s3,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );

    // Customer fast-forwards dest main to dest release's tip.
    dest_repo
        .reference("refs/heads/main", c, true, "fast-forward main to release")
        .unwrap();

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
        .expect("dest->source should reflect only content since main's own mirror boundary");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    let mut revwalk = source_remote_repo.revwalk().unwrap();
    revwalk.push(new_tip.id()).unwrap();
    revwalk.hide(s2).unwrap();
    assert_eq!(
        revwalk.count(),
        2,
        "only s3's mirror and the customer's commit should be replayed onto source main — not \
         release's own earlier mirror of s2, which main already has natively"
    );

    let tree = new_tip.tree().unwrap();
    assert!(
        tree.get_name("release.txt").is_some(),
        "s3's content must reach source main"
    );
}

#[test]
fn dest_to_source_resumes_from_its_own_import_marker_when_nothing_new_has_landed() {
    // Scenario 4: after a dest→source import writes its own DestToSource
    // marker naming the imported dest commit, and dest hasn't moved since,
    // the boundary must be that marker (B1) — it's the descendant of every
    // other candidate, so B1 wins without a walk being needed at all. A
    // second run must be a true no-op. Unlike scenarios 2/3/5, today's B1-
    // only code already gets this right — this pins that the fix must not
    // regress it. An older SourceToDest marker (D, release's own first
    // mirror) sits below B1 on dest's first-parent line so a broken
    // implementation that forgot the B1 lower bound on the walk — and
    // wandered past it looking for any B2 match — has something to
    // wrongly land on instead of vacuously passing.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    source_repo
        .branch("release", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    git::push(
        source_repo.workdir().unwrap(),
        &source_remote.path().display().to_string(),
        graft,
        "release",
        PushMode::FastForwardOnly,
    )
    .unwrap();

    // Mirror release to dest once, before any customer content lands — its
    // own first sync (decisions/0044/0047) writes an older, self-verified
    // SourceToDest marker (D) below B1 on dest's first-parent line.
    let config_release = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config_release,
        "release",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("release should mirror to dest once before any customer content lands");
    let d_release = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let config = Config::load(
        write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main", "release"],
        )
        .path(),
    )
    .unwrap();

    let c = add_independent_dest_commit_on(
        &dest_repo,
        "release",
        d_release,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );

    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    sync_pair_from_dest(&repo, source_dir.path(), &config, "release", &reporter)
        .expect("first import should reflect the customer's commit");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let tip_after_import = source_remote_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        tip_after_import
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {c}")),
        "the import must name the customer's dest commit as its own boundary"
    );

    // No later mirror: dest stays exactly where it is. A second dest→source
    // run must resume from its own freshly-written marker rather than
    // re-scanning further back.
    let repo = Repository::open(source_dir.path()).unwrap();
    sync_pair_from_dest(&repo, source_dir.path(), &config, "release", &reporter)
        .expect("a second, no-op run must still succeed");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let tip_after_second = source_remote_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_after_second,
        tip_after_import.id(),
        "the no-op run must not add another commit"
    );
}

#[test]
fn dest_to_source_stops_at_the_branchs_own_shared_base_not_an_inherited_earlier_mirror() {
    // Scenario 5: source release is cut from main's s1 — before s2 exists —
    // so release's own tip *is* main's s1 commit, verbatim. Dest release is
    // cut from dest main's own mirror of that same point, D(s1); a customer
    // commit (C0) lands on top, and then a further marker commit —
    // authenticated exactly like a real SourceToDest mirror, but naming s2
    // as its counterpart — lands on top of that (D(s2), dest release's own
    // tip). Source release's own first-parent history only ever reaches the
    // Setup graft (it has no commit of its own), so today's B1 is the
    // graft, and D(s1) (main's own mirror of s1 — the exact point release
    // shares with main) is – ancestor-wise – indistinguishable from a
    // phantom: its own marker says branch "main", so branch-scoped loop
    // prevention never recognizes it as release's own already-had content,
    // and it gets replayed like CODE-001's other cases even though it's a
    // pure no-op. D(s2) — dest release's own tip — must itself be rejected
    // by the walk: its counterpart s2 is *not* an ancestor of source
    // release's tip s1 (s2 comes after it). The boundary must instead
    // resolve to D(s1) (self-verified, its counterpart *is* source
    // release's own tip) — C0 and D(s2) are what's actually pending.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    let s1 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v2\n")], "s1");
    let s2 = add_commit_with_message(&source_repo, "main", &[("shared.txt", "v3\n")], "s2");

    // release is cut from s1 — before s2 — with no commits of its own.
    source_repo
        .branch("release", &source_repo.find_commit(s1).unwrap(), false)
        .unwrap();
    git::push(
        source_repo.workdir().unwrap(),
        &source_remote.path().display().to_string(),
        s1,
        "release",
        PushMode::FastForwardOnly,
    )
    .unwrap();

    // Mirror main up through s2 — D(s1) and D(s2) both carry branch "main".
    let config_main = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config_main,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("main should mirror up through s2");
    let dest_s2 = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let dest_s1 = dest_repo
        .find_commit(dest_s2)
        .unwrap()
        .parent_id(0)
        .unwrap();

    // Dest release is cut from dest main's own mirror of release's exact
    // shared base, D(s1) — release's own true starting point.
    dest_repo
        .branch("release", &dest_repo.find_commit(dest_s1).unwrap(), false)
        .unwrap();
    let c0 = add_independent_dest_commit_on(
        &dest_repo,
        "release",
        dest_s1,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );

    // A further marker commit lands on top of C0, authenticated exactly
    // like a real SourceToDest mirror but naming s2 — a commit release
    // never actually received — as its counterpart. This becomes dest
    // release's own tip, so the walk must reject the tip itself, not just
    // an intermediate commit. Branded "main" (not "release", unlike
    // `add_source_marker_commit_on_dest`'s usual same-branch shape) so
    // `pending_dest_commits`'s own branch-scoped loop prevention doesn't
    // filter it out before the boundary walk ever sees it — the same way
    // D(s1) above is real main content landing, untouched, on release's line.
    let d_s2 = {
        let parent_commit = dest_repo.find_commit(c0).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(b"v3\n").unwrap();
        builder
            .insert("shared.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let message = marker::build_message(
            "gitprism sync: source -> dest",
            MarkerDirection::SourceToDest,
            "main",
            s2,
            "Gitprism-Source-Commit",
            &[c0],
            tree.id(),
            &signature,
            &signature,
            &marker::load_key().unwrap(),
        );
        dest_repo
            .commit(
                Some("refs/heads/release"),
                &signature,
                &signature,
                &message,
                &tree,
                &[&parent_commit],
            )
            .unwrap()
    };

    let config = Config::load(
        write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main", "release"],
        )
        .path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    sync_pair_from_dest(&repo, source_dir.path(), &config, "release", &reporter).expect(
        "dest->source should reflect C0 and D(s2), not replay D(s1) as a phantom too, and not \
         mistake D(s2) for a valid boundary",
    );

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_tip = source_remote_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut revwalk = source_remote_repo.revwalk().unwrap();
    revwalk.push(new_tip.id()).unwrap();
    revwalk.hide(s1).unwrap();
    assert_eq!(
        revwalk.count(),
        2,
        "only the customer's commit and D(s2)'s own mirror should be replayed onto source \
         release — not a phantom no-op for D(s1), which release already has natively as its own \
         tip"
    );
    assert!(
        new_tip
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {d_s2}")),
        "the newest source commit must name D(s2) — dest release's own tip — as its own resume \
         boundary"
    );
    let tree = new_tip.tree().unwrap();
    let blob = source_remote_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        blob.content(),
        b"v3\n",
        "D(s2)'s content (s2's own change) must reach source release"
    );
}

#[test]
fn dest_to_source_refuses_when_the_boundary_marker_names_an_object_missing_from_the_local_odb() {
    // Scenario 6a: the precondition that keeps the fix fail-closed —
    // unchanged by decisions/0048 — must still refuse when the boundary a
    // source-recorded marker names simply isn't present locally (e.g. dest
    // was rewritten/garbage-collected out from under it), before any walk
    // is attempted.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    // A dest-space oid this local checkout has never fetched — stands in
    // for a rewritten/garbage-collected dest history where the object the
    // source-recorded boundary names is simply gone.
    let scratch_dir = tempdir().unwrap();
    let missing_oid =
        bare_repo_with_a_commit_on(scratch_dir.path(), "main", &[("gone.txt", "gone\n")]);

    // Overwrite source's own boundary marker to point at that missing
    // object, standing in for a marker whose named dest commit is no
    // longer reachable/present.
    add_dest_marker_commit(&source_repo, "main", graft, missing_oid);

    add_independent_dest_commit(
        &dest_repo,
        d0,
        ("customer.txt", "customer\n"),
        "an independent dest-side change",
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
    let err = sync_pair_from_dest(&repo, source_dir.path(), &config, "main", &reporter).expect_err(
        "a boundary marker naming an object missing from the local odb must refuse, not mis-walk",
    );
    let message = format!("{err:#}");
    assert!(message.contains("isn't an ancestor of dest's current tip"));
}

#[test]
fn dest_to_source_refuses_when_dest_is_force_rewound_past_an_already_imported_commit() {
    // Scenario 6b: dest release is force-rewound back to D — an older
    // commit that itself carries a legitimate, self-verifying SourceToDest
    // marker (a B2 match) — after source already imported a newer
    // customer commit (B1). B1 is not on dest's current line at all, so
    // the precondition must refuse before any walk ever gets to see that D
    // would otherwise satisfy B2; walking past the precondition here would
    // silently accept a rewound dest instead of reporting the divergence
    // (see the plan's "Revision history": this is exactly the flaw an
    // earlier, unshipped draft had).
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let d0 = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", d0, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);

    source_repo
        .branch("release", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let s1 = add_commit_with_message(&source_repo, "release", &[("release.txt", "r1\n")], "s1");
    git::push(
        source_repo.workdir().unwrap(),
        &source_remote.path().display().to_string(),
        s1,
        "release",
        PushMode::FastForwardOnly,
    )
    .unwrap();

    // First mirror: D, a legitimate SourceToDest marker on dest release.
    let config = Config::load(
        write_config(
            &source_remote.path().display().to_string(),
            &dest_dir.path().display().to_string(),
            &["main", "release"],
        )
        .path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "release",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("first mirror of release should reach dest");
    let d = dest_repo
        .find_branch("release", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Customer merges a PR onto dest release, then it round-trips back to
    // source, becoming source's own authenticated boundary (B1).
    add_independent_dest_commit_on(
        &dest_repo,
        "release",
        d,
        ("customer.txt", "customer\n"),
        "customer PR merged on dest release",
    );
    let repo = Repository::open(source_dir.path()).unwrap();
    sync_pair_from_dest(&repo, source_dir.path(), &config, "release", &reporter)
        .expect("the customer's commit should import cleanly");

    // Dest is force-rewound back to D, discarding the customer's own
    // commit — a real history rewrite, not a fast-forward.
    dest_repo
        .reference("refs/heads/release", d, true, "force-rewind release")
        .unwrap();

    let repo = Repository::open(source_dir.path()).unwrap();
    let err = sync_pair_from_dest(&repo, source_dir.path(), &config, "release", &reporter)
        .expect_err(
            "a dest force-rewound behind an already-imported commit must refuse, even though \
             the rewound tip itself carries a legitimate mirror marker",
        );
    let message = format!("{err:#}");
    assert!(message.contains("isn't an ancestor of dest's current tip"));
}
