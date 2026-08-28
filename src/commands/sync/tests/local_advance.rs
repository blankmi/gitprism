use super::*;

#[test]
fn dirty_checked_out_branch_fails_preflight_before_push() {
    let (dir, repo, first, second) = repository_with_commits();
    repo.reference("refs/heads/main", first, true, "restore test branch")
        .unwrap();
    repo.checkout_tree(
        repo.find_commit(first).unwrap().as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "local work\n").unwrap();

    let error = preflight_local_source_branch(&repo, "main", second).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("local working-tree or index changes")
    );
}

#[test]
fn staged_checked_out_branch_fails_preflight_before_push() {
    let (dir, repo, first, second) = repository_with_commits();
    repo.reference("refs/heads/main", first, true, "restore test branch")
        .unwrap();
    repo.checkout_tree(
        repo.find_commit(first).unwrap().as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "staged work\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    index.write().unwrap();

    let error = preflight_local_source_branch(&repo, "main", second).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("local working-tree or index changes")
    );
}

#[test]
fn unrelated_untracked_file_is_allowed_by_preflight() {
    let (dir, repo, first, second) = repository_with_commits();
    repo.reference("refs/heads/main", first, true, "restore test branch")
        .unwrap();
    repo.checkout_tree(
        repo.find_commit(first).unwrap().as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();
    std::fs::write(dir.path().join("unrelated.txt"), "keep me\n").unwrap();

    assert_eq!(
        preflight_local_source_branch(&repo, "main", second).unwrap(),
        first
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("unrelated.txt")).unwrap(),
        "keep me\n"
    );
}

#[test]
fn colliding_untracked_file_fails_preflight() {
    let (dir, repo, first, second) = repository_with_commits();
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let second_commit = repo.find_commit(second).unwrap();
    let blob = repo.blob(b"new file\n").unwrap();
    let mut tree_builder = repo
        .treebuilder(Some(&second_commit.tree().unwrap()))
        .unwrap();
    tree_builder.insert("new.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree_builder.write().unwrap()).unwrap();
    let third = repo
        .commit(
            Some("refs/heads/target"),
            &signature,
            &signature,
            "add new file",
            &tree,
            &[&second_commit],
        )
        .unwrap();
    drop(tree);
    drop(second_commit);
    repo.reference("refs/heads/main", first, true, "restore test branch")
        .unwrap();
    repo.checkout_tree(
        repo.find_commit(first).unwrap().as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();
    std::fs::write(dir.path().join("new.txt"), "untracked collision\n").unwrap();

    let error = preflight_local_source_branch(&repo, "main", third).unwrap_err();
    assert!(error.to_string().contains("untracked or ignored path"));
}

#[cfg(unix)]
#[test]
fn invalid_byte_paths_are_compared_without_utf8_conversion() {
    let target = b"bad-\xff.txt";
    assert!(git_paths_conflict(target, target));
    assert!(git_paths_conflict(target, b"bad-\xff.txt/child"));
    assert!(!git_paths_conflict(target, b"bad-.txt"));
}

#[test]
fn path_collision_requires_a_component_boundary() {
    assert!(!git_paths_conflict(b"foobar", b"foo"));
    assert!(git_paths_conflict(b"foo/bar", b"foo"));
    assert!(git_paths_conflict(b"foo", b"foo/bar"));
}

#[test]
fn colliding_ignored_file_fails_preflight() {
    let (dir, repo, first, second) = repository_with_commits();
    let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
    let second_commit = repo.find_commit(second).unwrap();
    let blob = repo.blob(b"ignored target\n").unwrap();
    let mut tree_builder = repo
        .treebuilder(Some(&second_commit.tree().unwrap()))
        .unwrap();
    tree_builder.insert("ignored.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree_builder.write().unwrap()).unwrap();
    let third = repo
        .commit(
            Some("refs/heads/target"),
            &signature,
            &signature,
            "add ignored target",
            &tree,
            &[&second_commit],
        )
        .unwrap();
    drop(tree);
    drop(second_commit);
    repo.reference("refs/heads/main", first, true, "restore test branch")
        .unwrap();
    repo.checkout_tree(
        repo.find_commit(first).unwrap().as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .unwrap();
    std::fs::write(dir.path().join(".git/info/exclude"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "local ignored file\n").unwrap();

    let error = preflight_local_source_branch(&repo, "main", third).unwrap_err();
    assert!(error.to_string().contains("untracked or ignored path"));
}

#[test]
fn compare_and_swap_does_not_overwrite_a_concurrent_ref_move() {
    let (dir, repo, first, second) = repository_with_commits();
    let signature = Signature::now("concurrent", "concurrent@example.com").unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "concurrent\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked.txt")).unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let first_commit = repo.find_commit(first).unwrap();
    let third = repo
        .commit(
            Some("refs/heads/concurrent"),
            &signature,
            &signature,
            "concurrent move",
            &tree,
            &[&first_commit],
        )
        .unwrap();
    drop(tree);
    drop(first_commit);
    repo.set_head_detached(first).unwrap();
    repo.reference("refs/heads/main", third, true, "concurrent Git move")
        .unwrap();

    let error = advance_local_source_branch(&repo, "main", second, first).unwrap_err();
    assert!(error.to_string().contains("refusing to overwrite"));
    assert_eq!(
        repo.find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap(),
        third
    );
}
