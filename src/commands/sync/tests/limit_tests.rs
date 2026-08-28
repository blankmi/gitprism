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
