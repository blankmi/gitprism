use super::*;

#[test]
fn run_pushes_a_new_source_commit_to_dest_filtered() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    // `.gitprismignore` is versioned in source (decisions/0011) — it has
    // to be part of the commit itself, not just written to the
    // filesystem, since `sync` filters using each commit's *own* tree.
    add_commit(
        &source_repo,
        "main",
        &[
            ("shared.txt", "v2"),
            ("secret.txt", "only for source"),
            (exclude::FILENAME, "secret.txt\n"),
        ],
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    let new_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(new_dest_tip.parent_id(0).unwrap(), dest_tip);
    assert_eq!(new_dest_tip.author().name().unwrap(), "A Developer");
    assert_eq!(
        new_dest_tip.committer().email().unwrap(),
        "gitprism@example.com"
    );
    assert!(
        new_dest_tip
            .message()
            .unwrap()
            .contains("Gitprism-Source-Commit:")
    );

    let tree = new_dest_tip.tree().unwrap();
    let shared = dest_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(shared.content(), b"v2");
    assert!(
        tree.get_name("secret.txt").is_none(),
        "an excluded file must never reach dest"
    );
    assert!(
        tree.get_name(exclude::FILENAME).is_none(),
        ".gitprismignore itself must never reach dest"
    );
}

#[test]
fn run_pushes_a_new_binary_file_to_dest_byte_identical() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    // NUL and high bytes — not valid UTF-8, and exactly the kind of
    // content libgit2 flags as a "binary" delta. With no `DiffOptions`,
    // `diff_tree_to_tree` omits the binary payload entirely, so
    // `apply_to_tree` can't reconstruct this file and misreports it as a
    // decisions/0007 content conflict instead of applying it.
    let binary_content: &[u8] = &[
        0x00, 0xFF, 0x01, 0xFE, b'b', b'i', b'n', 0x00, 0x89, b'P', b'N', b'G',
    ];
    add_commit_bytes(&source_repo, "main", &[("blob.bin", binary_content)]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path())
        .expect("sync should succeed and carry the binary file to dest");

    let new_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = new_dest_tip.tree().unwrap();
    let entry = tree
        .get_name("blob.bin")
        .expect("the binary file must reach dest");
    let blob = dest_repo.find_blob(entry.id()).unwrap();
    assert_eq!(
        blob.content(),
        binary_content,
        "the binary file's content must reach dest byte-identical"
    );
}

#[test]
fn run_pushes_nothing_when_the_only_pending_commit_filters_to_empty() {
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
    add_commit(
        &source_repo,
        "main",
        &[
            ("secret.txt", "only for source"),
            (exclude::FILENAME, "secret.txt\n"),
        ],
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed even with nothing to push");

    let still_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still_dest_tip.id(),
        dest_tip,
        "dest must not move when every pending commit filters to empty"
    );
}

#[test]
fn run_resumes_from_the_last_synced_commit_not_the_graft() {
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
    add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("first sync should succeed");
    // A second run with nothing new on source must be a true no-op, not
    // re-walk all the way back to the graft and re-push v2 again.
    run(source_dir.path(), config.path()).expect("second, no-op sync should succeed");

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    assert_eq!(
        revwalk.count(),
        2,
        "the no-op run must not add another commit"
    );
}

#[test]
fn run_does_not_reflect_a_dest_originated_commit_back_to_dest() {
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
    // Simulate dest→source having already reflected a dest commit back
    // onto source: a commit carrying Gitprism-Dest-Commit.
    let tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut builder = source_repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
    let blob = source_repo.blob(b"from dest").unwrap();
    builder
        .insert("shared.txt", blob, git2::FileMode::Blob.into())
        .unwrap();
    let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let message = marker::build_message(
        "gitprism resolve",
        MarkerDirection::DestToSource,
        "main",
        dest_tip,
        "Gitprism-Dest-Commit",
        &[tip.id()],
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
            &[&tip],
        )
        .unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    let still_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still_dest_tip.id(),
        dest_tip,
        "a commit that already came from dest must not be pushed back to dest"
    );
}

#[test]
fn run_does_not_trust_a_source_authored_mapping_trailer() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit_with_message(
        &source_repo,
        "main",
        &[("shared.txt", "source change")],
        &format!("ordinary source commit\n\nGitprism-Dest-Commit: {dest_tip}\n"),
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("a forged trailer is ordinary user text");

    let dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let blob = dest_repo
        .find_blob(
            dest_tip
                .tree()
                .unwrap()
                .get_name("shared.txt")
                .unwrap()
                .id(),
        )
        .unwrap();
    assert_eq!(blob.content(), b"source change");
}

#[test]
fn run_applies_the_current_exclude_list_even_to_an_already_committed_secret() {
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
    // First commit adds a secret with no exclude rule in effect yet.
    add_commit(
        &source_repo,
        "main",
        &[("shared.txt", "v2"), ("secret.txt", "leaked?")],
    );
    // A later commit adds the exclude rule, but never touches
    // secret.txt itself — decisions/0004 says the *current* list
    // governs everything being processed this run, not a per-commit
    // historical snapshot, so this must still catch the earlier commit.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "secret.txt\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    let new_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = new_dest_tip.tree().unwrap();
    assert!(
        tree.get_name("secret.txt").is_none(),
        "the current exclude-list must apply retroactively to an already-committed secret, not just commits made after the rule existed"
    );
}

#[test]
fn run_reflects_an_independent_dest_commit_into_source_and_still_syncs_source_to_dest() {
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
    add_commit(&source_repo, "main", &[("only-in-source.txt", "v2")]);
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

    // An independent change landing directly on dest (e.g. a PR merged
    // straight to dest) — content gitprism never put there and doesn't
    // know about yet. This must reach source (dest→source), and source's
    // own pending commit must still reach dest in the very same run
    // (source→dest) — neither direction blocks the other.
    let dest_only_tip = {
        let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
        let mut builder = dest_repo
            .treebuilder(Some(&dest_tip_commit.tree().unwrap()))
            .unwrap();
        let blob = dest_repo.blob(b"dest-only content").unwrap();
        builder
            .insert("dest-only.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
        dest_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "an independent dest-side change",
                &tree,
                &[&dest_tip_commit],
            )
            .unwrap()
    };

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("both directions should succeed");

    // dest→source: the independent commit landed on source's real
    // remote, cherry-picked, author preserved, gitprism as committer.
    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_source_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(new_source_tip.author().name().unwrap(), "Dest Maintainer");
    assert_eq!(
        new_source_tip.committer().email().unwrap(),
        "gitprism@example.com"
    );
    assert!(
        new_source_tip
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {dest_only_tip}"))
    );
    let new_source_tree = new_source_tip.tree().unwrap();
    assert!(
        new_source_tree.get_name("dest-only.txt").is_some(),
        "dest's independent content must reach source"
    );
    assert!(
        new_source_tree.get_name("only-in-source.txt").is_some(),
        "source's own pre-existing content must survive the cherry-pick"
    );

    // The local checkout's own branch ref must have advanced to match
    // what was just pushed — later same-run logic (and any future git
    // command against this checkout) needs to see it.
    let local_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(local_tip.id(), new_source_tip.id());
    // The working directory must actually reflect it too, not just the
    // ref — this branch is the one checked out in `source_dir`, so a
    // ref-only move would leave `dest-only.txt` missing on disk (and the
    // checkout looking dirty relative to its own HEAD).
    assert_eq!(
        fs::read_to_string(source_dir.path().join("dest-only.txt")).unwrap(),
        "dest-only content",
        "dest→source must check the working tree out, not just move the branch ref"
    );

    // source→dest: source's own pending commit still reached dest, in
    // this same run, even though dest→source ran first.
    let new_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(new_dest_tip.parent_id(0).unwrap(), dest_only_tip);
    let new_dest_tree = new_dest_tip.tree().unwrap();
    assert!(new_dest_tree.get_name("only-in-source.txt").is_some());
    assert!(
        new_dest_tree.get_name("dest-only.txt").is_some(),
        "dest's own pre-existing content must survive the filtered push"
    );
}

#[test]
fn run_refuses_to_sync_a_divergent_clone_even_though_dest_has_a_trailer() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    // Two independent clones of the same freshly-grafted source, exactly
    // like two checkouts of one real repo — clone A syncs first.
    let clone_a_dir = tempdir().unwrap();
    let clone_a = source_grafted_onto(clone_a_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(
        &clone_a,
        "main",
        &[("shared.txt", "vA"), ("only-a.txt", "from A")],
    );
    let config_a = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(clone_a_dir.path(), config_a.path()).expect("clone A's sync should succeed");

    let dest_tip_after_a = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // Clone B was grafted from the *same* original dest tip, before A's
    // push — its own source_tip is a sibling of A's commit, not a
    // descendant of it. Dest's tip now carries a
    // `Gitprism-Source-Commit` trailer naming A's commit, which is not
    // an ancestor of clone B's source_tip at all.
    let clone_b_dir = tempdir().unwrap();
    let clone_b = source_grafted_onto(clone_b_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(
        &clone_b,
        "main",
        &[("shared.txt", "vB"), ("only-b.txt", "from B")],
    );
    let config_b = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

    let err = run(clone_b_dir.path(), config_b.path()).expect_err(
        "a divergent clone must not rebuild its own snapshot on top of dest just because dest's tip has *some* Gitprism-Source-Commit trailer",
    );
    assert!(format!("{err:#}").contains("diverged"));

    let still_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still_dest_tip.id(),
        dest_tip_after_a,
        "a refused sync must not touch dest's branch at all"
    );
    let tree = still_dest_tip.tree().unwrap();
    assert!(
        tree.get_name("only-a.txt").is_some(),
        "clone A's already-synced content must survive clone B's refused sync"
    );
    assert!(
        tree.get_name("only-b.txt").is_none(),
        "clone B's content must never have been pushed"
    );
}

#[test]
fn run_refuses_a_divergent_clone_even_when_dests_tip_is_an_independent_commit() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    // Clone A syncs first, same as the sibling test above.
    let clone_a_dir = tempdir().unwrap();
    let clone_a = source_grafted_onto(clone_a_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(
        &clone_a,
        "main",
        &[("shared.txt", "vA"), ("only-a.txt", "from A")],
    );
    let config_a = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(clone_a_dir.path(), config_a.path()).expect("clone A's sync should succeed");

    let dest_tip_after_a = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // An independent commit lands directly on dest afterward, e.g. a
    // merged PR — dest's tip is now this commit, not a gitprism-written
    // one.
    let independent = add_independent_dest_commit(
        &dest_repo,
        dest_tip_after_a,
        ("dest-only.txt", "from a merged PR"),
        "an independent dest-side change",
    );

    // Clone B was grafted from the *same original* dest tip, before A's
    // push — its own source_tip is a sibling of A's commit, not a
    // descendant of it.
    let clone_b_dir = tempdir().unwrap();
    let clone_b = source_grafted_onto(clone_b_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(
        &clone_b,
        "main",
        &[("shared.txt", "vB"), ("only-b.txt", "from B")],
    );
    let clone_b_tip = clone_b
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    // Clone B's dest→source will legitimately cherry-pick the
    // independent dest commit and push it — needs its own real source
    // remote for that push to land somewhere.
    let clone_b_remote = bare_source_remote_seeded_at(&clone_b, "main", clone_b_tip);
    let config_b = write_config(
        &clone_b_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    let err = run(clone_b_dir.path(), config_b.path()).expect_err(
        "a divergent clone must not rebuild its own snapshot on top of dest just because dest→source could reflect dest's independent tip into it",
    );
    assert!(format!("{err:#}").contains("diverged"));

    let still_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still_dest_tip.id(),
        independent,
        "a refused sync must not touch dest's branch at all"
    );
    let tree = still_dest_tip.tree().unwrap();
    assert!(
        tree.get_name("only-a.txt").is_some(),
        "clone A's already-synced content must survive clone B's refused sync"
    );
    assert!(
        tree.get_name("only-b.txt").is_none(),
        "clone B's content must never have been pushed"
    );
}

#[test]
fn run_does_not_reapply_an_already_synced_commit_after_an_independent_dest_commit() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let notes_commit = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", notes_commit);

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    let tip_after_first = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // An independent change lands directly on dest — e.g. a merged PR —
    // content gitprism never put there.
    add_independent_dest_commit(
        &dest_repo,
        tip_after_first,
        ("dest-only.txt", "x\n"),
        "dest: a merged PR",
    );

    run(source_dir.path(), config.path())
        .expect("second sync, after an independent dest commit, should still succeed");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let notes_blob = dest_repo
        .find_blob(tree.get_name("notes.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        notes_blob.content(),
        b"line1\n",
        "an already-synced commit must not be reapplied on top of itself"
    );

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    assert_eq!(
        revwalk.count(),
        3,
        "dest history must be exactly: initial, the notes.txt push, the independent commit"
    );

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    let source_marker_count = revwalk
        .filter_map(|oid| oid.ok())
        .filter(|oid| {
            let commit = dest_repo.find_commit(*oid).unwrap();
            marker::parse(commit.message().unwrap_or(""))
                .is_some_and(|state| state.counterpart == notes_commit)
        })
        .count();
    assert_eq!(
        source_marker_count, 1,
        "exactly one dest commit should carry a Gitprism-Source-Commit trailer naming the notes.txt commit"
    );

    let tip_before_third = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    run(source_dir.path(), config.path()).expect("third sync should succeed");
    let tip_after_third = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_before_third, tip_after_third,
        "a third, no-op sync must not move dest's tip"
    );
}

#[test]
fn run_does_not_conflict_on_an_already_synced_commit_after_an_independent_dest_commit() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let shared_commit = add_commit(&source_repo, "main", &[("shared.txt", "v2\n")]);
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", shared_commit);

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    let tip_after_first = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let independent = add_independent_dest_commit(
        &dest_repo,
        tip_after_first,
        ("dest-only.txt", "x\n"),
        "dest: a merged PR",
    );

    run(source_dir.path(), config.path())
        .expect("second sync, after an independent dest commit, should still succeed");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_tip_commit.id(),
        independent,
        "with nothing new pending, dest's tip must still be the independent commit"
    );
    let tree = dest_tip_commit.tree().unwrap();
    let shared_blob = dest_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(shared_blob.content(), b"v2\n");

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    assert_eq!(
        revwalk.count(),
        3,
        "dest history must be exactly: initial, the shared.txt push, the independent commit"
    );

    run(source_dir.path(), config.path()).expect("third sync should succeed and move nothing");
    let tip_after_third = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_after_third, independent,
        "the pair must not be permanently stuck — a later sync must still succeed and move nothing"
    );
}

#[test]
fn run_does_not_duplicate_a_no_ff_merges_content_on_dest() {
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

    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);

    // An ordinary `git merge --no-ff feature`: main hasn't moved since the
    // graft, so the merge's own tree is exactly f1's tree, with main's
    // tip as first parent and f1 as second.
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let main_tip = source_repo.find_commit(graft).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &f1_commit.tree().unwrap(),
            &[&main_tip, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let feature_blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        feature_blob.content(),
        b"line1\n",
        "the merge's own first-parent diff must not re-apply feature.txt's content on \
         top of what the revwalk already applied for f1 (pre-fix: b\"line1\\nline1\\n\")"
    );

    // The merge commit itself contributes nothing beyond its side branch
    // (main hadn't moved), so its filtered diff against the cursor (f1)
    // is empty — requirements/0001 forbids pushing an empty commit, so
    // dest gets exactly one gitprism commit for f1, not two.
    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    assert_eq!(
        revwalk.count(),
        2,
        "dest history must be exactly: initial, one commit for f1 (the merge adds nothing)"
    );

    let tip_before_second = dest_tip_commit.id();
    run(source_dir.path(), config.path()).expect("second sync should succeed");
    let tip_after_second = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_before_second, tip_after_second,
        "a second, no-op sync must not move dest's tip"
    );
}

#[test]
fn run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once() {
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

    let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

    // A merge commit on main with parents [a1, f1] whose tree carries
    // shared.txt, main.txt, and feature.txt.
    let a1_commit = source_repo.find_commit(a1).unwrap();
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let mut builder = source_repo
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
    let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &merge_tree,
            &[&a1_commit, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    // decisions/0035 removed the interleaved-branch churn this comment
    // used to describe: feature's own commit (f1) is no longer emitted
    // by the first-parent-only walk, so dest never gets an intermediate
    // commit temporarily missing the other branch's file. Only the tip
    // is checked here regardless.
    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let shared_blob = dest_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    let main_blob = dest_repo
        .find_blob(tree.get_name("main.txt").unwrap().id())
        .unwrap();
    let feature_blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(shared_blob.content(), b"v1\n");
    assert_eq!(main_blob.content(), b"m1\n");
    assert_eq!(
        feature_blob.content(),
        b"f1\n",
        "pre-fix: feature.txt's content is duplicated on dest's tip"
    );

    let tip_before_second = dest_tip_commit.id();
    run(source_dir.path(), config.path()).expect("second sync should succeed");
    let tip_after_second = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        tip_before_second, tip_after_second,
        "a second, no-op sync must not move dest's tip"
    );
}

#[test]
fn run_honors_a_conflict_resolved_by_hand_inside_a_merge_commit() {
    // decisions/0035: R0 -> R1 -> R2 on main, R0 -> F1 on feature, both
    // R1 and F1 edit shared.txt differently, and M merges feature into
    // main with the conflict resolved by hand in M's own tree. Under the
    // full-DAG walk this hard-stops on a genuine merge-tree conflict
    // when F1 is replayed on its own; under the first-parent walk only
    // M itself is applied, carrying the human's resolution.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "base\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    add_commit(&source_repo, "main", &[("shared.txt", "landing\n")]);
    let r2 = add_commit(&source_repo, "main", &[("other.txt", "r2\n")]);

    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("shared.txt", "feature\n")]);

    // M resolves the shared.txt conflict by hand: neither side's own
    // content, main's r2 as first parent, feature's f1 as second.
    let r2_commit = source_repo.find_commit(r2).unwrap();
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let mut builder = source_repo
        .treebuilder(Some(&r2_commit.tree().unwrap()))
        .unwrap();
    let resolved_blob = source_repo.blob(b"resolved-by-human\n").unwrap();
    builder
        .insert("shared.txt", resolved_blob, git2::FileMode::Blob.into())
        .unwrap();
    let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &merge_tree,
            &[&r2_commit, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect(
        "a merge's own hand-resolved conflict must not be re-litigated against the \
         feature branch's own diff",
    );

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let shared_blob = dest_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        shared_blob.content(),
        b"resolved-by-human\n",
        "dest's tip must carry the human's resolution recorded in the merge commit itself"
    );
    let other_blob = dest_repo
        .find_blob(tree.get_name("other.txt").unwrap().id())
        .unwrap();
    assert_eq!(other_blob.content(), b"r2\n");
}

#[test]
fn run_applies_a_clean_two_parent_merge_without_replaying_the_side_branchs_own_commit() {
    // decisions/0035: unlike `run_carries_a_merge_of_two_diverged_source_branches_to_dest_exactly_once`
    // (which asserts only final content, identical under either walk),
    // this counts dest's own history to show feature's own commit is no
    // longer replayed onto dest individually — the merge is carried as
    // one net change against its first parent.
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

    let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

    let a1_commit = source_repo.find_commit(a1).unwrap();
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let mut builder = source_repo
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
    let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &merge_tree,
            &[&a1_commit, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("a clean two-parent merge should sync");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    assert!(tree.get_name("main.txt").is_some());
    assert!(tree.get_name("feature.txt").is_some());

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push(dest_tip_commit.id()).unwrap();
    assert_eq!(
        revwalk.count(),
        3,
        "dest history must be exactly: initial, one commit for a1, one for the merge — \
         feature's own commit (f1) must never be applied to dest on its own"
    );
}

#[test]
fn pending_commits_still_hides_a_boundary_reachable_only_via_a_merges_second_parent() {
    // decisions/0035: hide()'s own ancestor-exclusion walks all of a
    // hidden commit's parents regardless of simplify_first_parent, which
    // only restricts what the walk *emits*. `boundary` here is a merge's
    // second parent; the root beneath it is also a first-parent ancestor
    // of `tip` and must still be excluded.
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let empty_tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();

    let root = repo
        .commit(None, &signature, &signature, "root", &empty_tree, &[])
        .unwrap();
    let root_commit = repo.find_commit(root).unwrap();
    let x = repo
        .commit(
            None,
            &signature,
            &signature,
            "x",
            &empty_tree,
            &[&root_commit],
        )
        .unwrap();
    let x_commit = repo.find_commit(x).unwrap();
    let c = repo
        .commit(
            None,
            &signature,
            &signature,
            "c",
            &empty_tree,
            &[&root_commit],
        )
        .unwrap();
    let c_commit = repo.find_commit(c).unwrap();

    let unrelated_root = repo
        .commit(
            None,
            &signature,
            &signature,
            "unrelated-root",
            &empty_tree,
            &[],
        )
        .unwrap();
    let unrelated_root_commit = repo.find_commit(unrelated_root).unwrap();
    let y = repo
        .commit(
            None,
            &signature,
            &signature,
            "y",
            &empty_tree,
            &[&unrelated_root_commit],
        )
        .unwrap();
    let y_commit = repo.find_commit(y).unwrap();

    // boundary: first parent y (unrelated to root), second parent x
    // (root's own child) — root is reachable from boundary only via its
    // second parent.
    let boundary = repo
        .commit(
            None,
            &signature,
            &signature,
            "boundary",
            &empty_tree,
            &[&y_commit, &x_commit],
        )
        .unwrap();
    let boundary_commit = repo.find_commit(boundary).unwrap();

    // merge: first parent c (root's other child, on tip's own
    // first-parent line), second parent boundary (already synced).
    let merge = repo
        .commit(
            None,
            &signature,
            &signature,
            "merge",
            &empty_tree,
            &[&c_commit, &boundary_commit],
        )
        .unwrap();
    let merge_commit = repo.find_commit(merge).unwrap();
    let tip = repo
        .commit(
            None,
            &signature,
            &signature,
            "tip",
            &empty_tree,
            &[&merge_commit],
        )
        .unwrap();

    let pending = pending_commits(&repo, boundary, tip).unwrap();
    assert_eq!(
        pending,
        vec![c, merge, tip],
        "root must stay hidden even though it's only reachable from `boundary` via a \
         merge's second parent, and is also a first-parent ancestor of tip via c"
    );
}

#[test]
fn run_applies_a_squash_merged_source_commit_as_a_single_dest_commit() {
    // Regression case, not a fix case: `git merge --squash` never
    // records a second parent, so decisions/0035's
    // `simplify_first_parent()` has no effect on it. Confirms the
    // existing squash-merge shape still syncs unchanged.
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

    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "feature", &[("feature.txt", "line1\n")]);
    let f2 = add_commit(
        &source_repo,
        "feature",
        &[("feature.txt", "line1\nline2\n")],
    );

    // A single-parent squash commit on main, carrying feature's combined
    // final tree with no second parent recorded.
    let f2_commit = source_repo.find_commit(f2).unwrap();
    let graft_commit = source_repo.find_commit(graft).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Squash merge branch 'feature'",
            &f2_commit.tree().unwrap(),
            &[&graft_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("a squash-merged commit should sync");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let feature_blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(feature_blob.content(), b"line1\nline2\n");
}

#[test]
fn run_applies_a_rebased_linear_source_history_commit_by_commit() {
    // Regression case, not a fix case: a rebase-and-fast-forward
    // produces purely linear, single-parent history — decisions/0035's
    // `simplify_first_parent()` has no effect on it. Confirms ordinary
    // linear PR completion still syncs unchanged.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("a.txt", "a\n")]);
    add_commit(&source_repo, "main", &[("b.txt", "b\n")]);
    add_commit(&source_repo, "main", &[("c.txt", "c\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("linear history should sync");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        assert!(tree.get_name(name).is_some(), "{name}");
    }
    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push(dest_tip_commit.id()).unwrap();
    assert_eq!(
        revwalk.count(),
        4,
        "initial commit plus one dest commit per linear source commit"
    );
}

#[test]
fn run_correctly_merges_a_new_source_merge_onto_a_dest_tip_shaped_by_the_old_full_dag_walk() {
    // decisions/0035's migration case: a dest history already containing
    // a prior merge's side-branch commit individually — exactly what
    // the old, full-DAG `pending_commits` would have produced — must
    // still merge correctly once a later merge is processed under
    // first-parent semantics.
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

    // The old merge, already fully processed by a hypothetical old-code
    // sync: R1 on main, F1 on a feature branch, OM merging them with its
    // own extra content so OM's own diff isn't a no-op.
    let r1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);
    let r1_commit = source_repo.find_commit(r1).unwrap();
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let mut builder = source_repo
        .treebuilder(Some(&r1_commit.tree().unwrap()))
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
    let om_extra_blob = source_repo.blob(b"om-extra\n").unwrap();
    builder
        .insert("om-extra.txt", om_extra_blob, git2::FileMode::Blob.into())
        .unwrap();
    let om_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let om = source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &om_tree,
            &[&r1_commit, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    // Hand-build dest's history to match what the old, full-DAG
    // `pending_commits` would actually have produced: R1's own commit,
    // then F1's own commit (the side branch, replayed individually),
    // then OM's own commit (non-empty because of om-extra.txt) — three
    // dest commits, not the one net change the new walk would build.
    let d1 =
        add_source_marker_commit_on_dest(&dest_repo, "main", dest_tip, ("main.txt", "m1\n"), r1);
    let d2 = add_source_marker_commit_on_dest(&dest_repo, "main", d1, ("feature.txt", "f1\n"), f1);
    add_source_marker_commit_on_dest(&dest_repo, "main", d2, ("om-extra.txt", "om-extra\n"), om);

    // A brand-new merge, to be processed under the new first-parent walk
    // against this migrated dest state.
    let r3 = add_commit(&source_repo, "main", &[("main.txt", "m2\n")]);
    source_repo
        .branch("feature2", &source_repo.find_commit(om).unwrap(), false)
        .unwrap();
    let f2 = add_commit(&source_repo, "feature2", &[("feature2.txt", "f2\n")]);
    let r3_commit = source_repo.find_commit(r3).unwrap();
    let f2_commit = source_repo.find_commit(f2).unwrap();
    let mut builder2 = source_repo
        .treebuilder(Some(&r3_commit.tree().unwrap()))
        .unwrap();
    let feature2_entry = f2_commit
        .tree()
        .unwrap()
        .get_name("feature2.txt")
        .unwrap()
        .id();
    builder2
        .insert("feature2.txt", feature2_entry, git2::FileMode::Blob.into())
        .unwrap();
    let m2_tree = source_repo.find_tree(builder2.write().unwrap()).unwrap();
    let signature2 = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature2,
            &signature2,
            "Merge branch 'feature2'",
            &m2_tree,
            &[&r3_commit, &f2_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path())
        .expect("a new merge must apply correctly against a dest tip shaped by the old walk");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    for (name, expected) in [
        ("shared.txt", "v1\n"),
        ("main.txt", "m2\n"),
        ("feature.txt", "f1\n"),
        ("om-extra.txt", "om-extra\n"),
        ("feature2.txt", "f2\n"),
    ] {
        let blob = dest_repo
            .find_blob(tree.get_name(name).unwrap().id())
            .unwrap();
        assert_eq!(blob.content(), expected.as_bytes(), "{name}");
    }
}

#[test]
fn run_does_not_push_dest_originated_content_back_when_a_later_source_commit_follows_it() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let notes_commit = add_commit(&source_repo, "main", &[("notes.txt", "line1\n")]);
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", notes_commit);

    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    let tip_after_first = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // An independent change lands directly on dest.
    add_independent_dest_commit(
        &dest_repo,
        tip_after_first,
        ("dest-only.txt", "x\n"),
        "dest: a merged PR",
    );

    // dest→source cherry-picks it onto source; nothing goes to dest from
    // this run (source has nothing new pending).
    run(source_dir.path(), config.path())
        .expect("second sync (dest->source cherry-pick) should succeed");

    // A later, genuinely new source commit follows the loop-prevented
    // marker commit dest→source just wrote onto source. The cursor must
    // have advanced across that marker commit — otherwise this commit's
    // diff base stays behind it and re-includes dest-only.txt's content,
    // which is already on dest, duplicating it (or failing to apply
    // cleanly).
    add_commit(&source_repo, "main", &[("more.txt", "m\n")]);

    run(source_dir.path(), config.path()).expect("third sync should succeed");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_tip_commit.tree().unwrap();
    let more_blob = dest_repo
        .find_blob(tree.get_name("more.txt").unwrap().id())
        .unwrap();
    assert_eq!(more_blob.content(), b"m\n");
    let dest_only_blob = dest_repo
        .find_blob(tree.get_name("dest-only.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        dest_only_blob.content(),
        b"x\n",
        "dest-originated content must not be duplicated back onto dest \
         (pre-fix risk: b\"x\\nx\\n\")"
    );
    let notes_blob = dest_repo
        .find_blob(tree.get_name("notes.txt").unwrap().id())
        .unwrap();
    assert_eq!(notes_blob.content(), b"line1\n");

    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push_head().unwrap();
    assert_eq!(
        revwalk.count(),
        4,
        "dest history must be exactly: initial, notes.txt, the independent dest-only.txt \
         commit, more.txt"
    );
}

#[test]
fn sync_pair_to_dest_hard_stops_on_a_real_conflict() {
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
    let conflicting_source_commit = add_commit(
        &source_repo,
        "main",
        &[("shared.txt", "line one changed by source\n")],
    );

    // An independent, conflicting dest-side change to the same file —
    // genuinely disagrees with what source did to the same content.
    let dest_conflict_tip = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "line one changed by dest\n"),
        "an independent, conflicting dest-side change",
    );

    // Stamp a marker commit on source claiming dest→source already
    // accounted for dest_conflict_tip, so `dest_resume_point`'s own
    // boundary-recognition (covered elsewhere) doesn't get in the way of
    // this test, which targets source→dest's *apply* conflict handling
    // specifically. Same tree as the tip above it — a pure marker, no
    // content change of its own.
    add_dest_marker_commit(
        &source_repo,
        "main",
        conflicting_source_commit,
        dest_conflict_tip,
    );

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let err = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        branch,
        &reporter,
        &mut dest_ref_cache,
    )
    .expect_err("a real same-file conflict must hard-stop, not silently resolve either side");
    let message = format!("{err:#}");
    assert!(message.contains(&conflicting_source_commit.to_string()));
    assert!(message.contains("resolve"));
    assert!(message.contains("shared.txt"));

    // Nothing must have been pushed to dest at all.
    let still_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        still_dest_tip.id(),
        dest_conflict_tip,
        "a conflicting source commit must not be partially applied or pushed"
    );
}

#[test]
fn sync_pair_to_dest_warns_about_a_mirror_only_branch_with_no_shared_history() {
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

    // `ai-setup`: a genuinely unrelated local branch, a root commit with
    // no parents and no shared history with `main` at all — decisions
    // /0021's own example of a stray, untouched branch never touched by
    // `gitprism setup`.
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let tree = source_repo
        .find_tree(empty_tree(&source_repo).unwrap())
        .unwrap();
    source_repo
        .commit(
            Some("refs/heads/ai-setup"),
            &signature,
            &signature,
            "a stray pre-existing branch",
            &tree,
            &[],
        )
        .unwrap();

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
        "ai-setup",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a mirror-only branch with no shared history must warn, not hard-stop the run");

    // Nothing must have been pushed to dest for this branch at all.
    assert!(
        dest_repo
            .find_branch("ai-setup", git2::BranchType::Local)
            .is_err(),
        "a branch with no shared history must never be pushed to dest"
    );
}

#[test]
fn sync_pair_to_dest_carries_a_rename_and_keeps_dests_own_edit_to_the_renamed_file() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(
        dest_dir.path(),
        "main",
        &[("old.txt", "line one\nline two\nline three\n")],
    );

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let rename_commit = add_commit_removing(
        &source_repo,
        "main",
        &["old.txt"],
        &[("new.txt", "line one\nline two\nline three\n")],
    );

    let dest_edit_tip = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("old.txt", "line one\nline two\nline three edited by dest\n"),
        "an independent dest-side edit",
    );

    // Stamp a marker commit on source naming the dest edit (same
    // technique as `sync_pair_to_dest_hard_stops_on_a_real_conflict`) so
    // the boundary logic isn't what's under test here.
    add_dest_marker_commit(&source_repo, "main", rename_commit, dest_edit_tip);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let branch = "main";
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        branch,
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a rename carrying dest's own edit across it must merge cleanly");

    let new_dest_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = new_dest_tip.tree().unwrap();
    assert!(
        tree.get_name("old.txt").is_none(),
        "the renamed-away path must not survive on dest"
    );
    let new_blob = dest_repo
        .find_blob(tree.get_name("new.txt").unwrap().id())
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(new_blob.content()),
        "line one\nline two\nline three edited by dest\n"
    );
}

#[test]
fn both_directions_treat_the_same_independent_change_as_no_conflict() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1\n")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("shared.txt", "v2\n")]);
    let source_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", source_tip);

    let dest_independent = add_independent_dest_commit(
        &dest_repo,
        dest_tip,
        ("shared.txt", "v2\n"),
        "the same one-line fix, made independently on dest",
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
        .expect("an identical independent change must merge cleanly, not conflict");

    let source_remote_repo = Repository::open(source_remote.path()).unwrap();
    let new_source_tip = source_remote_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        new_source_tip
            .message()
            .unwrap()
            .contains(&format!("Gitprism-Dest-Commit: {dest_independent}")),
        "a merge-to-no-op must still get its own marker commit on source"
    );

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
    .expect("a content no-op merge must not be misreported as a conflict");

    let dest_tip_commit = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_tip_commit.id(),
        dest_independent,
        "a content no-op must not push an empty commit to dest"
    );
    let tree = dest_tip_commit.tree().unwrap();
    let shared_blob = dest_repo
        .find_blob(tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(shared_blob.content(), b"v2\n");

    let repo = Repository::open(source_dir.path()).unwrap();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        branch,
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a repeat sync of the same no-op merge must still succeed");
    let dest_tip_after_repeat = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_tip_after_repeat, dest_independent,
        "the pair must not be bricked by a commit that merges to a no-op and so never gets a trailer"
    );
}

#[test]
fn run_never_leaves_an_intermediate_dest_commit_missing_a_file_from_an_interleaved_merge() {
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

    let a1 = add_commit(&source_repo, "main", &[("main.txt", "m1\n")]);
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let f1 = add_commit(&source_repo, "feature", &[("feature.txt", "f1\n")]);

    // A merge commit on main with parents [a1, f1] whose tree carries
    // shared.txt, main.txt, and feature.txt.
    let a1_commit = source_repo.find_commit(a1).unwrap();
    let f1_commit = source_repo.find_commit(f1).unwrap();
    let mut builder = source_repo
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
    let merge_tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "Merge branch 'feature'",
            &merge_tree,
            &[&a1_commit, &f1_commit],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    let dest_tip_id = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    let mut revwalk = dest_repo.revwalk().unwrap();
    revwalk.push(dest_tip_id).unwrap();
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .unwrap();
    let commits: Vec<Oid> = revwalk.collect::<std::result::Result<Vec<_>, _>>().unwrap();
    assert_eq!(
        commits.len(),
        3,
        "dest history must be exactly: initial, one commit for a1, one commit for f1 \
         (the merge commit's own merge is a content no-op, skipped per requirements/0001)"
    );

    let mut seen_so_far: std::collections::HashSet<String> = std::collections::HashSet::new();
    for oid in commits {
        let commit = dest_repo.find_commit(oid).unwrap();
        let names: std::collections::HashSet<String> = commit
            .tree()
            .unwrap()
            .iter()
            .map(|entry| entry.name().unwrap().to_string())
            .collect();
        let dropped: Vec<&String> = seen_so_far.difference(&names).collect();
        assert!(
            dropped.is_empty(),
            "commit {oid} dropped path(s) an earlier commit had: {dropped:?}"
        );
        seen_so_far.extend(names);
    }
}

#[test]
fn run_never_pushes_an_excluded_directory_to_dest() {
    // Guard: this passes both before and after this change — a
    // regression check that decisions/0014's pre-filtering requirement
    // (an excluded directory never reaching dest, and its own history
    // never presenting as a spurious modify/delete conflict) still holds
    // now that source→dest merges through git merge-tree instead of
    // applying its own diff.
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
        .unwrap();

    let secret_blob = source_repo.blob(b"shh").unwrap();
    let mut secrets_builder = source_repo.treebuilder(None).unwrap();
    secrets_builder
        .insert("inner.txt", secret_blob, git2::FileMode::Blob.into())
        .unwrap();
    let secrets_tree = secrets_builder.write().unwrap();

    let mut builder = source_repo
        .treebuilder(Some(&graft_tip.tree().unwrap()))
        .unwrap();
    builder
        .insert("secrets", secrets_tree, git2::FileMode::Tree.into())
        .unwrap();
    let shared_blob = source_repo.blob(b"v2").unwrap();
    builder
        .insert("shared.txt", shared_blob, git2::FileMode::Blob.into())
        .unwrap();
    let ignore_blob = source_repo.blob(b"secrets/\n").unwrap();
    builder
        .insert(exclude::FILENAME, ignore_blob, git2::FileMode::Blob.into())
        .unwrap();
    let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let first_commit_oid = source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "adds an excluded directory",
            &tree,
            &[&graft_tip],
        )
        .unwrap();
    fs::write(source_dir.path().join(exclude::FILENAME), "secrets/\n").unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("first sync should succeed");

    let dest_tip_after_first = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let dest_tree = dest_tip_after_first.tree().unwrap();
    let shared_blob_on_dest = dest_repo
        .find_blob(dest_tree.get_name("shared.txt").unwrap().id())
        .unwrap();
    assert_eq!(shared_blob_on_dest.content(), b"v2");
    assert!(
        dest_tree.get_name("secrets").is_none(),
        "an excluded directory must never reach dest"
    );

    let first_commit = source_repo.find_commit(first_commit_oid).unwrap();
    let revised_secret_blob = source_repo.blob(b"shh, revised").unwrap();
    let mut revised_secrets_builder = source_repo.treebuilder(None).unwrap();
    revised_secrets_builder
        .insert(
            "inner.txt",
            revised_secret_blob,
            git2::FileMode::Blob.into(),
        )
        .unwrap();
    let revised_secrets_tree = revised_secrets_builder.write().unwrap();
    let mut second_builder = source_repo
        .treebuilder(Some(&first_commit.tree().unwrap()))
        .unwrap();
    second_builder
        .insert("secrets", revised_secrets_tree, git2::FileMode::Tree.into())
        .unwrap();
    let second_tree = source_repo
        .find_tree(second_builder.write().unwrap())
        .unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "edits only the excluded path",
            &second_tree,
            &[&first_commit],
        )
        .unwrap();

    run(source_dir.path(), config.path()).expect(
        "a second sync touching only an excluded path must succeed, not hit a spurious \
         modify/delete conflict",
    );

    let dest_tip_after_second = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_tip_after_second,
        dest_tip_after_first.id(),
        "a commit touching only an excluded path must not produce an empty commit on dest"
    );
}

#[test]
fn run_mirrors_an_ad_hoc_source_branch_with_no_config_entry() {
    // decisions/0017's central promise: a branch nobody ran `gitprism
    // setup` for and that appears nowhere in `config.branches` still
    // gets discovered and mirrored to dest — filtered and merge-tree'd
    // exactly like any configured branch — simulating a developer
    // branching off source's main with no setup step of their own.
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
    add_commit(
        &source_repo,
        "feature-x",
        &[
            ("feature.txt", "line1\n"),
            ("secret.txt", "only for source"),
            (exclude::FILENAME, "secret.txt\n"),
        ],
    );

    // "feature-x" appears nowhere here.
    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    let dest_feature_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must be mirrored to dest even with zero config entry for it")
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = dest_feature_tip.tree().unwrap();
    let feature_blob = dest_repo
        .find_blob(tree.get_name("feature.txt").unwrap().id())
        .unwrap();
    assert_eq!(feature_blob.content(), b"line1\n");
    assert!(
        tree.get_name("secret.txt").is_none(),
        "an excluded file must never reach dest, even on a discovered branch"
    );
    assert!(
        tree.get_name(exclude::FILENAME).is_none(),
        ".gitprismignore itself must never reach dest, even on a discovered branch"
    );
}

#[test]
fn run_mirrors_an_ad_hoc_branch_with_no_commits_of_its_own() {
    // decisions/0017: "every branch that exists on source is mirrored to
    // a same-named branch on dest" — including one that's freshly
    // branched off an already-synced tip with no commits of its own yet.
    // `build_pending_dest_tip` finds zero pending commits for a branch
    // like this (its boundary already equals its tip), which must not be
    // mistaken for "nothing to do": the branch itself still doesn't
    // exist on dest and has to be created there.
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
        .branch(
            "feature-empty",
            &source_repo.find_commit(graft).unwrap(),
            false,
        )
        .unwrap();

    // "feature-empty" appears nowhere in config, and carries no commits
    // beyond the graft it was branched from.
    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("sync should succeed");

    dest_repo
        .find_branch("feature-empty", git2::BranchType::Local)
        .expect("feature-empty must be mirrored to dest even with no commits of its own");
}

#[test]
fn sync_pair_to_dest_gives_a_branch_with_no_commits_of_its_own_a_branch_scoped_marker() {
    // decisions/0044 (review finding F-02): `task`, forked from
    // mirror-only `feature` with no commits of its own yet, anchors on
    // `feature`'s own dest tip (decisions/0043). That dest commit
    // carries `feature`'s own marker, not `task`'s — pointing `task`'s
    // dest ref directly at it made every later resync of `task` fail
    // `dest_tip_accounted_for` and bail permanently, even once `task`
    // gained a real commit of its own.
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

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());
    let mut cache = RunCache::default();

    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "feature",
        &reporter,
        &mut cache,
    )
    .expect("feature must mirror to dest");
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut cache,
    )
    .expect("task, with no commits of its own, must still be created on dest");

    let dest_task_tip = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let dest_feature_tip = dest_repo
        .find_branch("feature", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        dest_task_tip.id(),
        dest_feature_tip.id(),
        "task must get its own branch-scoped marker commit, not point directly at \
         feature's own dest commit"
    );
    assert_eq!(
        dest_task_tip.parent_id(0).unwrap(),
        dest_feature_tip.id(),
        "the branch-scoped marker commit must sit directly on top of feature's own dest tip"
    );
    assert_eq!(
        dest_task_tip.tree_id(),
        dest_feature_tip.tree_id(),
        "the branch-scoped marker commit must carry no content change"
    );

    // Second run, task unchanged: must be a genuine no-op — no bail
    // (the F-02 bug), and no new marker commit every run either.
    let mut cache2 = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut cache2,
    )
    .expect("an unchanged resync must succeed, not permanently bail");
    let dest_task_tip_after_second_run = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_task_tip_after_second_run,
        dest_task_tip.id(),
        "an unchanged branch must not gain a new marker commit every run"
    );

    // Third run: task gains a real commit of its own, which must sync
    // normally on top of its own branch-scoped marker.
    add_commit(&source_repo, "task", &[("task.txt", "x\n")]);
    let mut cache3 = RunCache::default();
    sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "task",
        &reporter,
        &mut cache3,
    )
    .expect("task's own real commit must sync normally once it has one");
    let dest_task_tip_after_third_run = dest_repo
        .find_branch("task", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_eq!(
        dest_task_tip_after_third_run.parent_id(0).unwrap(),
        dest_task_tip.id()
    );
    let tree = dest_task_tip_after_third_run.tree().unwrap();
    assert!(tree.get_name("task.txt").is_some());
}

#[test]
fn run_does_not_pull_back_independent_content_from_a_non_configured_branch() {
    // decisions/0017's deliberate asymmetry: dest→source only ever
    // reflects content back for branches named in `config.branches`.
    // Content landing directly on a discovered-but-unconfigured branch's
    // dest mirror must never be pulled back into source — feature
    // branches are transient and never round-trip.
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);
    let source_feature_tip = source_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");

    let dest_feature_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    // Content landing directly on dest's mirror — e.g. someone pushing
    // straight to it — independent of anything gitprism put there.
    let dest_feature_tip_with_independent_content = add_independent_dest_commit_on(
        &dest_repo,
        "feature-x",
        dest_feature_tip,
        ("dest-only.txt", "pushed straight to the mirror"),
        "an independent change on the mirrored feature branch",
    );

    // "feature-x" isn't in config.branches, so dest→source never
    // considers it at all — this run's source→dest half correctly
    // refuses to fast-forward feature-x over dest content it doesn't
    // recognize, the same safety check any configured branch gets
    // (decisions/0009) — expected to surface as a per-branch halt here
    // (decisions/0045: a discovered branch's refusal is never fatal on
    // its own) precisely because nothing will ever bring this branch's
    // dest content back into source to make it recognized; the overall
    // run still fails since a branch halted.
    let err = run(source_dir.path(), config.path())
        .expect_err("a halted branch must still fail the overall run (decisions/0037's precedent)");
    assert!(
        format!("{err:#}").to_lowercase().contains("halted"),
        "the run's own error should mention a halted branch: {err:#}"
    );

    let source_feature_tip_after = source_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        source_feature_tip_after, source_feature_tip,
        "feature-x's independent dest content must never be pulled back into source — \
         dest→source is scoped to config.branches only"
    );

    let dest_feature_tip_after = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_feature_tip_after, dest_feature_tip_with_independent_content,
        "nothing must be pushed to dest for the halted branch"
    );

    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_main_tip_after, dest_tip,
        "a properly configured branch must still be processed when a discovered \
         branch halts (decisions/0045's per-branch, not whole-run, halt)"
    );
}

#[test]
fn run_fails_clearly_when_a_round_tripped_branchs_dest_ref_is_deleted() {
    // decisions/0018, Case 1: a round-tripped branch (config.branches) always
    // has a dest ref — gitprism's own `setup` grafted it — so it going missing
    // is a real error, not a routine "first sync" case. Before the fix,
    // `sync_pair_from_dest` fetched unconditionally and let git's own raw
    // "couldn't find remote ref" error leak through.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

    // git2 refuses to delete a bare repo's own current HEAD branch via the
    // branch API, so move HEAD off "main" first, then delete the ref
    // directly — simulating an operator (or some other process) deleting
    // main on dest.
    dest_repo.set_head("refs/heads/unrelated-head").unwrap();
    dest_repo
        .find_reference("refs/heads/main")
        .unwrap()
        .delete()
        .unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path()).expect_err(
        "a round-tripped branch whose dest ref has vanished must fail clearly, not panic through on git's own raw fetch error",
    );

    let message = format!("{err:#}");
    assert!(
        message.contains("main") && message.contains("out of sync"),
        "the error should be gitprism's own clear, actionable message naming the \
         affected branch and explaining that source/dest are out of sync: {message}"
    );
    assert!(
        !message.contains("git fetch"),
        "the fix must check existence *before* ever attempting the fetch, so the \
         raw git-fetch failure text must never appear: {message}"
    );
}

#[test]
fn run_does_not_resurrect_a_mirror_only_branch_already_merged_and_deleted_on_dest() {
    // decisions/0018, Case 2: a mirror-only branch (not in config.branches)
    // that was mirrored to dest, then merged into a round-tripped branch via
    // an ordinary PR and cleaned up there, must not be blindly recreated —
    // that would undo the cleanup every single run.
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);

    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    // First sync: feature-x is mirrored to dest with no config entry.
    run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
    let dest_feature_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap();

    // Simulate a real PR: feature-x is merged into dest's main via a genuine
    // new commit made directly on dest (not gitprism's own mirrored commit,
    // which would carry a Gitprism-Source-Commit trailer and get
    // loop-prevented) — single-parent, the same shape a squash-merge
    // produces, and deliberately *not* a real two-parent git merge: this
    // test means to exercise decisions/0018's merge-status check in
    // isolation, not decisions/0019's first-parent-only marker scan (see
    // `run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge`
    // for the real-merge case, which decisions/0019 now handles
    // correctly).
    let dest_main_tip_before = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    dest_repo
        .commit(
            Some("refs/heads/main"),
            &merge_signature,
            &merge_signature,
            "Merge branch 'feature-x' into 'main'",
            &dest_feature_tip.tree().unwrap(),
            &[&dest_main_tip_before],
        )
        .unwrap();

    // Second sync: dest→source reflects that merge back into source's main.
    run(source_dir.path(), config.path())
        .expect("second sync should bring the PR merge back into source's main");
    let source_main_tip_after = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        source_main_tip_after
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some(),
        "source's main must now carry feature-x's content via dest→source"
    );

    // dest deletes feature-x as routine post-merge cleanup.
    dest_repo
        .find_reference("refs/heads/feature-x")
        .unwrap()
        .delete()
        .unwrap();

    // Third sync must not recreate feature-x on dest.
    run(source_dir.path(), config.path())
        .expect("third sync should succeed without recreating feature-x");
    assert!(
        dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .is_err(),
        "a mirror-only branch already merged into a round-tripped branch, then \
         deleted on dest, must not be resurrected"
    );
}

#[test]
fn run_warns_about_and_skips_a_mirror_only_branch_with_no_shared_history() {
    // decisions/0024's own Consequences: a properly round-tripped branch
    // and a genuinely unrelated mirror-only branch coexist in one run —
    // the whole run still succeeds, the round-tripped branch's own sync
    // proceeds normally, and the unrelated branch is never created on
    // dest.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    // `ai-setup`: a genuinely unrelated local branch, a root commit with
    // no shared history with `main` at all — decisions/0021's own
    // example of a stray, untouched branch never touched by `gitprism
    // setup`.
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let tree = source_repo
        .find_tree(empty_tree(&source_repo).unwrap())
        .unwrap();
    source_repo
        .commit(
            Some("refs/heads/ai-setup"),
            &signature,
            &signature,
            "a stray pre-existing branch",
            &tree,
            &[],
        )
        .unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);

    run(source_dir.path(), config.path())
        .expect("an unrelated mirror-only branch must be skipped, not abort the whole run");

    // `main` — the round-tripped branch — is unaffected: still at its
    // already-synced tip.
    let dest_main_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_main_tip, dest_tip,
        "main's own sync must proceed normally alongside the skipped branch"
    );

    // `ai-setup` is never created on dest.
    assert!(
        dest_repo
            .find_branch("ai-setup", git2::BranchType::Local)
            .is_err(),
        "a mirror-only branch with no shared history must never be pushed to dest"
    );
}

#[test]
fn run_does_not_resurrect_a_mirror_only_branch_merged_except_for_excluded_paths() {
    // decisions/0018 addendum: `already_merged_into_a_landing_branch` must
    // compare *filtered* trees, not raw source-side ones. A mirror-only
    // branch's commit touching an excluded path (`.gitprismignore`)
    // alongside an ordinary mirrored one is completely normal in a
    // source-is-a-superset repo — dest never receives that excluded path
    // either way, so its presence on `branch` but not on the landing
    // branch must not read as "genuinely unmerged." Before the fix, the
    // raw (unfiltered) tree comparison saw `secret.txt` as content
    // `landing` never received and wrongly concluded "not merged,"
    // resurrecting feature-x on dest every single run.
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

    // `.gitprismignore` excludes secret.txt from ever reaching dest
    // (decisions/0011) — versioned on main like any other source content.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "secret.txt\n")]);
    let main_after_ignore = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    source_repo
        .branch(
            "feature-x",
            &source_repo.find_commit(main_after_ignore).unwrap(),
            false,
        )
        .unwrap();
    // feature-x's own commit touches both a mirrored path and an excluded
    // path together — ordinary in a source-is-a-superset repo, and
    // exactly the shape the pre-fix bug mishandled.
    add_commit(
        &source_repo,
        "feature-x",
        &[
            ("feature.txt", "line1\n"),
            ("secret.txt", "only for source"),
        ],
    );

    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    // First sync: feature-x is mirrored to dest with no config entry —
    // secret.txt is filtered out.
    run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
    let dest_feature_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        dest_feature_tip
            .tree()
            .unwrap()
            .get_name("secret.txt")
            .is_none(),
        "secret.txt must never reach dest"
    );
    refresh_checked_out_branch(&source_repo, "main");

    // Simulate a real PR: feature-x is merged into dest's main via a
    // genuine new commit made directly on dest — single-parent, same
    // squash-shaped stand-in the sibling fixture uses, to isolate
    // decisions/0018's merge-status check from decisions/0019's
    // first-parent-only marker scan.
    let dest_main_tip_before = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    dest_repo
        .commit(
            Some("refs/heads/main"),
            &merge_signature,
            &merge_signature,
            "Merge branch 'feature-x' into 'main'",
            &dest_feature_tip.tree().unwrap(),
            &[&dest_main_tip_before],
        )
        .unwrap();

    // Second sync: dest→source reflects that merge back into source's
    // main — main's own tree still never gains secret.txt, since dest
    // never had it to bring back.
    run(source_dir.path(), config.path())
        .expect("second sync should bring the PR merge back into source's main");

    // dest deletes feature-x as routine post-merge cleanup.
    dest_repo
        .find_reference("refs/heads/feature-x")
        .unwrap()
        .delete()
        .unwrap();

    // Third sync must not recreate feature-x on dest, even though
    // feature-x's raw source-side tree carries secret.txt — an excluded
    // path main's tree never received and never will.
    run(source_dir.path(), config.path())
        .expect("third sync should succeed without recreating feature-x");
    assert!(
        dest_repo
            .find_branch("feature-x", git2::BranchType::Local)
            .is_err(),
        "a mirror-only branch already merged into a round-tripped branch (modulo \
         excluded paths dest never receives), then deleted on dest, must not be \
         resurrected"
    );
}

#[test]
fn run_still_recreates_a_mirror_only_branch_with_genuinely_unmerged_content() {
    // decisions/0018, Case 2's fall-through: a mirror-only branch whose dest
    // ref is missing but whose content is only *partially* present in a
    // landing branch (e.g. resumed work after a squash merge that only
    // captured part of it) must still be rebuilt and pushed normally, not
    // mistaken for "already merged and cleaned up."
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);
    add_commit(&source_repo, "feature-x", &[("extra.txt", "line2\n")]);
    let feature_tip = source_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();

    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    // First sync: feature-x (both commits) is mirrored to dest.
    run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
    dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync");

    // A squash merge onto dest's main that only captures feature.txt, not
    // extra.txt — e.g. the PR was merged before the branch's second commit
    // was pushed.
    let dest_main_tip_before = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut builder = dest_repo
        .treebuilder(Some(&dest_main_tip_before.tree().unwrap()))
        .unwrap();
    let blob = dest_repo.blob(b"line1\n").unwrap();
    builder
        .insert("feature.txt", blob, git2::FileMode::Blob.into())
        .unwrap();
    let squash_tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
    let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    // Single-parent, same reasoning as the sibling test above: this test
    // means to exercise decisions/0018's merge-status check (a genuinely
    // unmerged remainder must still be recreated) in isolation from
    // decisions/0019's first-parent-only marker scan, which a real
    // two-parent second parent here would also exercise.
    dest_repo
        .commit(
            Some("refs/heads/main"),
            &merge_signature,
            &merge_signature,
            "Merge branch 'feature-x' into 'main' (squash)",
            &squash_tree,
            &[&dest_main_tip_before],
        )
        .unwrap();

    run(source_dir.path(), config.path())
        .expect("second sync should bring the squash merge back into source's main");

    // dest deletes feature-x, believing it fully merged (only part of it
    // actually was).
    dest_repo
        .find_reference("refs/heads/feature-x")
        .unwrap()
        .delete()
        .unwrap();

    // Third sync: feature-x's tip still carries extra.txt, which main does
    // not have — not a no-op merge, so feature-x must be recreated on dest.
    run(source_dir.path(), config.path())
        .expect("third sync should succeed and recreate feature-x");
    let recreated = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must be recreated on dest: its content isn't fully merged into main yet")
        .get()
        .peel_to_commit()
        .unwrap();
    let tree = recreated.tree().unwrap();
    assert!(tree.get_name("feature.txt").is_some());
    assert!(
        tree.get_name("extra.txt").is_some(),
        "the genuinely unmerged remainder must reach dest"
    );
    assert_eq!(
        recreated.tree().unwrap().id(),
        feature_tip.tree().unwrap().id(),
        "the recreated mirror must match feature-x's own current content"
    );
}

#[test]
fn run_ignores_a_merged_in_branchs_own_trailer_when_resuming_after_a_real_merge() {
    // decisions/0019: a real, two-parent merge of a mirror-only branch
    // into a round-tripped branch on dest must not let the round-tripped
    // branch's own resume-point scan (`newest_source_marker`) cross into
    // the merged-in branch's own `Gitprism-Source-Commit` trailer via the
    // merge's second parent. Unlike decisions/0018's own Case 2 fixture
    // (which deliberately used a single-parent, squash-shaped stand-in to
    // avoid exactly this — see its comment and design/log.md), this test
    // performs the real thing: main stays first parent, feature-x's own
    // gitprism-authored mirror commit is the second — the ordinary shape
    // GitHub's/GitLab's "merge pull request" button, or `git merge`
    // run from the checked-out target branch, both produce.
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
    add_commit(&source_repo, "feature-x", &[("feature.txt", "line1\n")]);

    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    // First sync: feature-x is mirrored to dest with no config entry —
    // its dest tip carries gitprism's own Gitprism-Source-Commit trailer.
    run(source_dir.path(), config.path()).expect("first sync should mirror feature-x to dest");
    let dest_feature_tip = dest_repo
        .find_branch("feature-x", git2::BranchType::Local)
        .expect("feature-x must exist on dest after the first sync")
        .get()
        .peel_to_commit()
        .unwrap();

    // A real PR merge: main stays first parent, feature-x's own gitprism
    // mirror commit (carrying its own trailer) is the second parent —
    // deliberately the shape decisions/0018's own fixtures avoided.
    let dest_main_tip_before = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let merge_signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
    let merge_commit = dest_repo
        .commit(
            Some("refs/heads/main"),
            &merge_signature,
            &merge_signature,
            "Merge branch 'feature-x' into 'main'",
            &dest_feature_tip.tree().unwrap(),
            &[&dest_main_tip_before, &dest_feature_tip],
        )
        .unwrap();

    // Second sync: dest→source reflects the merge back into source's
    // main, then source→dest's own resume-point scan for main must not
    // be confused by feature-x's own trailer, reachable via the merge's
    // second parent. Before decisions/0019's fix, this fails with a false
    // "isn't at a point this clone can safely build on" refusal, since
    // `newest_source_marker`'s full-ancestry walk reaches feature-x's own
    // marker before main's, and main's source tip isn't a descendant of
    // that unrelated oid.
    run(source_dir.path(), config.path()).expect(
        "second sync must not mistake feature-x's own merged-in trailer for main's own resume point",
    );

    let source_main_tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        source_main_tip
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some(),
        "source's main must carry feature-x's content via dest→source"
    );

    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(
        dest_main_tip_after, merge_commit,
        "dest's main already carries everything source has (via the real merge); a \
         correct resume must find nothing new to push, leaving dest's tip unmoved"
    );
}

#[test]
fn divergence_after_exhausted_retries_message_names_the_branch_says_diverged_and_never_prescribes_a_reconciliation_method()
 {
    for ff_target in ["dest", "source"] {
        let message = divergence_after_exhausted_retries_message("main", ff_target);
        assert!(
            message.contains("\"main\""),
            "must name the branch: {message}"
        );
        assert!(
            message.contains("diverged"),
            "must say the histories diverged: {message}"
        );
        assert!(
            message.contains("ordinary git"),
            "must hand reconciliation to the operator: {message}"
        );
        let lower = message.to_lowercase();
        for method in ["merge", "rebase", "cherry-pick", "cherry pick"] {
            assert!(
                !lower.contains(method),
                "must not prescribe a specific reconciliation method ({method}): {message}"
            );
        }
    }
}

#[test]
fn mirror_only_skip_note_names_the_landing_branch_and_asserts_no_deletion_or_prior_existence() {
    let note = mirror_only_skip_note("main");
    assert!(
        note.contains("\"main\""),
        "must name the landing branch: {note}"
    );
    let lower = note.to_lowercase();
    for claim in ["delet", "clean", "existed", "existing", "recreat"] {
        assert!(
            !lower.contains(claim),
            "must not claim deletion, cleanup, or prior existence on dest ({claim}): {note}"
        );
    }
}

#[test]
fn unsafe_to_build_on_message_names_the_branch_and_never_claims_dest_to_source_could_apply() {
    let message = unsafe_to_build_on_message("hotfix");
    assert!(
        message.contains("\"hotfix\""),
        "must name the branch: {message}"
    );
    assert!(
        !message.to_lowercase().contains("hasn't reflected"),
        "a discovered (mirror-only) branch's refusal must never claim dest→source could \
         have applied — dest→source never runs for it: {message}"
    );
}
