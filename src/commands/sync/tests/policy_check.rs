use super::*;

#[test]
fn sync_pair_to_dest_halts_a_branch_whose_replayed_commit_has_a_differing_gitprismignore() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    // A replayed commit whose own .gitprismignore differs from what's
    // currently pinned — main's later commit (and, since checkout keeps
    // the working tree at HEAD, the working tree gitprism actually reads
    // the policy from) sets it to something else.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a policy mismatch halts the branch, it must not error the whole call");
    assert!(
        halted,
        "a replayed commit's differing .gitprismignore must halt this branch"
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
        "nothing must be pushed to dest for a branch that halts on a policy mismatch"
    );
}

#[test]
fn sync_pair_to_dest_replays_a_commit_with_a_differing_gitprism_toml_normally() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    // Two pending commits, neither yet synced to dest, each setting a
    // different .gitprism.toml — the setup-iteration shape: an operator
    // edits the config more than once before the first successful sync.
    // Unlike .gitprismignore, .gitprism.toml is never consulted per
    // replayed commit (decisions/0026's config is loaded exactly once,
    // globally) and is self-excluded from dest, so an earlier pending
    // commit's differing bytes can't leak anything the later commit
    // didn't already govern — decisions/0037's amendment: only
    // .gitprismignore is checked per pending commit.
    add_commit_bytes(
        &source_repo,
        "main",
        &[(crate::config::FILENAME, b"old config\n")],
    );
    add_commit_bytes(
        &source_repo,
        "main",
        &[
            (crate::config::FILENAME, b"new config\n"),
            ("normal.txt", b"content\n"),
        ],
    );

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a differing .gitprism.toml must not error");
    assert!(
        !halted,
        "a replayed commit's differing .gitprism.toml must never halt the branch"
    );

    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        dest_main_tip_after
            .tree()
            .unwrap()
            .get_name("normal.txt")
            .is_some(),
        "ordinary content alongside a differing .gitprism.toml must still reach dest"
    );
}

#[test]
fn sync_pair_to_dest_replays_a_commit_with_no_control_file_at_all_normally() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("normal.txt", "content\n")]);

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a commit with no control file at all must not error");
    assert!(
        !halted,
        "a replayed commit carrying no control file at all must never halt the branch"
    );

    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        dest_main_tip_after
            .tree()
            .unwrap()
            .get_name("normal.txt")
            .is_some(),
        "a commit carrying no control file must still be pushed to dest"
    );
}

#[test]
fn sync_pair_to_dest_syncs_normally_when_a_replayed_commits_control_file_matches_the_pin() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    // Two pending commits, each explicitly setting the same
    // .gitprismignore content — present, and byte-for-byte identical to
    // what ends up pinned (main's own tip, mirrored to the working
    // tree), at both of them.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
    add_commit(
        &source_repo,
        "main",
        &[("other.txt", "content\n"), (exclude::FILENAME, "*.log\n")],
    );

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "main",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a control file matching the pin must not error");
    assert!(
        !halted,
        "a replayed commit whose control file matches the pinned policy byte-for-byte \
         must not halt the branch"
    );

    let dest_main_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert!(
        dest_main_tip_after
            .tree()
            .unwrap()
            .get_name("other.txt")
            .is_some(),
        "a branch whose control file matches the pin must still sync its ordinary content"
    );
}

#[test]
fn run_continues_other_branches_and_fails_overall_when_one_branch_halts_for_a_policy_mismatch() {
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

    // A healthy mirror-only branch with no control files at all — must
    // still sync even though "main" (sorted before it) is about to
    // halt.
    source_repo
        .branch("other", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

    // "main" gets a replayed commit whose .gitprismignore disagrees with
    // what main's own tip (and therefore the checked-out working tree)
    // ends up pinning.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path())
        .expect_err("a policy mismatch on one branch must fail the overall run");
    assert!(
        format!("{err:#}").to_lowercase().contains("polic"),
        "the run's own error should mention the policy mismatch: {err:#}"
    );

    assert!(
        dest_repo
            .find_branch("other", git2::BranchType::Local)
            .is_ok(),
        "other branches must still sync when one branch halts for a policy mismatch"
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
        "nothing must be pushed to dest for the halted branch"
    );
}

#[test]
fn sync_pair_to_dest_halts_a_control_file_only_branch_instead_of_classifying_it_already_merged() {
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

    // "main" pins the approved policy via its own tip content, mirrored
    // to the checked-out working tree.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.secret\n")]);

    // "policy-update": the sanctioned GITPRISM_POLICY_SHA256-change
    // workflow (decisions/0037's own Context) — a branch whose sole diff
    // from "main" is a *different*, not-yet-approved .gitprismignore.
    // `add_commit_bytes` (unlike `add_commit`) never writes straight to
    // the working tree, so this doesn't clobber the pin main just set.
    source_repo
        .branch(
            "policy-update",
            &source_repo.find_commit(graft).unwrap(),
            false,
        )
        .unwrap();
    add_commit_bytes(
        &source_repo,
        "policy-update",
        &[(exclude::FILENAME, b"*.secret\n*.log\n")],
    );

    let config = Config::load(
        write_config("unused", &dest_dir.path().display().to_string(), &["main"]).path(),
    )
    .unwrap();
    let repo = Repository::open(source_dir.path()).unwrap();
    let reporter = Reporter::new(1, std::iter::empty());

    let mut dest_ref_cache = RunCache::default();
    let halted = sync_pair_to_dest(
        &repo,
        source_dir.path(),
        &config,
        "policy-update",
        &reporter,
        &mut dest_ref_cache,
    )
    .expect("a policy mismatch halts the branch, it must not error the whole call");
    assert!(
        halted,
        "a control-file-only branch that differs from the pinned policy must halt, not be \
         classified already-merged-and-cleaned-up"
    );

    assert!(
        dest_repo
            .find_branch("policy-update", git2::BranchType::Local)
            .is_err(),
        "a halted branch must never be pushed to dest"
    );
}

#[test]
fn run_continues_other_branches_and_fails_overall_when_one_branchs_control_file_is_a_directory() {
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

    // A healthy mirror-only branch with no control files at all — must
    // still sync even though "main" (sorted before it) is about to halt.
    source_repo
        .branch("other", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

    // "main" pins a normal policy via its own tip content, then a pending
    // commit replaces the control file with a directory — unreadable as a
    // blob at all, let alone comparable to the pin.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
    add_commit_with_gitprismignore_as_a_directory(&source_repo, "main");

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path()).expect_err(
        "a branch whose pending commit turns the control file into a directory must fail the overall run",
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("polic"),
        "the run's own error should mention the policy mismatch: {err:#}"
    );

    assert!(
        dest_repo
            .find_branch("other", git2::BranchType::Local)
            .is_ok(),
        "other branches must still sync when one branch halts because its control file isn't a regular file"
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
        "nothing must be pushed to dest for the halted branch"
    );
}

#[test]
fn run_continues_other_branches_and_fails_overall_when_one_branchs_control_file_exceeds_the_size_limit()
 {
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

    // A healthy mirror-only branch with no control files at all — must
    // still sync even though "main" (sorted before it) is about to halt.
    source_repo
        .branch("other", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    add_commit(&source_repo, "other", &[("other.txt", "content\n")]);

    // "main" pins a normal policy via its own tip content, then a pending
    // commit grows the control file past the byte limit.
    add_commit(&source_repo, "main", &[(exclude::FILENAME, "*.log\n")]);
    add_commit_with_oversized_gitprismignore(&source_repo, "main");

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let err = run(source_dir.path(), config.path()).expect_err(
        "a branch whose pending commit's control file exceeds the size limit must fail the overall run",
    );
    assert!(
        format!("{err:#}").to_lowercase().contains("polic"),
        "the run's own error should mention the policy mismatch: {err:#}"
    );

    assert!(
        dest_repo
            .find_branch("other", git2::BranchType::Local)
            .is_ok(),
        "other branches must still sync when one branch halts because its control file exceeds the size limit"
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
        "nothing must be pushed to dest for the halted branch"
    );
}

#[test]
fn policy_mismatch_message_states_the_actual_reason_for_each_variant() {
    let commit = Oid::from_str("0000000000000000000000000000000000000abc").unwrap();

    let differs = policy_mismatch_message(
        "main",
        &PolicyMismatch {
            commit,
            filename: exclude::FILENAME,
            reason: PolicyMismatchReason::DiffersFromPinnedPolicy,
        },
    );
    assert!(
        differs.contains("\"main\""),
        "must name the branch: {differs}"
    );
    assert!(
        differs.contains(&format!("{commit}")),
        "must name the commit: {differs}"
    );
    assert!(
        differs.contains("that differs from the pinned policy"),
        "existing differing-bytes wording must survive unchanged: {differs}"
    );

    let not_regular = policy_mismatch_message(
        "main",
        &PolicyMismatch {
            commit,
            filename: exclude::FILENAME,
            reason: PolicyMismatchReason::NotARegularFile,
        },
    );
    assert!(
        not_regular.contains("is not a regular file"),
        "must state the entry could not be read as a control file: {not_regular}"
    );
    assert!(
        !not_regular.contains("differs from the pinned policy"),
        "must not claim a byte comparison that never happened: {not_regular}"
    );

    let too_large = policy_mismatch_message(
        "main",
        &PolicyMismatch {
            commit,
            filename: exclude::FILENAME,
            reason: PolicyMismatchReason::ExceedsSizeLimit,
        },
    );
    assert!(
        too_large.contains("exceeds"),
        "must state the size limit was exceeded: {too_large}"
    );
    assert!(
        !too_large.contains("differs from the pinned policy"),
        "must not claim a byte comparison that never happened: {too_large}"
    );

    for message in [&differs, &not_regular, &too_large] {
        assert!(
            message.contains("GITPRISM_POLICY_SHA256"),
            "every variant must still point to the remedy: {message}"
        );
    }
}
