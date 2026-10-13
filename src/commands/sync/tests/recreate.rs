//! decisions/0053: a configured branch whose dest ref is absent halts per
//! branch, and `sync --recreate-dest <branch>` recreates it create-only.

use super::*;

fn branch_tip(repo: &Repository, branch: &str) -> Oid {
    repo.find_branch(branch, git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id()
}

fn dest_ref(repo: &Repository, branch: &str) -> Option<Oid> {
    repo.find_reference(&format!("refs/heads/{branch}"))
        .ok()
        .and_then(|reference| reference.target())
}

fn delete_dest_ref(repo: &Repository, branch: &str) {
    repo.find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .delete()
        .unwrap();
}

/// Source `main` (configured) mirrored once; `feature` created at `main`'s
/// tip, mirrored as a discovered branch, then added to `config.branches`.
struct Configured {
    dest_dir: tempfile::TempDir,
    dest_repo: Repository,
    source_dir: tempfile::TempDir,
    source_repo: Repository,
    source_remote: tempfile::TempDir,
    config: NamedTempFile,
}

fn configured_feature_fixture() -> Configured {
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(
        dest_dir.path(),
        "main",
        &[("shared.txt", "v1"), (exclude::FILENAME, "secret.txt\n")],
    );
    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft = branch_tip(&source_repo, "main");
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    let source_url = source_remote.path().display().to_string();
    let dest_url = dest_dir.path().display().to_string();

    let main_only = write_config(&source_url, &dest_url, &["main"]);
    let s0 = add_commit(&source_repo, "main", &[("s0.txt", "s0\n")]);
    run(source_dir.path(), main_only.path()).expect("main must mirror s0");
    source_repo
        .branch("feature", &source_repo.find_commit(s0).unwrap(), false)
        .unwrap();
    run(source_dir.path(), main_only.path()).expect("feature must be mirrored once");

    let config = write_config(&source_url, &dest_url, &["main", "feature"]);
    run(source_dir.path(), config.path()).expect("configured feature must sync");

    Configured {
        dest_dir,
        dest_repo,
        source_dir,
        source_repo,
        source_remote,
        config,
    }
}

fn assert_own_marker_on_tip(fx: &Configured, branch: &str) {
    let key = marker::test_key();
    let tip = dest_ref(&fx.dest_repo, branch).expect("dest ref must exist");
    let commit = fx.dest_repo.find_commit(tip).unwrap();
    assert!(
        marker::verify(
            &commit,
            branch,
            &[MarkerDirection::SourceToDest],
            None,
            &key
        )
        .is_some(),
        "dest {branch}'s tip {tip} must carry {branch}'s own marker"
    );
}

#[test]
fn a_missing_configured_dest_ref_halts_only_that_branch() {
    let fx = configured_feature_fixture();
    delete_dest_ref(&fx.dest_repo, "feature");
    add_commit(&fx.source_repo, "main", &[("m1.txt", "m1\n")]);
    let main_before = dest_ref(&fx.dest_repo, "main").unwrap();
    let other = add_commit(&fx.source_repo, "main", &[("other.txt", "o\n")]);
    fx.source_repo
        .branch("other", &fx.source_repo.find_commit(other).unwrap(), false)
        .unwrap();
    add_commit(&fx.source_repo, "other", &[("o2.txt", "o2\n")]);

    let error = run(fx.source_dir.path(), fx.config.path())
        .expect_err("a halted branch makes the run exit non-zero");
    let message = format!("{error:#}");
    assert!(
        message.contains("one or more branches halted"),
        "a per-branch halt, not 0018's whole-run bail: {message}"
    );
    assert_eq!(
        dest_ref(&fx.dest_repo, "feature"),
        None,
        "without the flag, the configured ref is never recreated"
    );
    assert_ne!(dest_ref(&fx.dest_repo, "main"), Some(main_before));
    assert!(dest_ref(&fx.dest_repo, "other").is_some());
}

#[test]
fn the_missing_configured_dest_ref_diagnostic_names_the_flag() {
    let message = missing_configured_dest_ref_message("feature");
    assert!(message.contains("\"feature\""));
    assert!(message.contains("`gitprism sync --recreate-dest <branch>`"));
    assert!(message.contains("never deletes"));
}

#[test]
fn the_configured_foreign_marker_remedy_names_the_flag() {
    let message = foreign_mirror_marker_message("feature", "main", Oid::ZERO_SHA1, true);
    assert!(
        message.contains("`gitprism sync --recreate-dest <branch>`"),
        "{message}"
    );
}

#[test]
fn hostile_branch_names_stay_outside_the_suggested_recreate_command() {
    let branch = "feature-$(touch-pwned)";
    git::validate_branch_name(branch).expect("the hostile test branch remains a valid ref");
    for message in [
        missing_configured_dest_ref_message(branch),
        foreign_mirror_marker_message(branch, "main", Oid::ZERO_SHA1, true),
    ] {
        assert!(message.contains(&format!("{branch:?}")), "{message}");
        assert!(
            message.contains("`gitprism sync --recreate-dest <branch>`"),
            "{message}"
        );
        assert!(
            !message.contains(&format!("--recreate-dest {branch}")),
            "a copied command must not carry the raw name: {message}"
        );
    }
}

#[test]
fn recreate_dest_restores_the_reports_foreign_marker_state() {
    let fx = configured_feature_fixture();
    let dest_main = dest_ref(&fx.dest_repo, "main").unwrap();
    fx.dest_repo
        .reference("refs/heads/feature", dest_main, true, "operator cleanup")
        .unwrap();
    run(fx.source_dir.path(), fx.config.path())
        .expect_err("a foreign newest marker halts the configured branch");
    delete_dest_ref(&fx.dest_repo, "feature");

    run_recreating(fx.source_dir.path(), fx.config.path(), &["feature"])
        .expect("an explicit recreation must succeed");
    let recreated = dest_ref(&fx.dest_repo, "feature").unwrap();
    assert_eq!(
        fx.source_repo
            .find_commit(recreated)
            .unwrap()
            .parent_id(0)
            .unwrap(),
        dest_main,
        "recreated on main's projection"
    );
    let key = marker::test_key();
    assert!(
        marker::verify(
            &fx.source_repo.find_commit(recreated).unwrap(),
            "feature",
            &[MarkerDirection::SourceToDest],
            None,
            &key
        )
        .is_some(),
        "with a feature-branded marker"
    );

    run(fx.source_dir.path(), fx.config.path()).expect("the next plain sync succeeds");
    assert_eq!(dest_ref(&fx.dest_repo, "feature"), Some(recreated));

    add_commit(&fx.source_repo, "feature", &[("f1.txt", "f1\n")]);
    run(fx.source_dir.path(), fx.config.path()).expect("source -> dest resumes");
    let dest_feature = dest_ref(&fx.dest_repo, "feature").unwrap();
    assert!(
        fx.dest_repo
            .find_commit(dest_feature)
            .unwrap()
            .tree()
            .unwrap()
            .get_name("f1.txt")
            .is_some()
    );
    add_independent_dest_commit_on(
        &fx.dest_repo,
        "feature",
        dest_feature,
        ("native.txt", "n\n"),
        "dest native",
    );
    run(fx.source_dir.path(), fx.config.path()).expect("dest -> source resumes");
    assert!(
        fx.source_repo
            .find_commit(branch_tip(&fx.source_repo, "feature"))
            .unwrap()
            .tree()
            .unwrap()
            .get_name("native.txt")
            .is_some()
    );
}

#[test]
fn recreate_dest_replays_source_commits_beyond_the_anchor() {
    let fx = configured_feature_fixture();
    delete_dest_ref(&fx.dest_repo, "feature");
    add_commit(&fx.source_repo, "feature", &[("f1.txt", "f1\n")]);

    run_recreating(fx.source_dir.path(), fx.config.path(), &["feature"]).unwrap();
    let recreated = fx
        .dest_repo
        .find_commit(dest_ref(&fx.dest_repo, "feature").unwrap())
        .unwrap();
    assert!(recreated.tree().unwrap().get_name("f1.txt").is_some());
    let dest_main = dest_ref(&fx.dest_repo, "main").unwrap();
    assert!(
        fx.dest_repo
            .graph_descendant_of(recreated.id(), dest_main)
            .unwrap()
    );
    assert_own_marker_on_tip(&fx, "feature");
}

/// `feature` carries a dest-native commit imported into source, then its
/// dest ref is deleted: the import's dest commit is reachable only from
/// the deleted ref.
fn orphaned_import_fixture() -> (Configured, Oid) {
    let fx = configured_feature_fixture();
    let dest_feature = dest_ref(&fx.dest_repo, "feature").unwrap();
    let native = add_independent_dest_commit_on(
        &fx.dest_repo,
        "feature",
        dest_feature,
        ("native.txt", "n\n"),
        "dest native",
    );
    run(fx.source_dir.path(), fx.config.path()).expect("the dest-native commit is imported");
    delete_dest_ref(&fx.dest_repo, "feature");
    (fx, native)
}

#[test]
fn recreate_dest_republishes_an_orphaned_imported_anchor_present_locally() {
    let (fx, native) = orphaned_import_fixture();
    run_recreating(fx.source_dir.path(), fx.config.path(), &["feature"]).unwrap();
    let recreated = dest_ref(&fx.dest_repo, "feature").unwrap();
    assert!(
        recreated == native || fx.dest_repo.graph_descendant_of(recreated, native).unwrap(),
        "the orphaned imported commit is republished"
    );
}

#[test]
fn recreate_dest_halts_when_the_anchor_object_is_missing_without_falling_back() {
    let (fx, _native) = orphaned_import_fixture();
    let remote = fx.source_remote.path().display().to_string();
    for branch in ["main", "feature"] {
        let tip = branch_tip(&fx.source_repo, branch);
        git::push(
            fx.source_dir.path(),
            &remote,
            tip,
            branch,
            PushMode::FastForwardOnly,
        )
        .unwrap();
    }
    // A clone of source alone: the import's dest commit was never fetched.
    let clone_dir = tempdir().unwrap();
    let clone = Repository::init(clone_dir.path()).unwrap();
    for branch in ["feature", "main"] {
        git::fetch(clone_dir.path(), &remote, branch).unwrap();
        let tip = clone
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        clone.branch(branch, &tip, false).unwrap();
    }
    clone.set_head("refs/heads/main").unwrap();
    crate::testutil::checkout_head_exact(&clone);

    run_recreating(clone_dir.path(), fx.config.path(), &["feature"])
        .expect_err("a missing anchor object halts the branch");
    assert_eq!(dest_ref(&fx.dest_repo, "feature"), None);
}

#[test]
fn recreate_dest_refuses_an_existing_dest_ref() {
    let fx = configured_feature_fixture();
    let before = dest_ref(&fx.dest_repo, "feature");
    add_commit(&fx.source_repo, "feature", &[("f1.txt", "f1\n")]);
    run_recreating(fx.source_dir.path(), fx.config.path(), &["feature"])
        .expect_err("the flag never applies to an existing ref");
    assert_eq!(dest_ref(&fx.dest_repo, "feature"), before);
}

/// A recreation attempt chosen from a stale "absent" cache entry while
/// dest `feature` already exists at whatever `existing` returns.
fn recreate_against_a_concurrent_ref(existing: impl FnOnce(&Configured) -> Oid) {
    let fx = configured_feature_fixture();
    delete_dest_ref(&fx.dest_repo, "feature");
    let concurrent = existing(&fx);
    fx.dest_repo
        .reference("refs/heads/feature", concurrent, true, "concurrent writer")
        .unwrap();

    let key = marker::test_key();
    let source_tip = branch_tip(&fx.source_repo, "feature");
    let exclude_list = load_current_exclude_list(&fx.source_repo, source_tip).unwrap();
    let ignore_raw = std::fs::read_to_string(fx.source_dir.path().join(exclude::FILENAME)).unwrap();
    let config = Config::load(fx.config.path()).unwrap();
    let dest_url = config.dest_url().unwrap();
    let (source_branches, _) = list_source_branches(&fx.source_repo).unwrap();
    let mut listing = git::remote_branch_names(fx.source_dir.path(), &dest_url).unwrap();
    listing.names.retain(|name| name != "feature");
    git::fetch_heads_into_namespace(fx.source_dir.path(), &dest_url, &listing.names).unwrap();
    let mut run_cache = RunCache::default();
    run_cache.mapping_index = reconstruct_mapping_index(
        &fx.source_repo,
        fx.source_dir.path(),
        &dest_url,
        &listing,
        &source_branches,
        &key,
        &mut run_cache,
    )
    .unwrap();
    assert_eq!(run_cache.dest_ref_exists.get("feature"), Some(&false));

    let reporter = Reporter::new(1, std::iter::empty());
    let halted = sync_pair_to_dest_with_key(
        &fx.source_repo,
        fx.source_dir.path(),
        &config,
        "feature",
        &reporter,
        &key,
        &exclude_list,
        &ignore_raw,
        &mut run_cache,
        true,
    )
    .expect("an appearing ref halts the branch, not the run");
    assert!(halted);
    assert_eq!(
        dest_ref(&fx.dest_repo, "feature"),
        Some(concurrent),
        "the concurrent ref is preserved"
    );
}

#[test]
fn recreate_dest_preserves_a_ref_created_concurrently_at_an_ancestor() {
    recreate_against_a_concurrent_ref(|fx| dest_ref(&fx.dest_repo, "main").unwrap());
}

#[test]
fn recreate_dest_preserves_a_ref_created_concurrently_at_an_incompatible_tip() {
    recreate_against_a_concurrent_ref(|fx| {
        let main = dest_ref(&fx.dest_repo, "main").unwrap();
        let commit = add_independent_dest_commit_on(
            &fx.dest_repo,
            "scratch",
            main,
            ("x.txt", "x\n"),
            "someone else's",
        );
        delete_dest_ref(&fx.dest_repo, "scratch");
        commit
    });
}

#[test]
fn recreate_dest_rejects_unusable_names_before_any_fetch() {
    let fx = configured_feature_fixture();
    let unreachable_dest = fx.dest_dir.path().join("missing");
    let source_url = fx.source_remote.path().display().to_string();
    let config = write_config(
        &source_url,
        &unreachable_dest.display().to_string(),
        &["main", "feature", "ghost"],
    );
    for (name, why) in [
        ("other", "not in config.branches"),
        ("bad..name", "invalid"),
        ("ghost", "no local source branch"),
    ] {
        let error = run_recreating(fx.source_dir.path(), config.path(), &[name]).expect_err(why);
        let message = format!("{error:#}");
        assert!(
            message.contains("--recreate-dest") && !message.contains("missing"),
            "{name} ({why}) must be a usage error before the dest is contacted: {message}"
        );
    }
}

#[test]
fn recreate_dest_halts_without_an_exact_anchor() {
    let fx = configured_feature_fixture();
    delete_dest_ref(&fx.dest_repo, "feature");
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let tree = fx
        .source_repo
        .find_commit(branch_tip(&fx.source_repo, "feature"))
        .unwrap()
        .tree()
        .unwrap();
    let unrelated = fx
        .source_repo
        .commit(None, &signature, &signature, "unrelated root", &tree, &[])
        .unwrap();
    fx.source_repo
        .branch(
            "feature",
            &fx.source_repo.find_commit(unrelated).unwrap(),
            true,
        )
        .unwrap();

    run_recreating(fx.source_dir.path(), fx.config.path(), &["feature"])
        .expect_err("no exact anchor halts the branch");
    assert_eq!(dest_ref(&fx.dest_repo, "feature"), None);
}

#[test]
fn sync_options_deduplicate_requested_branches() {
    let options = SyncOptions::new(["a", "b", "a"].map(String::from));
    assert_eq!(options.recreate_dest.iter().collect::<Vec<_>>(), ["a", "b"]);
}
