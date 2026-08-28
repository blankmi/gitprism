use super::*;

#[test]
fn filter_tree_drops_an_excluded_directory_wholesale() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let deep_blob = repo.blob(b"deep").unwrap();
    let mut nested_builder = repo.treebuilder(None).unwrap();
    nested_builder
        .insert("deep.txt", deep_blob, git2::FileMode::Blob.into())
        .unwrap();
    let nested_tree = nested_builder.write().unwrap();

    let inner_blob = repo.blob(b"inner").unwrap();
    let mut secrets_builder = repo.treebuilder(None).unwrap();
    secrets_builder
        .insert("inner.txt", inner_blob, git2::FileMode::Blob.into())
        .unwrap();
    secrets_builder
        .insert("nested", nested_tree, git2::FileMode::Tree.into())
        .unwrap();
    let secrets_tree = secrets_builder.write().unwrap();

    let shared_blob = repo.blob(b"shared").unwrap();
    let mut root_builder = repo.treebuilder(None).unwrap();
    root_builder
        .insert("shared.txt", shared_blob, git2::FileMode::Blob.into())
        .unwrap();
    root_builder
        .insert("secrets", secrets_tree, git2::FileMode::Tree.into())
        .unwrap();
    let root_tree = repo.find_tree(root_builder.write().unwrap()).unwrap();

    let exclude_list = ExcludeList::from_contents("secrets/\n").unwrap();
    let filtered_oid = filter_tree(&repo, &root_tree, Path::new(""), &exclude_list)
        .expect("filtering a tree with an excluded directory should succeed");
    let filtered = repo.find_tree(filtered_oid).unwrap();

    assert!(filtered.get_name("shared.txt").is_some());
    assert!(
        filtered.get_name("secrets").is_none(),
        "an excluded directory must not appear at all, not even as an empty subtree"
    );
    assert_eq!(
        filtered.iter().count(),
        1,
        "the excluded directory must not be recursed into and re-added empty"
    );
}

#[test]
fn filter_tree_preserves_file_modes_and_symlinks() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();

    let regular_blob = repo.blob(b"regular").unwrap();
    let exec_blob = repo.blob(b"#!/bin/sh\n").unwrap();
    let link_blob = repo.blob(b"target.txt").unwrap();
    let excluded_blob = repo.blob(b"secret").unwrap();

    let mut builder = repo.treebuilder(None).unwrap();
    builder
        .insert("regular.txt", regular_blob, git2::FileMode::Blob.into())
        .unwrap();
    builder
        .insert("run.sh", exec_blob, git2::FileMode::BlobExecutable.into())
        .unwrap();
    builder
        .insert("link.txt", link_blob, git2::FileMode::Link.into())
        .unwrap();
    builder
        .insert("secret.txt", excluded_blob, git2::FileMode::Blob.into())
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();

    let exclude_list = ExcludeList::from_contents("secret.txt\n").unwrap();
    let filtered_oid = filter_tree(&repo, &tree, Path::new(""), &exclude_list)
        .expect("filtering a tree with mixed filemodes should succeed");
    let filtered = repo.find_tree(filtered_oid).unwrap();

    assert_eq!(
        filtered.get_name("regular.txt").unwrap().filemode(),
        i32::from(git2::FileMode::Blob)
    );
    assert_eq!(
        filtered.get_name("run.sh").unwrap().filemode(),
        i32::from(git2::FileMode::BlobExecutable)
    );
    assert_eq!(
        filtered.get_name("link.txt").unwrap().filemode(),
        i32::from(git2::FileMode::Link)
    );
    assert!(
        filtered.get_name("secret.txt").is_none(),
        "an excluded file must still be dropped alongside preserving the others' filemodes"
    );
}

#[test]
fn filter_tree_directory_only_pattern_matches_a_submodule_gitlink_but_not_a_symlink() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let exclude_list = ExcludeList::from_contents("vendor-secret/\n").unwrap();

    let gitlink_target = repo
        .commit(
            None,
            &git2::Signature::now("Test", "test@example.com").unwrap(),
            &git2::Signature::now("Test", "test@example.com").unwrap(),
            "submodule commit",
            &repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap(),
            &[],
        )
        .unwrap();
    let shared_blob = repo.blob(b"shared").unwrap();
    let mut gitlink_builder = repo.treebuilder(None).unwrap();
    gitlink_builder
        .insert("shared.txt", shared_blob, git2::FileMode::Blob.into())
        .unwrap();
    gitlink_builder
        .insert(
            "vendor-secret",
            gitlink_target,
            git2::FileMode::Commit.into(),
        )
        .unwrap();
    let gitlink_tree = repo.find_tree(gitlink_builder.write().unwrap()).unwrap();

    let filtered_oid = filter_tree(&repo, &gitlink_tree, Path::new(""), &exclude_list)
        .expect("filtering a tree with a submodule gitlink should succeed");
    let filtered = repo.find_tree(filtered_oid).unwrap();
    assert!(filtered.get_name("shared.txt").is_some());
    assert!(
        filtered.get_name("vendor-secret").is_none(),
        "a directory-only exclude pattern must match a submodule gitlink of the same name"
    );

    let link_blob = repo.blob(b"target.txt").unwrap();
    let mut symlink_builder = repo.treebuilder(None).unwrap();
    symlink_builder
        .insert("vendor-secret", link_blob, git2::FileMode::Link.into())
        .unwrap();
    let symlink_tree = repo.find_tree(symlink_builder.write().unwrap()).unwrap();

    let filtered_oid = filter_tree(&repo, &symlink_tree, Path::new(""), &exclude_list)
        .expect("filtering a tree with a symlink should succeed");
    let filtered = repo.find_tree(filtered_oid).unwrap();
    assert!(
        filtered.get_name("vendor-secret").is_some(),
        "a directory-only exclude pattern must not match a symlink of the same name"
    );
}
