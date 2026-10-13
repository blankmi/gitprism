//! decisions/0052: branches advancing in lockstep reuse an existing
//! projection instead of replaying a second, incomparable copy.

use super::*;

fn branch_tip(repo: &Repository, branch: &str) -> Oid {
    repo.find_branch(branch, git2::BranchType::Local)
        .unwrap()
        .get()
        .peel_to_commit()
        .unwrap()
        .id()
}

fn set_branch(repo: &Repository, branch: &str, target: Oid) {
    repo.branch(branch, &repo.find_commit(target).unwrap(), true)
        .unwrap();
}

/// Source `main` (configured) grafted onto dest; `feature` created on source
/// at `main`'s tip and synced once, so dest `feature` is a decisions/0044
/// marker on top of dest `main`.
struct Lockstep {
    dest_dir: tempfile::TempDir,
    dest_repo: Repository,
    source_dir: tempfile::TempDir,
    source_repo: Repository,
    source_remote_url: String,
    _source_remote: tempfile::TempDir,
    config: NamedTempFile,
}

fn lockstep_fixture() -> Lockstep {
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
    let source_remote_url = source_remote.path().display().to_string();
    let config = write_config(
        &source_remote_url,
        &dest_dir.path().display().to_string(),
        &["main"],
    );

    // main mirrors one commit, so its dest tip carries main's own marker.
    let s0 = add_commit(&source_repo, "main", &[("s0.txt", "s0\n")]);
    run(source_dir.path(), config.path()).expect("main must mirror s0");
    source_repo
        .branch("feature", &source_repo.find_commit(s0).unwrap(), false)
        .unwrap();
    run(source_dir.path(), config.path()).expect("the initial sync must create dest feature");
    {
        let dest_feature = dest_repo
            .find_commit(branch_tip(&dest_repo, "feature"))
            .unwrap();
        assert_eq!(
            dest_feature.parent_id(0).unwrap(),
            branch_tip(&dest_repo, "main"),
            "fixture precondition: dest feature is a 0044 marker on dest main"
        );
        assert_eq!(
            dest_feature.tree_id(),
            dest_feature.parent(0).unwrap().tree_id()
        );
    }

    Lockstep {
        dest_dir,
        dest_repo,
        source_dir,
        source_repo,
        source_remote_url,
        _source_remote: source_remote,
        config,
    }
}

fn assert_single_projection(fx: &Lockstep, shared: Oid) {
    let main = branch_tip(&fx.dest_repo, "main");
    let feature = branch_tip(&fx.dest_repo, "feature");
    let main_has_feature =
        main == feature || fx.dest_repo.graph_descendant_of(main, feature).unwrap();
    let feature_has_main =
        main == feature || fx.dest_repo.graph_descendant_of(feature, main).unwrap();
    assert!(
        main_has_feature || feature_has_main,
        "dest main ({main}) and dest feature ({feature}) must not be incomparable copies of {shared}"
    );
}

fn child_of_shared_commit_syncs(fx: &Lockstep, shared: Oid) {
    fx.source_repo
        .branch("task", &fx.source_repo.find_commit(shared).unwrap(), false)
        .unwrap();
    add_commit(&fx.source_repo, "task", &[("task.txt", "task\n")]);
    run(fx.source_dir.path(), fx.config.path())
        .expect("a child of the shared commit must not halt on contradictory mappings");
    let dest_task = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "task"))
        .unwrap();
    assert!(dest_task.tree().unwrap().get_name("task.txt").is_some());
    assert!(dest_task.tree().unwrap().get_name("s1.txt").is_some());
}

#[test]
fn lockstep_advance_in_one_run_reuses_mains_projection() {
    let fx = lockstep_fixture();
    let shared = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    set_branch(&fx.source_repo, "feature", shared);

    run(fx.source_dir.path(), fx.config.path()).expect("the lockstep advance must sync");

    assert_single_projection(&fx, shared);
    child_of_shared_commit_syncs(&fx, shared);
}

#[test]
fn lockstep_advance_in_separate_runs_reuses_mains_projection() {
    let fx = lockstep_fixture();
    let shared = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    run(fx.source_dir.path(), fx.config.path()).expect("main must sync s1 first");
    set_branch(&fx.source_repo, "feature", shared);
    run(fx.source_dir.path(), fx.config.path()).expect("feature must catch up to main");

    let dest_main = branch_tip(&fx.dest_repo, "main");
    let dest_feature = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "feature"))
        .unwrap();
    assert_eq!(
        dest_feature.parent_count(),
        2,
        "feature gets an adoption merge"
    );
    assert_eq!(dest_feature.parent_id(1).unwrap(), dest_main);
    assert_eq!(
        dest_feature.tree_id(),
        fx.dest_repo.find_commit(dest_main).unwrap().tree_id()
    );
    child_of_shared_commit_syncs(&fx, shared);
}

#[test]
fn two_successive_lockstep_advances_keep_a_single_projection() {
    let fx = lockstep_fixture();
    let first = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    set_branch(&fx.source_repo, "feature", first);
    run(fx.source_dir.path(), fx.config.path()).unwrap();

    let second = add_commit(&fx.source_repo, "main", &[("s2.txt", "s2\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    set_branch(&fx.source_repo, "feature", second);
    run(fx.source_dir.path(), fx.config.path()).expect("the second lockstep advance must sync");

    let dest_main = branch_tip(&fx.dest_repo, "main");
    let dest_feature = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "feature"))
        .unwrap();
    assert_eq!(dest_feature.parent_count(), 2);
    assert_eq!(dest_feature.parent_id(1).unwrap(), dest_main);
    assert_eq!(
        fx.dest_repo
            .find_commit(dest_feature.parent_id(0).unwrap())
            .unwrap()
            .parent_count(),
        2,
        "the first adoption merge stays on feature's first-parent line"
    );
    child_of_shared_commit_syncs(&fx, second);
}

#[test]
fn lockstep_without_a_marker_fast_forwards_onto_mains_projection() {
    // Both branches configured and sitting on the same dest commit: no 0044
    // marker separates them, so reuse is a plain fast-forward.
    let dest_dir = tempdir().unwrap();
    let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
    let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
    {
        let tip = dest_repo.find_commit(dest_tip).unwrap();
        dest_repo.branch("feature", &tip, false).unwrap();
    }
    let source_dir = tempdir().unwrap();
    let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
    let graft = branch_tip(&source_repo, "main");
    source_repo
        .branch("feature", &source_repo.find_commit(graft).unwrap(), false)
        .unwrap();
    let source_remote = bare_source_remote_seeded_at(&source_repo, "main", graft);
    {
        let remote = Repository::open(source_remote.path()).unwrap();
        let commit = remote.find_commit(graft).unwrap();
        remote.branch("feature", &commit, false).unwrap();
    }
    let config = write_config(
        &source_remote.path().display().to_string(),
        &dest_dir.path().display().to_string(),
        &["main", "feature"],
    );

    let shared = add_commit(&source_repo, "main", &[("s1.txt", "s1\n")]);
    set_branch(&source_repo, "feature", shared);
    run(source_dir.path(), config.path()).expect("the lockstep advance must sync");

    // `feature` sorts first and projects s1; `main` then fast-forwards onto
    // that projection.
    let dest_main = dest_repo
        .find_commit(branch_tip(&dest_repo, "main"))
        .unwrap();
    let dest_feature = branch_tip(&dest_repo, "feature");
    assert_eq!(
        dest_main.parent_count(),
        1,
        "a fast-forward needs no bridge"
    );
    assert_eq!(
        dest_main.parent_id(0).unwrap(),
        dest_feature,
        "main must fast-forward onto feature's projection, not fork its own copy"
    );
    assert_eq!(
        dest_main.tree_id(),
        dest_repo.find_commit(dest_feature).unwrap().tree_id()
    );
}

#[test]
fn reuse_halts_when_mains_projection_carries_content_feature_never_imported() {
    // Jack's q sequence: main imports a dest-native file, then mirrors the
    // shared commit onto its q-bearing dest tip. feature, at the shared
    // commit, never imported q, so reusing main's projection would leak it.
    let fx = lockstep_fixture();
    let shared = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    let dest_main_before = branch_tip(&fx.dest_repo, "main");
    add_independent_dest_commit(
        &fx.dest_repo,
        dest_main_before,
        ("q.txt", "q\n"),
        "dest native q",
    );
    // dest→source imports q as a child of `shared`, then source→dest mirrors
    // `shared` onto the q-bearing dest tip.
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let dest_main = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "main"))
        .unwrap();
    assert!(dest_main.tree().unwrap().get_name("q.txt").is_some());

    let dest_feature_before = branch_tip(&fx.dest_repo, "feature");
    set_branch(&fx.source_repo, "feature", shared);
    let error = run(fx.source_dir.path(), fx.config.path())
        .expect_err("reuse can't be proven, so feature must halt");
    assert!(
        format!("{error:#}").contains("couldn't be proven safe"),
        "{error:#}"
    );
    assert_eq!(
        branch_tip(&fx.dest_repo, "feature"),
        dest_feature_before,
        "nothing is pushed for a branch whose reuse proof fails"
    );
    let inputs = proof_inputs(&fx, None);
    match plan(&fx, &inputs, limits::MAX_MARKER_SCAN_COMMITS) {
        reuse::ReusePlan::Halt(message) => assert!(
            message.contains("dest content not represented"),
            "the halt must come from the unrepresented q commit: {message}"
        ),
        other => panic!("expected a halt, got {other:?}"),
    }
}

#[test]
fn a_configured_branch_parked_on_another_branchs_projection_halts_only_itself() {
    // The report's recovery state: dest feature force-moved onto main's own
    // projection, with feature configured.
    let mut fx = lockstep_fixture();
    let shared = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let dest_main = branch_tip(&fx.dest_repo, "main");
    fx.dest_repo
        .reference("refs/heads/feature", dest_main, true, "operator recovery")
        .unwrap();
    set_branch(&fx.source_repo, "feature", shared);
    fx.config = write_config(
        &fx.source_remote_url,
        &fx.dest_dir.path().display().to_string(),
        &["main", "feature"],
    );

    // An unrelated discovered branch must still sync in the same run.
    fx.source_repo
        .branch("other", &fx.source_repo.find_commit(shared).unwrap(), false)
        .unwrap();
    add_commit(&fx.source_repo, "other", &[("other.txt", "other\n")]);

    let error = run(fx.source_dir.path(), fx.config.path())
        .expect_err("feature's refusal must still fail the run");
    let message = format!("{error:#}");
    assert!(
        !message.contains("fetch/pull the latest source history"),
        "the configured refusal must no longer abort with the fetch/pull advice: {message}"
    );
    assert!(
        fx.dest_repo
            .find_branch("other", git2::BranchType::Local)
            .is_ok(),
        "a later branch must still be processed"
    );
    assert_eq!(branch_tip(&fx.dest_repo, "feature"), dest_main);
}

/// Proof inputs for `feature` as `sync` would compute them, without pushing.
struct ProofInputs {
    index: mapping_index::MappingIndex,
    pending: Vec<Oid>,
    boundary: Oid,
    dest_tip: Oid,
}

fn proof_inputs(fx: &Lockstep, dest_tip_override: Option<(Oid, Oid)>) -> ProofInputs {
    let key = marker::test_key();
    let config = Config::load(fx.config.path()).unwrap();
    let dest_url = config.dest_url().unwrap();
    let source_root = fx.source_dir.path();
    let (source_branches, _) = list_source_branches(&fx.source_repo).unwrap();
    let listing = git::remote_branch_names(source_root, &dest_url).unwrap();
    git::fetch_heads_into_namespace(source_root, &dest_url, &listing.names).unwrap();
    let mut run_cache = RunCache::default();
    let index = reconstruct_mapping_index(
        &fx.source_repo,
        source_root,
        &dest_url,
        &listing,
        &source_branches,
        &key,
        &mut run_cache,
    )
    .unwrap();
    let source_tip = branch_tip(&fx.source_repo, "feature");
    let (dest_tip, boundary) = match dest_tip_override {
        Some(pair) => pair,
        None => {
            git::fetch(source_root, &dest_url, "feature").unwrap();
            let dest_tip = fx
                .source_repo
                .find_reference("FETCH_HEAD")
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id();
            let boundary = dest_resume_point_for_branch(
                &fx.source_repo,
                source_tip,
                dest_tip,
                "feature",
                &key,
            )
            .unwrap()
            .expect("feature's own dest tip is a safe resume point");
            (dest_tip, boundary)
        }
    };
    let pending = pending_commits(&fx.source_repo, boundary, source_tip).unwrap();
    ProofInputs {
        index,
        pending,
        boundary,
        dest_tip,
    }
}

fn plan(fx: &Lockstep, inputs: &ProofInputs, scan_limit: usize) -> reuse::ReusePlan {
    reuse::plan_projection_reuse_bounded(
        &fx.source_repo,
        &inputs.index,
        &inputs.pending,
        inputs.boundary,
        inputs.dest_tip,
        "feature",
        &marker::test_key(),
        scan_limit,
    )
    .unwrap()
}

/// A gitprism-shaped SourceToDest commit in the source repo's odb, as
/// `build_dest_commit` would write it, but with arbitrary parents, tree,
/// branch and key.
fn craft_dest_commit(
    repo: &Repository,
    parents: &[Oid],
    tree: Oid,
    branch: &str,
    counterpart: Oid,
    key: &marker::StateKey,
) -> Oid {
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let message = marker::build_message(
        "crafted",
        MarkerDirection::SourceToDest,
        branch,
        counterpart,
        "Gitprism-Source-Commit",
        parents,
        tree,
        &signature,
        &signature,
        key,
    );
    let parent_commits: Vec<_> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    repo.commit(
        None,
        &signature,
        &signature,
        &message,
        &repo.find_tree(tree).unwrap(),
        &parent_commits.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

fn tree_with(repo: &Repository, base: Oid, name: &str, body: &str) -> Oid {
    let base_tree = repo.find_commit(base).unwrap().tree().unwrap();
    let mut builder = repo.treebuilder(Some(&base_tree)).unwrap();
    let blob = repo.blob(body.as_bytes()).unwrap();
    builder
        .insert(name, blob, git2::FileMode::Blob.into())
        .unwrap();
    builder.write().unwrap()
}

/// main has mirrored s1; feature sits on source s1, dest feature still at
/// its 0044 marker. Returns (s1, dest main's projection of s1).
fn main_ahead_by_one(fx: &Lockstep) -> (Oid, Oid) {
    let s1 = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    set_branch(&fx.source_repo, "feature", s1);
    (s1, branch_tip(&fx.dest_repo, "main"))
}

#[test]
fn the_proof_budget_is_one_aggregate_allowance_checked_before_each_load() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let inputs = proof_inputs(&fx, None);
    // Distinct commits loaded: dest feature's marker, the commit it sits on
    // (also the shared base), main's projection of s1, and source s1 itself
    // (the in-range import scan). Re-reads of an already-loaded commit are
    // not counted.
    assert_eq!(
        plan(&fx, &inputs, 4),
        reuse::ReusePlan::Bridge {
            shared_source: s1,
            projection: d1
        },
        "a proof completing exactly at the allowance is valid"
    );
    match plan(&fx, &inputs, 3) {
        reuse::ReusePlan::Halt(message) => assert!(message.contains("scan limit"), "{message}"),
        other => panic!("one load short of the allowance must halt, got {other:?}"),
    }
}

#[test]
fn only_an_authenticated_same_branch_bridge_of_the_right_shape_is_trusted() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let dest_feature_marker = branch_tip(&fx.dest_repo, "feature");
    let s2 = add_commit(&fx.source_repo, "main", &[("s2.txt", "s2\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let d2 = branch_tip(&fx.dest_repo, "main");
    set_branch(&fx.source_repo, "feature", s2);
    // Make the dest objects available in the source odb.
    let _ = proof_inputs(&fx, None);

    let repo = &fx.source_repo;
    let key = marker::test_key();
    let d1_tree = repo.find_commit(d1).unwrap().tree_id();
    let marker_tree = repo.find_commit(dest_feature_marker).unwrap().tree_id();
    let other_key = marker::StateKey::from_hex(&"ab".repeat(32)).unwrap();
    let d0 = repo.find_commit(d1).unwrap().parent_id(0).unwrap();

    let valid = craft_dest_commit(
        repo,
        &[dest_feature_marker, d1],
        d1_tree,
        "feature",
        s1,
        &key,
    );
    let inputs = proof_inputs(&fx, Some((valid, s1)));
    assert_eq!(
        plan(&fx, &inputs, limits::MAX_MARKER_SCAN_COMMITS),
        reuse::ReusePlan::Bridge {
            shared_source: s2,
            projection: d2
        },
        "a valid earlier bridge lets the next lockstep advance reuse main's projection"
    );

    let untrusted = [
        (
            "wrong tree",
            craft_dest_commit(
                repo,
                &[dest_feature_marker, d1],
                marker_tree,
                "feature",
                s1,
                &key,
            ),
        ),
        (
            "wrong branch",
            craft_dest_commit(repo, &[dest_feature_marker, d1], d1_tree, "other", s1, &key),
        ),
        (
            "unauthenticated",
            craft_dest_commit(
                repo,
                &[dest_feature_marker, d1],
                d1_tree,
                "feature",
                s1,
                &other_key,
            ),
        ),
        (
            "three parents",
            craft_dest_commit(
                repo,
                &[dest_feature_marker, d1, d0],
                d1_tree,
                "feature",
                s1,
                &key,
            ),
        ),
    ];
    for (label, bridge) in untrusted {
        let inputs = proof_inputs(&fx, Some((bridge, s1)));
        assert!(
            matches!(
                plan(&fx, &inputs, limits::MAX_MARKER_SCAN_COMMITS),
                reuse::ReusePlan::Halt(_)
            ),
            "a bridge with the {label} must not be trusted"
        );
    }
}

#[test]
fn lineage_counterparts_never_reset_to_an_earlier_source_commit() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let s2 = add_commit(&fx.source_repo, "main", &[("s2.txt", "s2\n")]);
    set_branch(&fx.source_repo, "feature", s2);
    let inputs = proof_inputs(&fx, None);
    let repo = &fx.source_repo;
    let key = marker::test_key();
    let d1_tree = repo.find_commit(d1).unwrap().tree_id();

    for (label, alias_names, expect_bridge) in [
        ("repeats the current counterpart", s1, true),
        ("resets to the boundary", inputs.boundary, false),
    ] {
        let alias = craft_dest_commit(repo, &[d1], d1_tree, "main", alias_names, &key);
        let projection = craft_dest_commit(
            repo,
            &[alias],
            tree_with(repo, alias, "s2.txt", "s2\n"),
            "main",
            s2,
            &key,
        );
        let mut index = mapping_index::MappingIndex::new();
        index.record_built_mapping("main", s2, projection);
        let inputs = ProofInputs {
            index,
            pending: inputs.pending.clone(),
            boundary: inputs.boundary,
            dest_tip: inputs.dest_tip,
        };
        let result = plan(&fx, &inputs, limits::MAX_MARKER_SCAN_COMMITS);
        assert_eq!(
            matches!(result, reuse::ReusePlan::Bridge { .. }),
            expect_bridge,
            "an alias that {label}: {result:?}"
        );
    }
}

fn source_counterpart(repo: &Repository, dest_commit: Oid) -> Oid {
    marker::verify_self(
        &repo.find_commit(dest_commit).unwrap(),
        &[MarkerDirection::SourceToDest],
        None,
        &marker::test_key(),
    )
    .expect("a gitprism SourceToDest commit")
    .counterpart
}

#[test]
fn a_bridge_with_nothing_built_on_top_names_the_shared_commit_not_the_source_tip() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    // feature's only commit beyond s1 touches an excluded path, so nothing
    // is built on top of the bridge.
    let excluded_only = add_commit(&fx.source_repo, "feature", &[("secret.txt", "hidden\n")]);
    run(fx.source_dir.path(), fx.config.path()).expect("feature must sync");

    let dest_feature = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "feature"))
        .unwrap();
    assert_eq!(dest_feature.parent_count(), 2);
    assert_eq!(dest_feature.parent_id(1).unwrap(), d1);
    let named = source_counterpart(&fx.dest_repo, dest_feature.id());
    assert_eq!(named, s1, "the bridge names the shared commit");
    assert_ne!(named, excluded_only);

    run(fx.source_dir.path(), fx.config.path()).expect("a second run must be a no-op");
    assert_eq!(branch_tip(&fx.dest_repo, "feature"), dest_feature.id());
}

#[test]
fn a_reused_projection_carries_on_with_the_branchs_own_commits() {
    let fx = lockstep_fixture();
    let (_, d1) = main_ahead_by_one(&fx);
    let own = add_commit(&fx.source_repo, "feature", &[("feature.txt", "f\n")]);
    run(fx.source_dir.path(), fx.config.path()).expect("feature must sync");

    let dest_feature = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "feature"))
        .unwrap();
    assert_eq!(source_counterpart(&fx.dest_repo, dest_feature.id()), own);
    let bridge = fx
        .dest_repo
        .find_commit(dest_feature.parent_id(0).unwrap())
        .unwrap();
    assert_eq!(
        bridge.parent_count(),
        2,
        "feature's commit is built on the bridge"
    );
    assert_eq!(bridge.parent_id(1).unwrap(), d1);
    assert!(
        dest_feature
            .tree()
            .unwrap()
            .get_name("feature.txt")
            .is_some()
    );
    assert!(dest_feature.tree().unwrap().get_name("s1.txt").is_some());
}

#[test]
fn a_policy_mismatch_inside_the_reused_range_still_halts() {
    let fx = lockstep_fixture();
    let s1 = add_commit(
        &fx.source_repo,
        "main",
        &[
            (exclude::FILENAME, "secret.txt\n*.log\n"),
            ("s1.txt", "s1\n"),
        ],
    );
    run(fx.source_dir.path(), fx.config.path()).expect("main syncs s1 under its own pin");
    // The pin moves on; s1's control file now disagrees with it.
    add_commit(
        &fx.source_repo,
        "main",
        &[(exclude::FILENAME, "secret.txt\n*.secret\n")],
    );
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let dest_feature_before = branch_tip(&fx.dest_repo, "feature");
    set_branch(&fx.source_repo, "feature", s1);

    let error = run(fx.source_dir.path(), fx.config.path())
        .expect_err("the 0037 check runs over the original range before any reuse");
    assert!(
        format!("{error:#}").to_lowercase().contains("polic"),
        "{error:#}"
    );
    assert_eq!(branch_tip(&fx.dest_repo, "feature"), dest_feature_before);
}

#[test]
fn a_truncated_index_halts_on_an_observed_entry_and_replays_without_one() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let inputs = proof_inputs(&fx, None);

    let mut observed = mapping_index::MappingIndex::new();
    observed.record_built_mapping("main", s1, d1);
    observed.note_incomplete_reconstruction("a test horizon".to_owned());
    let with_entry = ProofInputs {
        index: observed,
        pending: inputs.pending.clone(),
        boundary: inputs.boundary,
        dest_tip: inputs.dest_tip,
    };
    assert!(
        matches!(
            plan(&fx, &with_entry, limits::MAX_MARKER_SCAN_COMMITS),
            reuse::ReusePlan::Halt(_)
        ),
        "an observed entry in a truncated index must not be bypassed"
    );

    let mut unobserved = mapping_index::MappingIndex::new();
    unobserved.note_incomplete_reconstruction("a test horizon".to_owned());
    let without_entry = ProofInputs {
        index: unobserved,
        ..with_entry
    };
    assert_eq!(
        plan(&fx, &without_entry, limits::MAX_MARKER_SCAN_COMMITS),
        reuse::ReusePlan::Replay,
        "absence in a truncated index keeps today's replay"
    );
}

#[test]
fn refusal_classification_keeps_the_original_refusal_at_its_horizon() {
    let fx = lockstep_fixture();
    let (_, d1) = main_ahead_by_one(&fx);
    let _ = proof_inputs(&fx, None);
    let repo = &fx.source_repo;
    let key = marker::test_key();
    // d1 carries main's marker: one load finds it.
    assert_eq!(
        classify_refusal_bounded(repo, d1, "feature", &key, 1).unwrap(),
        ResumeRefusal::ForeignNewestMirrorMarker {
            branch: "main".to_owned(),
            marker: d1
        }
    );
    // A dest-native commit above it pushes the marker past a limit of 1.
    let native = {
        let signature = Signature::now("Dest Maintainer", "m@example.com").unwrap();
        let parent = repo.find_commit(d1).unwrap();
        repo.commit(
            None,
            &signature,
            &signature,
            "dest native",
            &repo.find_tree(tree_with(repo, d1, "n.txt", "n\n")).unwrap(),
            &[&parent],
        )
        .unwrap()
    };
    assert_eq!(
        classify_refusal_bounded(repo, native, "feature", &key, 1).unwrap(),
        ResumeRefusal::Unsafe {
            classification_inconclusive: true
        },
        "a horizon only marks the original refusal inconclusive; it never escalates"
    );
    assert_eq!(
        classify_refusal_bounded(repo, d1, "main", &key, 1).unwrap(),
        ResumeRefusal::Unsafe {
            classification_inconclusive: false
        },
        "the branch's own newest marker is not a foreign-marker refusal"
    );
}

/// A source commit on `parent` with `tree`, carrying a marker of
/// `direction` naming `counterpart`.
fn craft_source_marker(
    repo: &Repository,
    parent: Oid,
    tree: Oid,
    direction: MarkerDirection,
    counterpart: Oid,
    key: &marker::StateKey,
) -> Oid {
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let message = marker::build_message(
        "gitprism sync: dest -> source",
        direction,
        "main",
        counterpart,
        "Gitprism-Dest-Commit",
        &[parent],
        tree,
        &signature,
        &signature,
        key,
    );
    let parent_commit = repo.find_commit(parent).unwrap();
    repo.commit(
        None,
        &signature,
        &signature,
        &message,
        &repo.find_tree(tree).unwrap(),
        &[&parent_commit],
    )
    .unwrap()
}

fn native_dest_commit(repo: &Repository, parents: &[Oid], tree: Oid) -> Oid {
    let signature = Signature::now("Dest Maintainer", "m@example.com").unwrap();
    let parent_commits: Vec<_> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    repo.commit(
        None,
        &signature,
        &signature,
        "dest native",
        &repo.find_tree(tree).unwrap(),
        &parent_commits.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

/// Runs the proof for `feature` at `feature_tip` against a hand-built index
/// holding `mappings` (source, dest), all recorded for "main".
fn plan_against(fx: &Lockstep, feature_tip: Oid, mappings: &[(Oid, Oid)]) -> reuse::ReusePlan {
    set_branch(&fx.source_repo, "feature", feature_tip);
    let inputs = proof_inputs(fx, None);
    let mut index = mapping_index::MappingIndex::new();
    for (source, dest) in mappings {
        index.record_built_mapping("main", *source, *dest);
    }
    plan(
        fx,
        &ProofInputs { index, ..inputs },
        limits::MAX_MARKER_SCAN_COMMITS,
    )
}

fn source_commit_on(repo: &Repository, parent: Oid, name: &str) -> Oid {
    native_dest_commit(repo, &[parent], tree_with(repo, parent, name, "x\n"))
}

#[test]
fn an_import_inside_the_pending_range_represents_its_dest_native_commit() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let _ = proof_inputs(&fx, None);
    let repo = &fx.source_repo;
    let key = marker::test_key();
    let q = native_dest_commit(repo, &[d1], tree_with(repo, d1, "q.txt", "q\n"));
    let n_tree = tree_with(repo, s1, "q.txt", "q\n");

    // D1 -> Q -> D2 on dest; s1, N (imports Q), s2 in source.
    let n = craft_source_marker(repo, s1, n_tree, MarkerDirection::DestToSource, q, &key);
    let s2 = source_commit_on(repo, n, "s2.txt");
    let d2 = craft_dest_commit(
        repo,
        &[q],
        tree_with(repo, q, "s2.txt", "x\n"),
        "main",
        s2,
        &key,
    );
    assert_eq!(
        plan_against(&fx, s2, &[(s2, d2)]),
        reuse::ReusePlan::Bridge {
            shared_source: s2,
            projection: d2
        },
        "an exact in-range import proves Q is represented"
    );

    let other_key = marker::StateKey::from_hex(&"ab".repeat(32)).unwrap();
    for (label, importer) in [
        (
            "unauthenticated",
            craft_source_marker(
                repo,
                s1,
                n_tree,
                MarkerDirection::DestToSource,
                q,
                &other_key,
            ),
        ),
        (
            "a Setup marker",
            craft_source_marker(repo, s1, n_tree, MarkerDirection::Setup, q, &key),
        ),
    ] {
        let s2 = source_commit_on(repo, importer, "s2.txt");
        let d2 = craft_dest_commit(
            repo,
            &[q],
            tree_with(repo, q, "s2.txt", "x\n"),
            "main",
            s2,
            &key,
        );
        assert!(
            matches!(
                plan_against(&fx, s2, &[(s2, d2)]),
                reuse::ReusePlan::Halt(message) if message.contains("not represented")
            ),
            "an import with {label} must not represent Q"
        );
    }

    // Out of range: the import sits after the shared commit.
    let s2_direct = source_commit_on(repo, s1, "s2.txt");
    let d2_direct = craft_dest_commit(
        repo,
        &[q],
        tree_with(repo, q, "s2.txt", "x\n"),
        "main",
        s2_direct,
        &key,
    );
    let after = craft_source_marker(
        repo,
        s2_direct,
        n_tree,
        MarkerDirection::DestToSource,
        q,
        &key,
    );
    let _ = after;
    assert!(
        matches!(
            plan_against(&fx, s2_direct, &[(s2_direct, d2_direct)]),
            reuse::ReusePlan::Halt(message) if message.contains("not represented")
        ),
        "an import after the shared commit doesn't represent Q"
    );

    // Out of order: on dest Q sits below s2's projection, but in source the
    // import comes after s2.
    let late_import = craft_source_marker(
        repo,
        s2_direct,
        tree_with(repo, s2_direct, "q.txt", "q\n"),
        MarkerDirection::DestToSource,
        q,
        &key,
    );
    let s3 = source_commit_on(repo, late_import, "s3.txt");
    let d3 = craft_dest_commit(
        repo,
        &[d2_direct],
        tree_with(repo, d2_direct, "s3.txt", "x\n"),
        "main",
        s3,
        &key,
    );
    assert!(
        matches!(
            plan_against(&fx, s3, &[(s3, d3)]),
            reuse::ReusePlan::Halt(message) if message.contains("out of first-parent order")
        ),
        "s2's projection above an import that comes later in source is out of order"
    );

    // A two-parent dest-native commit isn't accepted even with an exact
    // in-range import.
    let side = native_dest_commit(repo, &[d1], tree_with(repo, d1, "side.txt", "s\n"));
    let merge = native_dest_commit(repo, &[d1, side], tree_with(repo, d1, "q.txt", "q\n"));
    let merge_import =
        craft_source_marker(repo, s1, n_tree, MarkerDirection::DestToSource, merge, &key);
    let s2_merge = source_commit_on(repo, merge_import, "s2.txt");
    let d2_merge = craft_dest_commit(
        repo,
        &[merge],
        tree_with(repo, merge, "s2.txt", "x\n"),
        "main",
        s2_merge,
        &key,
    );
    assert!(
        matches!(
            plan_against(&fx, s2_merge, &[(s2_merge, d2_merge)]),
            reuse::ReusePlan::Halt(message) if message.contains("more than one parent")
        ),
        "a two-parent dest-native commit is outside this decision's scope"
    );
}

#[test]
fn step_one_considers_every_replayable_pending_mapping() {
    let fx = lockstep_fixture();
    let (s1, d1) = main_ahead_by_one(&fx);
    let inputs = proof_inputs(&fx, None);
    let repo = &fx.source_repo;
    let key = marker::test_key();
    let feature_tip_on_dest = inputs.dest_tip;
    let d0 = repo.find_commit(d1).unwrap().parent_id(0).unwrap();
    let q = native_dest_commit(repo, &[d1], tree_with(repo, d1, "q.txt", "q\n"));
    let n = craft_source_marker(
        repo,
        s1,
        tree_with(repo, s1, "q.txt", "q\n"),
        MarkerDirection::DestToSource,
        q,
        &key,
    );
    let s2 = source_commit_on(repo, s1, "s2.txt");

    assert_eq!(
        plan_against(&fx, n, &[(n, q)]),
        reuse::ReusePlan::Replay,
        "an import never triggers reuse: replay loop-prevents it"
    );
    assert_eq!(
        plan_against(&fx, n, &[(n, q), (s1, d1)]),
        reuse::ReusePlan::Bridge {
            shared_source: s1,
            projection: d1
        },
        "an import above an older noncontained replayable mapping selects the older one"
    );
    assert_eq!(
        plan_against(&fx, s2, &[(s2, feature_tip_on_dest), (s1, d1)]),
        reuse::ReusePlan::Bridge {
            shared_source: s1,
            projection: d1
        },
        "a newest mapping contained in T doesn't hide an older noncontained one"
    );
    let incomparable = native_dest_commit(repo, &[d0], tree_with(repo, d0, "other.txt", "o\n"));
    assert!(
        matches!(
            plan_against(&fx, s2, &[(s2, feature_tip_on_dest), (s1, d1), (s1, incomparable)]),
            reuse::ReusePlan::Halt(message) if message.contains("contradictory")
        ),
        "an older Contradictory mapping halts even below a contained one"
    );
    assert_eq!(
        plan_against(&fx, s2, &[(s2, feature_tip_on_dest), (s1, d0)]),
        reuse::ReusePlan::Replay,
        "when every observed mapping is contained in T, replay runs"
    );
}

#[test]
fn a_fresh_clone_reconstructs_both_lockstep_advances_onto_mains_projections() {
    let fx = lockstep_fixture();
    let first = add_commit(&fx.source_repo, "main", &[("s1.txt", "s1\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let d1 = branch_tip(&fx.dest_repo, "main");
    set_branch(&fx.source_repo, "feature", first);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let second = add_commit(&fx.source_repo, "main", &[("s2.txt", "s2\n")]);
    run(fx.source_dir.path(), fx.config.path()).unwrap();
    let d2 = branch_tip(&fx.dest_repo, "main");
    set_branch(&fx.source_repo, "feature", second);
    run(fx.source_dir.path(), fx.config.path()).unwrap();

    // A clone with nothing but the two source branches.
    let clone_dir = tempdir().unwrap();
    let clone = Repository::init(clone_dir.path()).unwrap();
    let source_url = fx.source_repo.path().display().to_string();
    for branch in ["main", "feature"] {
        git::fetch(clone_dir.path(), &source_url, branch).unwrap();
        let tip = clone
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        clone.branch(branch, &tip, false).unwrap();
    }
    let config = Config::load(fx.config.path()).unwrap();
    let dest_url = config.dest_url().unwrap();
    let (source_branches, _) = list_source_branches(&clone).unwrap();
    let listing = git::remote_branch_names(clone_dir.path(), &dest_url).unwrap();
    git::fetch_heads_into_namespace(clone_dir.path(), &dest_url, &listing.names).unwrap();
    let index = reconstruct_mapping_index(
        &clone,
        clone_dir.path(),
        &dest_url,
        &listing,
        &source_branches,
        &marker::test_key(),
        &mut RunCache::default(),
    )
    .unwrap();
    assert_eq!(index.resolve(&clone, first).unwrap().unwrap().dest, d1);
    assert_eq!(index.resolve(&clone, second).unwrap().unwrap().dest, d2);
}

#[test]
fn a_configured_branch_keeps_round_tripping_after_a_bridge() {
    let mut fx = lockstep_fixture();
    let (_, _) = main_ahead_by_one(&fx);
    run(fx.source_dir.path(), fx.config.path()).expect("feature gets its bridge");
    let bridge = branch_tip(&fx.dest_repo, "feature");
    assert_eq!(fx.dest_repo.find_commit(bridge).unwrap().parent_count(), 2);

    fx.config = write_config(
        &fx.source_remote_url,
        &fx.dest_dir.path().display().to_string(),
        &["main", "feature"],
    );
    add_independent_dest_commit_on(
        &fx.dest_repo,
        "feature",
        bridge,
        ("pr.txt", "merged PR\n"),
        "merged PR",
    );
    run(fx.source_dir.path(), fx.config.path())
        .expect("dest→source must import past the bridge without aborting");
    let feature_source = fx
        .source_repo
        .find_commit(branch_tip(&fx.source_repo, "feature"))
        .unwrap();
    assert!(feature_source.tree().unwrap().get_name("pr.txt").is_some());

    add_commit(&fx.source_repo, "feature", &[("next.txt", "next\n")]);
    run(fx.source_dir.path(), fx.config.path()).expect("feature keeps syncing both ways");
    let dest_feature = fx
        .dest_repo
        .find_commit(branch_tip(&fx.dest_repo, "feature"))
        .unwrap();
    assert!(dest_feature.tree().unwrap().get_name("next.txt").is_some());
    assert!(dest_feature.tree().unwrap().get_name("pr.txt").is_some());
    run(fx.source_dir.path(), fx.config.path()).expect("a further run is a clean no-op");
}
