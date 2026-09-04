use super::*;

#[test]
fn run_reports_the_repository_error_for_a_wrong_cwd_not_the_state_key_error() {
    // F-14: repository discovery must run before state-key validation,
    // so a wrong cwd reports "not a git repository," never a state-key
    // complaint (`marker::load_key` uses a fixed test key and can't
    // itself fail here, but the ordering this guards is the same either
    // way — see the F-14 report note in this commit).
    let dir = tempdir().unwrap();

    let error = run(dir.path(), Path::new(".gitprism.toml"))
        .expect_err("running outside any git repository must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("must be run inside an existing git repository"),
        "expected the repository-discovery error, got: {message}"
    );
    assert!(
        !message.to_uppercase().contains("GITPRISM_STATE_KEY"),
        "the repository error must not be shadowed by a state-key complaint: {message}"
    );
}

#[test]
fn run_succeeds_without_a_source_url_when_nothing_needs_pushing_to_source() {
    let _guard = crate::config::ENV_VAR_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("GITPRISM_SOURCE_URL");
    }

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
    // Something for source→dest to do, but dest never advances
    // independently — dest→source has nothing to push this run.
    add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

    // No [source] section at all, and GITPRISM_SOURCE_URL unset — valid
    // per decisions/0013 as long as nothing actually needs it.
    let mut config_file = NamedTempFile::new().unwrap();
    write!(
        config_file,
        r#"
        branches = ["main"]

        [committer]
        name = "gitprism"
        email = "gitprism@example.com"

        [dest]
        url = '{}'
        "#,
        dest_dir.path().display()
    )
    .unwrap();

    run(source_dir.path(), config_file.path())
        .expect("a source→dest-only run must not fail merely for lacking a source URL");
}

#[test]
fn run_rejects_a_bare_repo_as_cwd_with_a_clear_message() {
    let dir = tempdir().unwrap();
    Repository::init_bare(dir.path()).unwrap();

    let config = write_config("unused", "unused", &["main"]);
    let error = run(dir.path(), config.path())
        .expect_err("a bare repo has no working tree for sync to operate in");
    assert!(
        error
            .to_string()
            .contains("requires a repo with a working tree, not a bare repo"),
        "expected the bare-repo rejection, got: {error}"
    );
}

#[test]
fn run_syncs_normally_when_source_is_in_a_detached_head_state() {
    // No code anywhere consults HEAD to decide which branch to operate
    // on (branches are always named explicitly, from config or
    // discovery) — this pins today's actual behavior, that a detached
    // HEAD in the invoking repo is not special-cased at all and simply
    // does not get in the way.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    source_repo.set_head_detached(tip).unwrap();
    assert!(source_repo.head_detached().unwrap());

    add_commit(&source_repo, "main", &[("shared.txt", "v2")]);
    assert!(
        source_repo.head_detached().unwrap(),
        "committing onto the named branch ref must not itself reattach HEAD"
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path())
        .expect("a detached HEAD in the invoking repo must not block sync");

    let dest_new_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(
        dest_new_tip.id(),
        dest_tip,
        "dest must still advance despite source's detached HEAD"
    );
}

#[test]
fn run_from_a_linked_worktree_succeeds_and_locks_the_common_directory() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    add_commit(&source_repo, "main", &[("shared.txt", "v2")]);
    let tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();

    // A detached linked worktree (same primitive `resolve` uses,
    // decisions/0027) rather than a new branch, so branch discovery
    // below still sees only "main" — the point here is exercising `cwd`
    // resolution and lock placement, not adding an incidental second
    // branch for source→dest to mirror.
    let worktree_dir = tempdir().unwrap();
    git::worktree_add(source_dir.path(), worktree_dir.path(), tip).unwrap();

    let common_dir = Repository::discover(worktree_dir.path())
        .unwrap()
        .commondir()
        .to_path_buf();
    assert_eq!(
        common_dir,
        Repository::discover(source_dir.path())
            .unwrap()
            .commondir()
            .to_path_buf(),
        "a linked worktree must share the main worktree's common directory"
    );

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(worktree_dir.path(), config.path())
        .expect("sync must succeed when invoked from a linked worktree");

    let dest_new_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    assert_ne!(dest_new_tip.id(), dest_tip, "dest must still advance");

    assert!(
        common_dir.join("gitprism.lock").exists(),
        "the operation lock must live in the shared common directory"
    );
}

#[test]
fn run_preserves_a_registered_submodule_gitlink_through_source_to_dest_filtering() {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let empty_tree = source_repo
        .find_tree(source_repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let submodule_commit = source_repo
        .commit(
            None,
            &signature,
            &signature,
            "submodule commit",
            &empty_tree,
            &[],
        )
        .unwrap();

    let tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let gitmodules_blob = source_repo
        .blob(b"[submodule \"vendor\"]\n\tpath = vendor\n\turl = https://example.invalid/vendor.git\n")
        .unwrap();
    let mut builder = source_repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
    builder
        .insert(".gitmodules", gitmodules_blob, git2::FileMode::Blob.into())
        .unwrap();
    builder
        .insert("vendor", submodule_commit, git2::FileMode::Commit.into())
        .unwrap();
    let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "add a submodule",
            &tree,
            &[&tip],
        )
        .unwrap();
    refresh_checked_out_branch(&source_repo, "main");

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path())
        .expect("a submodule gitlink must pass through source→dest filtering, not fail it");

    let dest_new_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let dest_tree = dest_new_tip.tree().unwrap();
    let entry = dest_tree
        .get_name("vendor")
        .expect("the submodule gitlink must reach dest, not be dropped");
    assert_eq!(
        entry.filemode(),
        i32::from(git2::FileMode::Commit),
        "the gitlink's filemode (160000) must survive unchanged"
    );
    assert_eq!(
        entry.id(),
        submodule_commit,
        "the gitlink must still point at the exact same submodule commit, \
         proving it was copied through opaquely rather than traversed as a tree"
    );
}

#[test]
fn run_preserves_an_unregistered_nested_repository_gitlink_without_a_gitmodules_file() {
    // Unlike the registered-submodule test above, this tree has no
    // `.gitmodules` at all — just a bare gitlink entry, the shape git
    // itself produces automatically for any directory that happens to
    // contain its own `.git` (a nested repo checked in ad hoc, not a
    // deliberately registered submodule). gitprism's gitlink handling is
    // a structural filemode check, not a `.gitmodules` lookup, so this
    // must be preserved identically.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);

    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let empty_tree = source_repo
        .find_tree(source_repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let nested_repo_commit = source_repo
        .commit(
            None,
            &signature,
            &signature,
            "nested repo's own commit",
            &empty_tree,
            &[],
        )
        .unwrap();

    let tip = source_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let mut builder = source_repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
    builder
        .insert(
            "nested-repo",
            nested_repo_commit,
            git2::FileMode::Commit.into(),
        )
        .unwrap();
    let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "add a nested checkout",
            &tree,
            &[&tip],
        )
        .unwrap();
    refresh_checked_out_branch(&source_repo, "main");

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    run(source_dir.path(), config.path()).expect(
        "an unregistered nested-repo gitlink must pass through filtering just like a \
         registered submodule's, not be traversed or dropped",
    );

    let dest_new_tip = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap();
    let entry = dest_new_tip
        .tree()
        .unwrap()
        .get_name("nested-repo")
        .expect("the nested repo's gitlink must reach dest, not be dropped")
        .to_owned();
    assert_eq!(entry.filemode(), i32::from(git2::FileMode::Commit));
    assert_eq!(entry.id(), nested_repo_commit);
}

#[test]
fn run_fails_clearly_against_a_completely_empty_dest_repo() {
    // Documents today's actual behavior for a zero-commit, no-refs dest:
    // `sync` never seeds dest itself (that's `setup`'s job), and a
    // round-tripped branch with no ref on dest at all is reported the
    // same way as one whose ref was deleted after a real setup —
    // `remote_ref_exists` can't tell "never existed" apart from
    // "existed, then vanished."
    let dest_dir = tempdir().unwrap();
    Repository::init_bare(dest_dir.path()).unwrap();

    let source_dir = tempdir().unwrap();
    let source_repo = Repository::init(source_dir.path()).unwrap();
    let tree = source_repo
        .find_tree(source_repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    source_repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "initial",
            &tree,
            &[],
        )
        .unwrap();
    source_repo.set_head("refs/heads/main").unwrap();
    source_repo.checkout_head(None).unwrap();

    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let error = run(source_dir.path(), config.path())
        .expect_err("a round-tripped branch missing from a completely empty dest must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("has no ref on dest anymore"),
        "expected the missing-dest-ref rejection, got: {message}"
    );
}

#[test]
// A genuine `--depth=1` clone of a mirror-only branch, run through `run()`
// itself rather than the `sync_pair_to_dest` wrapper — proves in the actual
// production entry point what design/log.md's 2026-08-31 investigation
// established directly: a fresh `Repository::discover` walks a shallow
// clone cleanly, so this reaches decisions/0045's per-branch halt, never a
// `git2::Error` from mapping reconstruction.
fn run_halts_a_mirror_only_branch_through_a_genuine_depth_1_clone_without_a_git2_error() {
    let (dest_dir, dest_repo, _source_dir, repo, graft, _config) =
        mirror_only_feature_branch_synced_once();

    // Amend `feature-x` in place, same as the shallow wrapper-level test:
    // dest's marker for it now names a commit this clone will never fetch.
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

    // No round-tripped branches: a real CI checkout of a mirror-only branch
    // fetches only that branch, so `main` never exists in this clone —
    // `config.branches` must be empty or `run()`'s dest→source phase would
    // fail outright on a branch this clone never has, before source→dest
    // ever gets a turn.
    let config = write_config("unused", &dest_dir.path().display().to_string(), &[]);

    let error = run(fresh_dir.path(), config.path()).expect_err(
        "a shallow clone's missing boundary object must halt this branch, not silently mirror it",
    );
    let message = format!("{error:#}");
    assert!(
        message.contains("one or more branches halted"),
        "expected decisions/0045's aggregate halted-branch error, got: {message}"
    );
    assert!(
        error
            .chain()
            .all(|cause| cause.downcast_ref::<git2::Error>().is_none()),
        "a correctly refused branch must never surface a git2::Error in its chain: {error:#}"
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
fn list_source_branches_lists_every_local_branch_sorted() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let root = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "root",
            &tree,
            &[],
        )
        .unwrap();
    let root_commit = repo.find_commit(root).unwrap();
    // Created out of alphabetical order — asserting a specific sorted
    // order below pins the sorted-by-construction guarantee explicitly
    // (an explicit `.sort()`, not an assumption about git2's own
    // iteration order, which its docs don't promise) even though this
    // machine's libgit2 happens to already iterate loose refs
    // alphabetically.
    repo.branch("zeta", &root_commit, false).unwrap();
    repo.branch("alpha", &root_commit, false).unwrap();

    let (branches, skipped) =
        list_source_branches(&repo).expect("listing source's local branches should succeed");

    assert_eq!(
        branches,
        vec!["alpha".to_string(), "main".to_string(), "zeta".to_string()],
        "every local branch must be listed, sorted for deterministic run order"
    );
    assert!(skipped.is_empty());
}

#[cfg(unix)]
#[test]
fn list_source_branches_warns_about_and_skips_a_non_utf8_branch_name() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let root = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "root",
            &tree,
            &[],
        )
        .unwrap();

    // git2's safe `Repository::branch`/`reference` API requires a valid
    // `&str` name, and even a raw loose-ref *filename* with invalid
    // UTF-8 bytes is rejected by some filesystems (macOS's among them).
    // `packed-refs` sidesteps both: it's one ordinarily-named file whose
    // *content* — where the ref name lives, not the path — a
    // filesystem never validates as text, giving the same genuinely
    // non-UTF-8 ref a real `git gc --aggressive` run can produce
    // (decisions/0024's precedent is for exactly this kind of
    // operator-controlled, but gitprism-unreadable, oddity).
    let mut packed_refs = Vec::new();
    packed_refs.extend_from_slice(b"# pack-refs with: peeled fully-peeled sorted\n");
    packed_refs.extend_from_slice(root.to_string().as_bytes());
    packed_refs.push(b' ');
    packed_refs.extend_from_slice(b"refs/heads/feature-");
    packed_refs.extend_from_slice(&[0xFF, 0xFE]);
    packed_refs.push(b'\n');
    std::fs::write(repo.path().join("packed-refs"), packed_refs).unwrap();

    let (branches, skipped) = list_source_branches(&repo)
        .expect("a non-UTF-8 branch name must not fail the whole listing");

    assert_eq!(
        branches,
        vec!["main".to_string()],
        "the non-UTF-8 branch must be skipped, not silently mirrored under some other name"
    );
    assert_eq!(
        skipped.len(),
        1,
        "the non-UTF-8 branch must be reported as skipped"
    );
    assert!(
        skipped[0].reason.contains("non-UTF-8"),
        "the skip reason must explain why: {}",
        skipped[0].reason
    );
}

// TEST-001: the env-reading entry point must actually enforce
// `GITPRISM_STATE_KEY`/`GITPRISM_POLICY_SHA256`, refusing before any fetch
// or ref mutation. `marker::load_key`/`policy::verify_expected_digest` are
// cfg(test)-forked to always succeed today, so these three fail until
// TEST-001 step 2 removes those forks.

fn clear_pin_env_vars() {
    unsafe {
        std::env::remove_var("GITPRISM_STATE_KEY");
        std::env::remove_var("GITPRISM_POLICY_SHA256");
    }
}

#[test]
#[ignore = "TEST-001 step 2: policy::verify_expected_digest/marker::load_key are cfg(test)-forked to always succeed until the forks are removed"]
fn run_refuses_without_a_state_key_env_var_and_leaves_no_side_effects() {
    let _guard = crate::config::ENV_VAR_LOCK.lock().unwrap();
    clear_pin_env_vars();

    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

    let source_dir = tempdir().unwrap();
    source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let expected_digest = crate::policy::hash_files(
        config.path(),
        &source_dir.path().join(crate::exclude::FILENAME),
    )
    .unwrap();
    unsafe {
        std::env::set_var("GITPRISM_POLICY_SHA256", &expected_digest);
    }

    let error = run(source_dir.path(), config.path())
        .expect_err("an unset state key must refuse before any fetch or mutation");
    assert!(
        error.to_string().contains("GITPRISM_STATE_KEY"),
        "error must name the missing variable: {error}"
    );

    let dest_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(dest_tip_after, dest_tip, "dest's refs must be unchanged");
    assert!(
        !source_dir.path().join(".git/FETCH_HEAD").exists(),
        "no fetch must have happened before the key check"
    );
    assert!(
        !source_dir.path().join(".git/gitprism.lock").exists(),
        "no operation lock must have been created before the key check"
    );

    clear_pin_env_vars();
}

#[test]
#[ignore = "TEST-001 step 2: policy::verify_expected_digest/marker::load_key are cfg(test)-forked to always succeed until the forks are removed"]
fn run_refuses_with_a_wrong_policy_digest_and_leaves_no_side_effects() {
    let _guard = crate::config::ENV_VAR_LOCK.lock().unwrap();
    clear_pin_env_vars();

    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

    let source_dir = tempdir().unwrap();
    source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    unsafe {
        std::env::set_var(
            "GITPRISM_STATE_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
        // Valid-looking (64 hex characters) but wrong.
        std::env::set_var("GITPRISM_POLICY_SHA256", "a".repeat(64));
    }

    let error = run(source_dir.path(), config.path())
        .expect_err("a wrong policy digest must refuse before any fetch or mutation");
    assert!(
        error.to_string().contains("does not match"),
        "error must say the pin does not match: {error}"
    );

    let dest_tip_after = dest_repo
        .find_branch("main", git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id();
    assert_eq!(dest_tip_after, dest_tip, "dest's refs must be unchanged");
    assert!(
        !source_dir.path().join(".git/FETCH_HEAD").exists(),
        "no fetch must have happened before the policy check"
    );
    assert!(
        !source_dir.path().join(".git/gitprism.lock").exists(),
        "no operation lock must have been created before the policy check"
    );

    clear_pin_env_vars();
}

#[test]
#[ignore = "TEST-001 step 2: policy::verify_expected_digest/marker::load_key are cfg(test)-forked to always succeed until the forks are removed"]
fn run_succeeds_with_correct_state_key_and_policy_digest_env_vars() {
    let _guard = crate::config::ENV_VAR_LOCK.lock().unwrap();
    clear_pin_env_vars();

    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("f.txt", "1")]);

    let source_dir = tempdir().unwrap();
    source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let config = write_config("unused", &dest_dir.path().display().to_string(), &["main"]);
    let expected_digest = crate::policy::hash_files(
        config.path(),
        &source_dir.path().join(crate::exclude::FILENAME),
    )
    .unwrap();
    unsafe {
        std::env::set_var(
            "GITPRISM_STATE_KEY",
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
        std::env::set_var("GITPRISM_POLICY_SHA256", &expected_digest);
    }

    run(source_dir.path(), config.path())
        .expect("correct env vars must let the real env-reading path run to completion");

    clear_pin_env_vars();
}
