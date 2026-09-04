//! Binary smoke tests (TEST-001 step 5). Runs the real compiled binary via
//! `env!("CARGO_BIN_EXE_gitprism")` — the crate has no `[lib]` target, so
//! this external integration test binary has no access to gitprism's
//! internals at all, only its process boundary (stdin/stdout/stderr, exit
//! status).

use std::process::Command;

use tempfile::tempdir;

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
    assert!(
        stderr.contains("git repository") || stderr.contains("GITPRISM_STATE_KEY"),
        "expected the repository or state-key requirement named in stderr, got: {stderr:?}"
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
