//! Binary smoke tests (TEST-001 step 5). Runs the real compiled binary via
//! `env!("CARGO_BIN_EXE_gitprism")` — the crate has no `[lib]` target, so
//! this external integration test binary has no access to gitprism's
//! internals at all, only its process boundary (stdin/stdout/stderr, exit
//! status).

use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

/// decisions/0050 (CODE-010) needs no real secrecy — a fixed key lets every
/// invocation in the fixture below agree on it without threading state
/// through the test.
const STATE_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Runs a real `git` subprocess against `cwd`, with a fixed author/committer
/// identity (`GIT_AUTHOR_*`/`GIT_COMMITTER_*` bypass config lookup entirely,
/// so no global git config is required) and an isolated global config file
/// so this fixture never reads the machine's real `~/.gitconfig`.
fn git(args: &[&str], cwd: &Path, isolated_global_config: &Path) -> Output {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Author")
        .env("GIT_COMMITTER_EMAIL", "author@example.com")
        .env("GIT_CONFIG_GLOBAL", isolated_global_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|error| panic!("running git {args:?} in {}: {error}", cwd.display()));
    assert!(
        output.status.success(),
        "git {args:?} in {} failed ({:?}): {}",
        cwd.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Runs the real compiled `gitprism` binary against `cwd`, with the fixed
/// [`STATE_KEY`] and `policy_digest` set — every other precondition
/// (`.gitprism.toml`/`.gitprismignore` on disk, `git` itself) is real.
fn gitprism(subcommand: &str, cwd: &Path, policy_digest: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gitprism"))
        .arg(subcommand)
        .current_dir(cwd)
        .env("GITPRISM_STATE_KEY", STATE_KEY)
        .env("GITPRISM_POLICY_SHA256", policy_digest)
        .env_remove("GITPRISM_SOURCE_URL")
        .env_remove("GITPRISM_DEST_URL")
        .output()
        .unwrap_or_else(|error| {
            panic!(
                "running gitprism {subcommand} in {}: {error}",
                cwd.display()
            )
        })
}

/// The advertised OID for `refs/heads/<branch>` on a real remote, read the
/// same way gitprism itself does — a real `git ls-remote` subprocess, not an
/// assumption about what was pushed.
fn remote_branch_tip(remote: &Path, branch: &str, isolated_global_config: &Path) -> String {
    let output = git(
        &[
            "ls-remote",
            "--exit-code",
            &remote.display().to_string(),
            &format!("refs/heads/{branch}"),
        ],
        remote,
        isolated_global_config,
    );
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_else(|| panic!("git ls-remote for {branch:?} produced no OID"))
        .to_string()
}

#[test]
fn version_flag_exits_zero_and_prints_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_gitprism"))
        .arg("--version")
        .output()
        .expect("running gitprism --version");
    assert!(
        output.status.success(),
        "expected --version to exit 0, got {:?}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected the crate version in --version output, got: {stdout:?}"
    );
}

#[test]
fn sync_in_an_empty_directory_with_no_env_fails_and_names_the_requirement() {
    let dir = tempdir().expect("creating an empty temp dir");
    // Deliberately not a git repository at all, and no
    // GITPRISM_STATE_KEY/GITPRISM_POLICY_SHA256 — whichever precondition
    // gitprism reports first, the message must name a real requirement
    // (the repository or the state key), not exit silently or with an
    // unrelated panic.
    let output = Command::new(env!("CARGO_BIN_EXE_gitprism"))
        .arg("sync")
        .current_dir(dir.path())
        .env_remove("GITPRISM_STATE_KEY")
        .env_remove("GITPRISM_POLICY_SHA256")
        .output()
        .expect("running gitprism sync");
    assert!(
        !output.status.success(),
        "sync in an empty directory with no env must not exit 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Repository discovery runs before state-key validation (F-14, see
    // src/commands/sync/mod.rs's `run_with`), and the fixture directory is
    // deliberately not a repository with both env vars absent, so this is
    // deterministic: it must always be the repository error, never the
    // state-key one.
    assert!(
        stderr.contains("git repository"),
        "expected the repository requirement named in stderr, got: {stderr:?}"
    );
}

#[test]
fn policy_hash_prints_the_known_answer_digest_for_fixed_file_contents() {
    // This digest is a known answer for exactly these two files' bytes,
    // shared by value with `policy::tests::
    // digest_bytes_matches_the_known_answer_shared_with_tests_cli_rs` in
    // src/policy.rs — that unit test is what guards this literal actually
    // matches `digest_bytes`'s real output; update both together.
    const KNOWN_ANSWER_DIGEST: &str =
        "0dc03d353c0e6daa24478eb68ffeb59e6a65407a58dd2c8890ea18d6ac340255";

    let dir = tempdir().expect("creating a fixture repo dir");
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .expect("running git init for the fixture repo");
    assert!(init.success(), "git init must succeed for the fixture repo");

    std::fs::write(dir.path().join(".gitprism.toml"), b"config bytes\n")
        .expect("writing the fixture .gitprism.toml");
    std::fs::write(dir.path().join(".gitprismignore"), b"ignore bytes\n")
        .expect("writing the fixture .gitprismignore");

    let output = Command::new(env!("CARGO_BIN_EXE_gitprism"))
        .arg("policy-hash")
        .current_dir(dir.path())
        .output()
        .expect("running gitprism policy-hash");
    assert!(
        output.status.success(),
        "expected policy-hash to exit 0, got {:?} (stderr: {})",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim(),
        KNOWN_ANSWER_DIGEST,
        "policy-hash must print the known-answer digest for these fixed bytes"
    );
}

/// decisions/0050 (CODE-010), end to end through the real compiled binary
/// and real bare source/dest remotes — the shape unit-level fixtures can't
/// prove: the local and remote source tips here are genuinely distinct
/// objects on disk that `gitprism` can only tell apart by asking the
/// remote, not by comparing values already held in the same test process.
///
/// Also covers, in one fixture: a genuine per-branch halt exits nonzero
/// while an unaffected mirror-only branch still syncs in the same run
/// (decisions/0045), the documented `merge --ff-only` recovery
/// (design/playbooks/0003), and that a second rerun after recovery is a
/// no-op.
#[test]
fn sync_halts_a_stale_fetched_not_pulled_clone_and_recovers_via_ff_only_merge() {
    let global_git_config = tempdir().expect("creating an isolated global git config dir");
    let global_git_config = global_git_config.path().join("gitconfig");
    std::fs::write(&global_git_config, b"").expect("creating an empty isolated global git config");

    // dest: a real bare remote, seeded via an ordinary push from a scratch
    // working repo (a bare repo has no working tree of its own to commit
    // into directly).
    let dest_dir = tempdir().expect("creating dest's bare repo dir");
    git(
        &["init", "--quiet", "--bare", "--initial-branch=main"],
        dest_dir.path(),
        &global_git_config,
    );
    let seed_dir = tempdir().expect("creating a scratch seed repo dir");
    git(
        &["init", "--quiet", "--initial-branch=main"],
        seed_dir.path(),
        &global_git_config,
    );
    std::fs::write(seed_dir.path().join("shared.txt"), b"v1\n").expect("seeding dest's content");
    git(&["add", "-A"], seed_dir.path(), &global_git_config);
    git(
        &["commit", "--quiet", "-m", "initial"],
        seed_dir.path(),
        &global_git_config,
    );
    git(
        &[
            "push",
            "--quiet",
            &dest_dir.path().display().to_string(),
            "main",
        ],
        seed_dir.path(),
        &global_git_config,
    );

    // true_source: a real, non-bare repo that both plays the role of "the
    // source remote" (`[source].url` below points straight at it) and is
    // where `gitprism setup`/an initial `gitprism sync` run, exactly as an
    // operator would run them directly against source's own checkout.
    let true_source_dir = tempdir().expect("creating source's repo dir");
    git(
        &["init", "--quiet", "--initial-branch=main"],
        true_source_dir.path(),
        &global_git_config,
    );
    std::fs::write(
        true_source_dir.path().join(".gitprism.toml"),
        format!(
            "branches = [\"main\"]\n\n\
             [committer]\n\
             name = \"gitprism\"\n\
             email = \"gitprism@example.com\"\n\n\
             [source]\n\
             url = '{}'\n\n\
             [dest]\n\
             url = '{}'\n",
            true_source_dir.path().display(),
            dest_dir.path().display(),
        ),
    )
    .expect("writing .gitprism.toml");
    std::fs::write(true_source_dir.path().join(".gitprismignore"), b"")
        .expect("writing .gitprismignore");

    let policy_hash_output = gitprism("policy-hash", true_source_dir.path(), "");
    assert!(
        policy_hash_output.status.success(),
        "gitprism policy-hash must succeed: {}",
        String::from_utf8_lossy(&policy_hash_output.stderr)
    );
    let policy_digest = String::from_utf8_lossy(&policy_hash_output.stdout)
        .trim()
        .to_string();

    let setup_output = gitprism("setup", true_source_dir.path(), &policy_digest);
    assert!(
        setup_output.status.success(),
        "gitprism setup must succeed: {}",
        String::from_utf8_lossy(&setup_output.stderr)
    );

    // A mirror-only "task" branch, first commit (S1).
    git(
        &["checkout", "--quiet", "-b", "task"],
        true_source_dir.path(),
        &global_git_config,
    );
    std::fs::write(true_source_dir.path().join("t1.txt"), b"1\n").expect("writing t1.txt");
    git(&["add", "-A"], true_source_dir.path(), &global_git_config);
    git(
        &["commit", "--quiet", "-m", "task: first commit"],
        true_source_dir.path(),
        &global_git_config,
    );

    let first_sync = gitprism("sync", true_source_dir.path(), &policy_digest);
    assert!(
        first_sync.status.success(),
        "the initial sync must mirror task to dest: {}",
        String::from_utf8_lossy(&first_sync.stderr)
    );
    let s1 = remote_branch_tip(true_source_dir.path(), "task", &global_git_config);

    // Clone B, taken now — at S1 — with its own local "task" and "other"
    // branches materialized (matching what a workstation or CI checkout
    // actually has locally, not just a remote-tracking ref).
    let clone_b_dir = tempdir().expect("creating clone B's dir");
    git(
        &[
            "clone",
            "--quiet",
            "--branch",
            "task",
            &true_source_dir.path().display().to_string(),
            &clone_b_dir.path().display().to_string(),
        ],
        true_source_dir.path(),
        &global_git_config,
    );

    // A second, unrelated mirror-only branch — present locally in clone B
    // and given its own genuinely new content, to prove decisions/0045's
    // per-branch isolation: it must still sync even while task halts in the
    // very same run.
    git(
        &["checkout", "--quiet", "main"],
        true_source_dir.path(),
        &global_git_config,
    );
    git(
        &["checkout", "--quiet", "-b", "other"],
        true_source_dir.path(),
        &global_git_config,
    );
    git(
        &["checkout", "--quiet", "main"],
        true_source_dir.path(),
        &global_git_config,
    );
    let other_sync = gitprism("sync", true_source_dir.path(), &policy_digest);
    assert!(
        other_sync.status.success(),
        "mirroring the fresh other branch must succeed: {}",
        String::from_utf8_lossy(&other_sync.stderr)
    );
    git(
        &[
            "fetch",
            "--quiet",
            &true_source_dir.path().display().to_string(),
            "other",
        ],
        clone_b_dir.path(),
        &global_git_config,
    );
    git(
        &["checkout", "--quiet", "-b", "other", "FETCH_HEAD"],
        clone_b_dir.path(),
        &global_git_config,
    );
    std::fs::write(clone_b_dir.path().join("o2.txt"), b"2\n").expect("writing o2.txt");
    git(&["add", "-A"], clone_b_dir.path(), &global_git_config);
    git(
        &["commit", "--quiet", "-m", "other: second commit"],
        clone_b_dir.path(),
        &global_git_config,
    );

    // Clone A (true_source_dir itself) advances task to S2 — an ordinary
    // fast-forward, no rewrite — and syncs.
    git(
        &["checkout", "--quiet", "task"],
        true_source_dir.path(),
        &global_git_config,
    );
    std::fs::write(true_source_dir.path().join("t2.txt"), b"2\n").expect("writing t2.txt");
    git(&["add", "-A"], true_source_dir.path(), &global_git_config);
    git(
        &["commit", "--quiet", "-m", "task: second commit"],
        true_source_dir.path(),
        &global_git_config,
    );
    let second_sync = gitprism("sync", true_source_dir.path(), &policy_digest);
    assert!(
        second_sync.status.success(),
        "the ordinary fast-forward sync must succeed: {}",
        String::from_utf8_lossy(&second_sync.stderr)
    );
    let s2 = remote_branch_tip(true_source_dir.path(), "task", &global_git_config);
    assert_ne!(s1, s2);
    let dest_task_at_s2 = remote_branch_tip(dest_dir.path(), "task", &global_git_config);

    // Clone B fetches S2 into its own odb — the exact CODE-010 shape — but
    // never moves its local "task" branch off S1.
    git(
        &[
            "fetch",
            "--quiet",
            &true_source_dir.path().display().to_string(),
            "task",
        ],
        clone_b_dir.path(),
        &global_git_config,
    );
    let clone_b_local_task = git(
        &["rev-parse", "task"],
        clone_b_dir.path(),
        &global_git_config,
    );
    assert_eq!(
        String::from_utf8_lossy(&clone_b_local_task.stdout).trim(),
        s1,
        "the fetch must never move clone B's local task branch"
    );

    let stale_sync = gitprism("sync", clone_b_dir.path(), &policy_digest);
    assert!(
        !stale_sync.status.success(),
        "a stale fetched-not-pulled clone must exit nonzero, not force-push dest"
    );
    let stale_stderr = String::from_utf8_lossy(&stale_sync.stderr);
    assert!(
        stale_stderr.contains("task") && stale_stderr.contains("halted"),
        "stderr must name the halted branch: {stale_stderr}"
    );
    assert!(
        stale_stderr.contains("decisions/0050")
            && stale_stderr.contains("playbooks/0003-recover-from-a-stale-source-checkout-refusal"),
        "stderr must point at decisions/0050 and playbook 0003: {stale_stderr}"
    );
    assert!(
        stale_stderr.contains(&s1) && stale_stderr.contains(&s2),
        "stderr must name both this clone's local tip ({s1}) and the source \
         remote's advertised tip ({s2}): {stale_stderr}"
    );
    assert!(
        !stale_stderr.contains("source branch was rewritten"),
        "a halted condition-5 run must never report the rewrite as applied: {stale_stderr}"
    );

    let dest_task_after_stale_sync = remote_branch_tip(dest_dir.path(), "task", &global_git_config);
    assert_eq!(
        dest_task_after_stale_sync, dest_task_at_s2,
        "the stale clone must never move dest's task ref backwards"
    );
    let dest_other_after_stale_sync =
        remote_branch_tip(dest_dir.path(), "other", &global_git_config);
    let other_tree = git(
        &["ls-tree", "-r", "--name-only", &dest_other_after_stale_sync],
        clone_b_dir.path(),
        &global_git_config,
    );
    assert!(
        String::from_utf8_lossy(&other_tree.stdout).contains("o2.txt"),
        "the unaffected other branch must still sync in the very same halted run"
    );

    // Recovery (design/playbooks/0003): switch to the affected branch,
    // `merge --ff-only` from source, then rerun.
    git(
        &["checkout", "--quiet", "task"],
        clone_b_dir.path(),
        &global_git_config,
    );
    git(
        &[
            "fetch",
            "--quiet",
            &true_source_dir.path().display().to_string(),
            "task",
        ],
        clone_b_dir.path(),
        &global_git_config,
    );
    git(
        &["merge", "--ff-only", "--quiet", "FETCH_HEAD"],
        clone_b_dir.path(),
        &global_git_config,
    );
    let clone_b_local_task_after_merge = git(
        &["rev-parse", "task"],
        clone_b_dir.path(),
        &global_git_config,
    );
    assert_eq!(
        String::from_utf8_lossy(&clone_b_local_task_after_merge.stdout).trim(),
        s2,
        "the ff-only merge must land clone B's local task exactly on S2"
    );

    let recovered_sync = gitprism("sync", clone_b_dir.path(), &policy_digest);
    assert!(
        recovered_sync.status.success(),
        "sync after the ff-only recovery must succeed: {}",
        String::from_utf8_lossy(&recovered_sync.stderr)
    );
    let dest_task_after_recovery = remote_branch_tip(dest_dir.path(), "task", &global_git_config);
    assert_eq!(
        dest_task_after_recovery, dest_task_at_s2,
        "recovery must not move dest's task ref — it was already at S2"
    );

    // A second rerun must be a genuine no-op.
    let rerun = gitprism("sync", clone_b_dir.path(), &policy_digest);
    assert!(
        rerun.status.success(),
        "a second rerun after recovery must succeed: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    let dest_task_after_rerun = remote_branch_tip(dest_dir.path(), "task", &global_git_config);
    assert_eq!(
        dest_task_after_rerun, dest_task_at_s2,
        "a second rerun after recovery must be a no-op"
    );
}
