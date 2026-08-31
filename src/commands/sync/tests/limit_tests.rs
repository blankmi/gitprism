use super::*;

#[test]
fn pending_commits_accepts_the_commit_limit_but_rejects_one_more() {
    let dir = tempdir().unwrap();
    let repo = Repository::init_bare(dir.path()).unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let empty_tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();

    let boundary = repo
        .commit(None, &signature, &signature, "root", &empty_tree, &[])
        .unwrap();
    let mut tip = boundary;
    let mut at_limit = None;
    for i in 0..limits::MAX_PENDING_COMMITS + 1 {
        let parent = repo.find_commit(tip).unwrap();
        tip = repo
            .commit(None, &signature, &signature, "c", &empty_tree, &[&parent])
            .unwrap();
        if i + 1 == limits::MAX_PENDING_COMMITS {
            at_limit = Some(tip);
        }
    }
    let at_limit = at_limit.unwrap();

    let pending = pending_commits(&repo, boundary, at_limit)
        .expect("exactly the pending-commit limit must still be accepted");
    assert_eq!(pending.len(), limits::MAX_PENDING_COMMITS);

    let error = pending_commits(&repo, boundary, tip)
        .expect_err("one more than the pending-commit limit must be rejected");
    assert!(error.to_string().contains("commit limit"));
}

#[test]
fn list_source_branches_accepts_the_branch_limit_but_rejects_one_more() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    let root = repo
        .commit(None, &signature, &signature, "root", &tree, &[])
        .unwrap();
    let root_commit = repo.find_commit(root).unwrap();

    for i in 0..limits::MAX_SOURCE_BRANCHES {
        repo.branch(&format!("b{i:07}"), &root_commit, false)
            .unwrap();
    }
    let (branches, skipped) = list_source_branches(&repo)
        .expect("exactly the source-branch limit must still be accepted");
    assert_eq!(branches.len(), limits::MAX_SOURCE_BRANCHES);
    assert!(skipped.is_empty());

    repo.branch("one-more", &root_commit, false).unwrap();
    let error = list_source_branches(&repo)
        .err()
        .expect("one more than the source-branch limit must be rejected");
    assert!(error.to_string().contains("branch limit"));
}

// M-01: a non-UTF-8 branch name is pushed into `skipped`, not
// `source_branches`, so it must still consume the enumeration budget on
// its own — otherwise a repository with more than
// `MAX_SOURCE_BRANCHES` non-UTF-8 refs drives unbounded enumeration,
// allocation, and warning output past decisions/0032's declared bound.
#[cfg(unix)]
#[test]
fn list_source_branches_bails_when_non_utf8_branches_alone_exceed_the_limit() {
    let dir = tempdir().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = Signature::now("A Developer", "dev@example.com").unwrap();
    // Detached: no loose ref needed, only an oid for packed-refs to point at.
    let root = repo
        .commit(None, &signature, &signature, "root", &tree, &[])
        .unwrap();

    // See list_source_branches_warns_about_and_skips_a_non_utf8_branch_name
    // for why packed-refs is the only way to get a genuinely non-UTF-8 ref
    // name past git2's/the filesystem's own validation.
    let mut packed_refs = Vec::new();
    packed_refs.extend_from_slice(b"# pack-refs with: peeled fully-peeled sorted\n");
    for i in 0..=limits::MAX_SOURCE_BRANCHES {
        packed_refs.extend_from_slice(root.to_string().as_bytes());
        packed_refs.push(b' ');
        packed_refs.extend_from_slice(b"refs/heads/feature-");
        packed_refs.extend_from_slice(format!("{i:05}").as_bytes());
        packed_refs.extend_from_slice(&[0xFF, 0xFE]);
        packed_refs.push(b'\n');
    }
    std::fs::write(repo.path().join("packed-refs"), packed_refs).unwrap();

    let error = list_source_branches(&repo).err().expect(
        "non-UTF-8 branches alone must still count against the branch limit and bail past it",
    );
    assert!(error.to_string().contains("branch limit"));
}

#[cfg(unix)]
#[test]
fn list_source_branches_accepts_the_branch_limit_with_a_mix_of_valid_and_non_utf8_names() {
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

    let valid_count = limits::MAX_SOURCE_BRANCHES / 2;
    for i in 0..valid_count {
        repo.branch(&format!("b{i:07}"), &root_commit, false)
            .unwrap();
    }
    // "main" from the initial commit is also a real, valid branch.
    let non_utf8_count = limits::MAX_SOURCE_BRANCHES - valid_count - 1;

    let mut packed_refs = Vec::new();
    packed_refs.extend_from_slice(b"# pack-refs with: peeled fully-peeled sorted\n");
    for i in 0..non_utf8_count {
        packed_refs.extend_from_slice(root.to_string().as_bytes());
        packed_refs.push(b' ');
        packed_refs.extend_from_slice(b"refs/heads/nonutf8-");
        packed_refs.extend_from_slice(format!("{i:07}").as_bytes());
        packed_refs.extend_from_slice(&[0xFF, 0xFE]);
        packed_refs.push(b'\n');
    }
    std::fs::write(repo.path().join("packed-refs"), packed_refs).unwrap();

    let (branches, skipped) = list_source_branches(&repo).expect(
        "exactly the branch limit, mixing valid and non-UTF-8 names, must still be accepted",
    );
    assert_eq!(branches.len() + skipped.len(), limits::MAX_SOURCE_BRANCHES);
    assert_eq!(skipped.len(), non_utf8_count);
}
