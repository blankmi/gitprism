//! Push/fetch go through a real `git` subprocess, not `git2-rs` — see
//! design/decisions/0002-hybrid-git-backend.md. This is the fast-forward-only
//! network path, so it should inherit the same credential helpers/SSH
//! agent/`GIT_ASKPASS` handling a human running `git` would get, rather than
//! a library reimplementation of it.

use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

/// Git hooks and helpers inherit a subprocess environment. The marker key and
/// resolved URL fallbacks are intentionally removed from every git invocation:
/// repository-controlled hooks must not receive either the mapping secret or
/// credential-bearing remote values. Authentication variables such as
/// `GIT_ASKPASS` remain inherited so normal Git transport behavior is intact.
fn git_command() -> Command {
    let mut command = Command::new("git");
    command.env_remove("GITPRISM_STATE_KEY");
    command.env_remove("GITPRISM_SOURCE_URL");
    command.env_remove("GITPRISM_DEST_URL");
    command
}

fn validate_remote(url: &str) -> Result<()> {
    if url.is_empty() {
        anyhow::bail!("configured remote is empty");
    }
    if url.starts_with('-') {
        anyhow::bail!("configured remote cannot start with '-'");
    }
    if url.chars().any(char::is_control) {
        anyhow::bail!("configured remote contains control characters");
    }
    Ok(())
}

pub(crate) fn validate_branch_name(branch: &str) -> Result<()> {
    if !git2::Branch::name_is_valid(branch)
        .with_context(|| format!("validating branch {branch:?}"))?
    {
        anyhow::bail!("invalid branch name {branch:?}");
    }
    Ok(())
}

/// Render Git path or diagnostic bytes without lossy UTF-8 replacement.
/// Printable UTF-8 remains readable; ASCII controls and backslashes are
/// escaped, and malformed UTF-8 bytes use deterministic `\xNN` escapes.
pub(crate) fn escape_bytes(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                append_escaped_text(&mut escaped, text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if let Ok(text) = std::str::from_utf8(&remaining[..valid]) {
                    append_escaped_text(&mut escaped, text);
                }
                let invalid = remaining[valid];
                let _ = write!(escaped, "\\x{invalid:02X}");
                remaining = &remaining[valid + 1..];
            }
        }
    }
    escaped
}

fn append_escaped_text(output: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            character if character.is_control() => {
                if (character as u32) <= 0xFF {
                    let _ = write!(output, "\\x{:02X}", character as u32);
                } else {
                    let _ = write!(output, "\\u{{{:X}}}", character as u32);
                }
            }
            character => output.push(character),
        }
    }
}

/// Convert Git path bytes to a native path without calling git2's Windows
/// byte-to-path helper, which assumes UTF-8 with an internal unwrap.
pub(crate) fn path_from_git_bytes(bytes: &[u8]) -> Result<&Path> {
    #[cfg(unix)]
    {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(OsStr::from_bytes(bytes)))
    }

    #[cfg(windows)]
    {
        let text = std::str::from_utf8(bytes).with_context(|| {
            format!(
                "Git path {} is not valid UTF-8 on this platform",
                escape_bytes(bytes)
            )
        })?;
        Ok(Path::new(text))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let text = std::str::from_utf8(bytes).with_context(|| {
            format!(
                "Git path {} is not valid UTF-8 on this platform",
                escape_bytes(bytes)
            )
        })?;
        Ok(Path::new(text))
    }
}

fn git_diagnostic(raw: &[u8], remote: Option<&str>) -> String {
    let redacted = remote
        .map(|remote| redact_bytes(raw, remote.as_bytes()))
        .unwrap_or_else(|| raw.to_vec());
    let redacted = escape_bytes(&redacted);
    let mut framed = String::new();
    for character in redacted.chars() {
        if framed.len() + character.len_utf8() > MAX_DIAGNOSTIC_BYTES {
            framed.push('…');
            break;
        }
        framed.push(character);
    }
    framed
}

fn redact_bytes(raw: &[u8], secret: &[u8]) -> Vec<u8> {
    if secret.is_empty() {
        return raw.to_vec();
    }

    let mut redacted = Vec::with_capacity(raw.len());
    let mut cursor = 0;
    while cursor < raw.len() {
        let Some(relative) = raw[cursor..]
            .windows(secret.len())
            .position(|candidate| candidate == secret)
        else {
            redacted.extend_from_slice(&raw[cursor..]);
            break;
        };
        let start = cursor + relative;
        redacted.extend_from_slice(&raw[cursor..start]);
        redacted.extend_from_slice(b"<configured remote>");
        cursor = start + secret.len();
    }
    redacted
}

fn is_non_fast_forward_rejection(raw: &[u8]) -> bool {
    raw.split(|byte| *byte == b'\n').any(|line| {
        let mut fields = line.split(|byte| *byte == b'\t');
        let status = fields.next();
        let reason = fields.next_back().unwrap_or_default();
        status == Some(b"!") && reason.starts_with(b"[rejected]")
    })
}

fn run_git_output(mut command: Command) -> Result<std::process::Output> {
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().context("starting git subprocess")?;
    let stdout = child
        .stdout
        .take()
        .context("capturing git subprocess stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing git subprocess stderr")?;
    let stdout_thread = std::thread::spawn(|| read_bounded(stdout));
    let stderr_thread = std::thread::spawn(|| read_bounded(stderr));
    let status = child.wait().context("waiting for git subprocess")?;
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("reading git subprocess stdout panicked"))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("reading git subprocess stderr panicked"))??;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn read_bounded(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(MAX_DIAGNOSTIC_BYTES + 1);
    let mut buffer = [0; 4096];
    let mut total: usize = 0;
    loop {
        let count = reader
            .read(&mut buffer)
            .context("reading git diagnostics")?;
        if count == 0 {
            break;
        }
        let remaining = MAX_DIAGNOSTIC_BYTES + 1 - retained.len();
        retained.extend_from_slice(&buffer[..count.min(remaining)]);
        total = total.saturating_add(count);
        if total > MAX_DIAGNOSTIC_BYTES + 1 {
            retained.truncate(MAX_DIAGNOSTIC_BYTES + 1);
        }
    }
    Ok(retained)
}

/// Fetch `branch` from `url` into `repo_dir`'s local object database,
/// landing at `FETCH_HEAD` — same as running `git fetch <url> <refspec>` by
/// hand inside `repo_dir`. Quiet: git's own "From <url> / * branch ... ->
/// FETCH_HEAD" summary is raw plumbing output with no framing about which
/// sync phase or branch it belongs to, and looks identical whether it's
/// checking a round-tripped branch or a mirror-only one — callers print
/// their own labeled progress line instead (see `sync.rs`). Real failures
/// (bad ref, network, auth) still surface: `-q` only silences the progress
/// summary, not errors.
pub fn fetch(repo_dir: &Path, url: &str, branch: &str) -> Result<()> {
    validate_remote(url)?;
    validate_branch_name(branch)?;
    let source_ref = format!("refs/heads/{branch}");
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("fetch")
        .arg("-q")
        .arg("--")
        .arg(url)
        .arg(&source_ref)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_git_output(command).context("running git fetch from configured remote")?;

    if !output.status.success() {
        let diagnostic = git_diagnostic(&output.stderr, Some(url));
        anyhow::bail!(
            "git fetch branch {branch:?} from configured remote failed ({}): {diagnostic}",
            output.status
        );
    }

    Ok(())
}

/// Whether `refspec` currently exists as a branch on `url` — a real `git
/// ls-remote --exit-code` subprocess check, run before attempting a [`fetch`]
/// where "doesn't exist yet" is an expected, ordinary outcome rather than a
/// failure: a branch source→dest discovers that was never through `gitprism
/// setup` (decisions/0017 — a brand-new feature branch, say) has no
/// same-named counterpart on dest until this very sync run creates one, and
/// [`fetch`]'s own "no such ref" failure is the wrong shape for that case.
pub fn remote_ref_exists(repo_dir: &Path, url: &str, branch: &str) -> Result<bool> {
    validate_remote(url)?;
    validate_branch_name(branch)?;
    let refname = format!("refs/heads/{branch}");
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("ls-remote")
        .arg("--exit-code")
        .arg("--")
        .arg(url)
        .arg(&refname)
        .stdout(std::process::Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0");
    let output =
        run_git_output(command).context("running git ls-remote against configured remote")?;

    match output.status.code() {
        Some(0) => Ok(true),
        // git's own convention for `--exit-code`: 2 means the query
        // succeeded but matched nothing, distinct from any other failure
        // (bad URL, network, auth, ...).
        Some(2) => Ok(false),
        _ => {
            let diagnostic = git_diagnostic(&output.stderr, Some(url));
            anyhow::bail!(
                "git ls-remote for branch {branch:?} against configured remote failed ({}): {diagnostic}",
                output.status
            )
        }
    }
}

/// What happened to a [`push`] attempt: either it landed, or it was
/// rejected specifically for being a non-fast-forward update — the one
/// failure decisions/0009 says is worth refetching dest and recomputing for.
/// Every other failure (auth, hooks, network, ...) is a plain `Err`.
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    RejectedNotFastForward,
}

/// Push `commit` (a local oid already in `repo_dir`'s object database) to
/// `dest_branch` on `url` — same as `git push <url> <commit>:<dest_branch>`
/// by hand. Deliberately no `--force`: git's own default refuses a
/// non-fast-forward update, which is exactly the fast-forward-only
/// constraint source→dest sync requires (requirements/0001, decisions/0009).
pub fn push(
    repo_dir: &Path,
    url: &str,
    commit: git2::Oid,
    dest_branch: &str,
) -> Result<PushOutcome> {
    validate_remote(url)?;
    validate_branch_name(dest_branch)?;
    let refspec = format!("{commit}:refs/heads/{dest_branch}");
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("push")
        .arg("--porcelain")
        .arg("--")
        .arg(url)
        .arg(&refspec)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_git_output(command).context("running git push to configured remote")?;

    if output.status.success() {
        return Ok(PushOutcome::Accepted);
    }

    // git's own wording for "the ref moved since we last looked" — the only
    // case decisions/0009 wants recomputed and retried. Everything else
    // (bad credentials, a rejecting pre-receive hook, a dropped connection,
    // ...) must surface immediately instead of being silently retried.
    if is_non_fast_forward_rejection(&output.stdout) {
        return Ok(PushOutcome::RejectedNotFastForward);
    }

    let diagnostic = git_diagnostic(
        if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        },
        Some(url),
    );
    anyhow::bail!(
        "git push branch {dest_branch:?} to configured remote failed ({}): {diagnostic}",
        output.status
    );
}

/// Add a detached linked worktree for an interactive resolve operation. The
/// path is passed as an argument, never through a shell, and the worktree is
/// deliberately owned by git so its cherry-pick state survives the command
/// that starts the human resolution.
pub(crate) fn worktree_add(repo_dir: &Path, worktree: &Path, commit: git2::Oid) -> Result<()> {
    let output = git_command()
        .arg("-C")
        .arg(repo_dir)
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(worktree)
        .arg(commit.to_string())
        .output()
        .context("running git worktree add")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!("git worktree add failed ({}): {stderr}", output.status);
    }
    Ok(())
}

pub(crate) fn worktree_remove(repo_dir: &Path, worktree: &Path) -> Result<()> {
    let output = git_command()
        .arg("-C")
        .arg(repo_dir)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree)
        .output()
        .context("running git worktree remove")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!("git worktree remove failed ({}): {stderr}", output.status);
    }
    Ok(())
}

/// What a real `git cherry-pick` subprocess attempt (decisions/0015) came
/// back with. `Clean` covers both "applied with no conflicts" and, for
/// [`cherry_pick_continue`], "the human's resolution is now complete" —
/// either way there's a new commit on the checked-out branch ready for
/// `gitprism resolve` to rebuild with the right identity/trailer. `Conflict`
/// means real, unresolved conflict markers are sitting in the working tree
/// (or still are, for `cherry_pick_continue`) — a human, not a bug.
#[derive(Debug, PartialEq, Eq)]
pub enum CherryPickOutcome {
    Clean,
    Conflict,
}

/// Cherry-picks `commit` onto whatever's currently checked out in `repo_dir`
/// via a real `git cherry-pick` subprocess — deliberately not `git2`, unlike
/// every other cherry-pick in this codebase (`commands::sync`'s is entirely
/// in the object database and never touches the working tree). `resolve`
/// needs the opposite: a real conflict must leave real working-tree conflict
/// markers and `CHERRY_PICK_HEAD` for the human to resolve with ordinary git
/// (decisions/0008, decisions/0015).
///
/// `mainline` is the 1-based parent number for a merge commit (git's own
/// `-m` convention), `None` for an ordinary commit. `GIT_EDITOR=true` keeps
/// even a clean apply from ever blocking on an interactive commit-message
/// prompt — the resulting commit's author/committer/message all get replaced
/// by `gitprism resolve` regardless of whether it applied cleanly or a human
/// had to intervene (decisions/0003, 0010, 0015).
///
/// Told apart from a genuine, unrelated failure (bad sha, dirty working
/// tree, ...) by git's own exit-code convention: `0` clean, `1` a real
/// conflict needing resolution, anything else a plain `Err`.
///
/// `--empty=keep` is always passed: a pending dest commit that cherry-picks
/// to no actual change (e.g. source already has the same content) must still
/// produce its own marker commit, the same rule `sync`'s own no-op cherry-
/// picks already follow (decisions/0007's resume-trailer requirement) —
/// without it, git stops and asks a human to choose `--allow-empty` vs
/// `--skip` even though there was never a real conflict to resolve, and this
/// function would have no way to tell that apart from an actual conflict
/// from the exit code alone.
///
/// `committer_name` and `committer_email` are supplied as Git's committer
/// environment for the temporary commit. The picked commit's author remains
/// untouched, and the final gitprism commit is rebuilt with the same identity
/// through git2.
pub fn cherry_pick(
    repo_dir: &Path,
    commit: git2::Oid,
    mainline: Option<u32>,
    committer_name: &str,
    committer_email: &str,
) -> Result<CherryPickOutcome> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(repo_dir).arg("cherry-pick");
    if let Some(mainline) = mainline {
        cmd.arg("-m").arg(mainline.to_string());
    }
    cmd.arg("--empty=keep");
    cmd.arg(commit.to_string());
    cmd.env("GIT_EDITOR", "true");
    set_committer_identity(&mut cmd, committer_name, committer_email);

    let output = cmd
        .output()
        .with_context(|| format!("running git cherry-pick {commit}"))?;
    cherry_pick_outcome(output, || format!("git cherry-pick {commit}"))
}

/// Starts an interactive cherry-pick without creating a commit. This keeps
/// identity and commit hooks out of the temporary resolution worktree; the
/// final authenticated commit is built by gitprism after the index is staged.
pub(crate) fn cherry_pick_no_commit(
    repo_dir: &Path,
    commit: git2::Oid,
    mainline: Option<u32>,
) -> Result<CherryPickOutcome> {
    let mut cmd = git_command();
    cmd.arg("-C").arg(repo_dir).arg("cherry-pick");
    if let Some(mainline) = mainline {
        cmd.arg("-m").arg(mainline.to_string());
    }
    cmd.arg("--no-commit").arg(commit.to_string());
    let output = cmd
        .output()
        .with_context(|| format!("running git cherry-pick --no-commit {commit}"))?;
    cherry_pick_outcome(output, || format!("git cherry-pick --no-commit {commit}"))
}

/// Finishes a cherry-pick already in progress in `repo_dir` (started by
/// [`cherry_pick`]) after the human has resolved its conflicts and `git
/// add`ed them — the real `git cherry-pick --continue`.
///
/// `--continue` doesn't accept `--empty=keep` itself (git rejects the
/// combination outright), so a conflict a human resolves by keeping source's
/// existing content exactly (a legitimate resolution, not a mistake) still
/// makes `--continue` stop at exit `1` with *no* conflicted paths left in the
/// index — git's own "the previous cherry-pick is now empty" prompt, asking
/// whether to `git commit --allow-empty` or `--skip`. That's told apart from
/// a genuine still-unresolved conflict by whether any unmerged paths remain:
/// none left means finish it by hand with `git commit --allow-empty`, which
/// clears the sequencer state exactly like a normal `--continue` would.
pub fn cherry_pick_continue(
    repo_dir: &Path,
    committer_name: &str,
    committer_email: &str,
) -> Result<CherryPickOutcome> {
    let mut command = git_command();
    set_committer_identity(&mut command, committer_name, committer_email);
    let output = command
        .arg("-C")
        .arg(repo_dir)
        .arg("cherry-pick")
        .arg("--continue")
        .env("GIT_EDITOR", "true")
        .output()
        .context("running git cherry-pick --continue")?;

    match output.status.code() {
        Some(0) => Ok(CherryPickOutcome::Clean),
        Some(1) if !has_unmerged_paths(repo_dir)? => {
            finish_empty_continue(repo_dir, committer_name, committer_email)?;
            Ok(CherryPickOutcome::Clean)
        }
        Some(1) => Ok(CherryPickOutcome::Conflict),
        _ => {
            let stderr = git_diagnostic(&output.stderr, None);
            anyhow::bail!(
                "git cherry-pick --continue failed ({}): {stderr}",
                output.status
            )
        }
    }
}

/// Whether the index still has any unmerged (conflicted) path — used only to
/// tell `cherry_pick_continue`'s two exit-`1` cases apart (a real remaining
/// conflict vs. a fully-resolved-but-empty result).
fn has_unmerged_paths(repo_dir: &Path) -> Result<bool> {
    let output = git_command()
        .arg("-C")
        .arg(repo_dir)
        .arg("ls-files")
        .arg("--unmerged")
        .output()
        .context("checking for unmerged paths")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!(
            "git ls-files --unmerged failed ({}): {stderr}",
            output.status
        );
    }
    Ok(!output.stdout.is_empty())
}

/// Finishes a cherry-pick resolution that turned out empty (see
/// [`cherry_pick_continue`]) the way git itself suggests when it refuses to:
/// a real, empty commit, which finishes the sequence (clears
/// `CHERRY_PICK_HEAD`) exactly like a normal `--continue` would have.
/// `gitprism resolve` immediately replaces whatever commit this produces
/// with its own properly-stamped one, so this commit's own message/identity
/// are never user-visible. Its committer identity still comes from the
/// verified gitprism configuration rather than the operator's Git config.
fn finish_empty_continue(
    repo_dir: &Path,
    committer_name: &str,
    committer_email: &str,
) -> Result<()> {
    let mut command = git_command();
    set_committer_identity(&mut command, committer_name, committer_email);
    let output = command
        .arg("-C")
        .arg(repo_dir)
        .arg("commit")
        .arg("--allow-empty")
        .arg("--no-edit")
        .output()
        .context("running git commit --allow-empty to finish an empty cherry-pick resolution")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!(
            "git commit --allow-empty failed ({}): {stderr}",
            output.status
        );
    }
    Ok(())
}

fn set_committer_identity(command: &mut Command, name: &str, email: &str) {
    command
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email);
}

fn cherry_pick_outcome(
    output: std::process::Output,
    describe: impl FnOnce() -> String,
) -> Result<CherryPickOutcome> {
    match output.status.code() {
        Some(0) => Ok(CherryPickOutcome::Clean),
        // git's own convention: 1 is a real conflict needing resolution,
        // distinct from 128 (a fatal, unrelated usage error) or any other
        // unexpected code.
        Some(1) => Ok(CherryPickOutcome::Conflict),
        _ => {
            let stderr = git_diagnostic(&output.stderr, None);
            anyhow::bail!("{} failed ({}): {stderr}", describe(), output.status)
        }
    }
}

/// The floor [`merge_tree`]'s exact command line needs, which is higher than
/// any single one of its flags would suggest. Three separate additions stack:
/// `--write-tree` itself landed in git 2.38, `--merge-base=<tree-ish>` in
/// 2.40, but passing **raw tree oids** as the two positional arguments — what
/// `merge_tree` does, since a filtered tree has no commit to name it — only
/// became supported and documented in **2.45** (git commit `5f43cf5b2e`,
/// "merge-tree: accept 3 trees as arguments"; the man page's `--merge-base`
/// entry now reads "trees are enough"). On 2.40–2.44 those positions still
/// require commit-ish and this would fail, so 2.45 is the real floor rather
/// than the union of the older two.
///
/// If gitprism ever needs to run against 2.40–2.44, the fix is to wrap each
/// filtered tree in a throwaway commit object rather than to lower this
/// number.
pub const MIN_GIT_VERSION: (u32, u32) = (2, 45);

/// What a real `git merge-tree --write-tree` subprocess (decisions/0016)
/// came back with for one pending commit's 3-way merge. `merge-tree` touches
/// neither the index nor the working tree, which is what lets `sync` build
/// commits straight into the object database without a checkout — the same
/// property decisions/0015 relies on to justify `resolve` being the one
/// place that *does* use a working tree.
#[derive(Debug, PartialEq, Eq)]
pub enum MergeTreeOutcome {
    Clean(git2::Oid),
    Conflict { paths: Vec<String> },
}

/// Computes the 3-way merge of `ours` and `theirs` against `base` — all raw
/// tree oids, no commits needed — via a real `git merge-tree --write-tree`
/// subprocess (decisions/0016). This is the one merge primitive both sync
/// directions now share; filtering (excluded paths) is gitprism's own job,
/// done by building the filtered trees passed in here, not by this function.
///
/// `--no-messages` is load-bearing for the parse below, not cosmetic: with
/// informational messages enabled, `-z` appends a further NUL-terminated
/// section (an "Auto-merging"/"CONFLICT" narration) after the path list that
/// would otherwise be indistinguishable from more conflicted paths.
pub fn merge_tree(
    repo_dir: &Path,
    base: git2::Oid,
    ours: git2::Oid,
    theirs: git2::Oid,
) -> Result<MergeTreeOutcome> {
    let merge_base_arg = format!("--merge-base={base}");
    let output = git_command()
        .arg("-C")
        .arg(repo_dir)
        .arg("merge-tree")
        .arg("--write-tree")
        .arg("-z")
        .arg("--name-only")
        .arg("--no-messages")
        .arg(&merge_base_arg)
        .arg(ours.to_string())
        .arg(theirs.to_string())
        .output()
        .with_context(|| {
            format!("running git merge-tree --write-tree --merge-base={base} {ours} {theirs}")
        })?;

    let mut records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty());

    match output.status.code() {
        Some(0) => {
            let tree_record = records.next().unwrap_or_default();
            let tree_record = std::str::from_utf8(tree_record).with_context(|| {
                format!(
                    "merge-tree reported a non-UTF-8 tree oid {} for \
                     --merge-base={base} {ours} {theirs}",
                    escape_bytes(tree_record)
                )
            })?;
            let oid = git2::Oid::from_str(tree_record.trim()).with_context(|| {
                format!(
                    "parsing merge-tree's reported tree oid {tree_record:?} for \
                     --merge-base={base} {ours} {theirs}"
                )
            })?;
            Ok(MergeTreeOutcome::Clean(oid))
        }
        // git's own convention: 1 is a real conflict needing resolution.
        // The first record here is the tree oid git wrote alongside the
        // conflict — deliberately discarded, not merely unused: that tree
        // contains conflict markers and must never be committed anywhere.
        // Only the exit code decides Clean vs Conflict (decisions/0007:
        // hard-stop, never auto-resolve) — `paths` below exists purely to
        // make that hard-stop message actionable, never as a control-flow
        // input. Paths are escaped byte-wise below so malformed names remain
        // actionable without being interpreted as terminal control data.
        //
        // Leaving that discarded tree (and its blobs) as unreferenced loose
        // objects is accepted, not overlooked: ordinary git garbage on an
        // error path, in a CI checkout, reclaimed by `git gc` — the same
        // residue an aborted `git merge` leaves behind.
        Some(1) => {
            let mut paths: Vec<String> = records.skip(1).map(escape_bytes).collect();
            // The same normalisation resolve::conflicted_paths already
            // does — don't assume git deduplicates stages for us.
            paths.sort();
            paths.dedup();
            Ok(MergeTreeOutcome::Conflict { paths })
        }
        _ => {
            let stderr = git_diagnostic(&output.stderr, None);
            anyhow::bail!(
                "git merge-tree --write-tree --merge-base={base} {ours} {theirs} failed ({}): {stderr}",
                output.status
            )
        }
    }
}

/// Parses the two leading version components out of `git --version`'s own
/// output, e.g. `"git version 2.50.1 (Apple Git-155)"` -> `Some((2, 50))`.
/// Must also tolerate distro/platform suffixes tacked onto the patch
/// component, e.g. `"2.45.1.windows.1"`.
fn parse_git_version(raw: &str) -> Option<(u32, u32)> {
    let version = raw.split_whitespace().nth(2)?;
    let mut components = version.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    Some((major, minor))
}

/// Confirms the `git` on `PATH` is new enough for [`merge_tree`]'s flag set
/// (decisions/0016) — checked once, up front, so an operator sees a clear
/// version/reason message instead of a confusing parse failure the first
/// time `merge_tree` itself runs. Deliberately no `-C repo_dir`: this is a
/// property of the `git` binary, not of any particular repository.
pub fn ensure_merge_tree_supported() -> Result<()> {
    let output = git_command()
        .arg("--version")
        .output()
        .context("running git --version")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!("git --version failed ({}): {stderr}", output.status);
    }

    let raw = std::str::from_utf8(&output.stdout).with_context(|| {
        format!(
            "git --version returned non-UTF-8 output: {}",
            escape_bytes(&output.stdout)
        )
    })?;
    let (major, minor) = parse_git_version(raw).with_context(|| {
        format!(
            "could not parse a version out of {raw:?} — needed to confirm git supports \
             the merge-tree flag set gitprism relies on (--merge-base with raw tree \
             oids, -z --name-only --no-messages), which requires git >= {}.{}",
            MIN_GIT_VERSION.0, MIN_GIT_VERSION.1
        )
    })?;

    if (major, minor) < MIN_GIT_VERSION {
        anyhow::bail!(
            "found git {major}.{minor}, but gitprism's merge-tree-based sync \
             needs git >= {}.{} for the --merge-base/-z/--name-only/--no-messages flag set \
             it depends on",
            MIN_GIT_VERSION.0,
            MIN_GIT_VERSION.1
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use git2::Repository;
    use tempfile::tempdir;

    use super::*;

    /// A repo with one commit (an empty tree) on `branch`, suitable as a
    /// fetch source in tests — no working directory needed.
    fn repo_with_a_commit_on(dir: &Path, branch: &str) -> git2::Oid {
        let repo = Repository::init(dir).unwrap();
        let tree_oid = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "initial",
            &tree,
            &[],
        )
        .unwrap()
    }

    #[test]
    fn fetch_lands_the_remote_branch_at_fetch_head() {
        let dest_dir = tempdir().unwrap();
        let expected = repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
        )
        .expect("fetching an existing branch should succeed");

        let source_repo = Repository::open(source_dir.path()).unwrap();
        let fetched = source_repo
            .find_reference("FETCH_HEAD")
            .expect("FETCH_HEAD should exist after a successful fetch")
            .peel_to_commit()
            .unwrap();

        assert_eq!(fetched.id(), expected);
    }

    #[test]
    fn remote_ref_exists_reports_true_for_a_branch_that_exists() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        assert!(
            remote_ref_exists(
                source_dir.path(),
                &dest_dir.path().display().to_string(),
                "main",
            )
            .expect("checking an existing branch should succeed")
        );
    }

    #[test]
    fn remote_ref_exists_reports_false_for_a_branch_that_does_not_exist() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        assert!(
            !remote_ref_exists(
                source_dir.path(),
                &dest_dir.path().display().to_string(),
                "no-such-branch",
            )
            .expect("checking a missing branch should succeed, just report false")
        );
    }

    #[test]
    fn fetch_fails_loudly_on_an_unknown_ref() {
        let dest_dir = tempdir().unwrap();
        repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        let err = fetch(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "no-such-branch",
        )
        .expect_err("fetching a branch that doesn't exist must not silently succeed");

        assert!(err.to_string().contains("git fetch"));
    }

    #[test]
    fn fetch_rejects_a_raw_refspec_before_touching_fetch_head() {
        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        let err = fetch(
            source_dir.path(),
            "/does/not/matter",
            "refs/heads/main:refs/heads/other",
        )
        .expect_err("fetch accepts branch names, not caller-provided refspecs");

        assert!(err.to_string().contains("invalid branch name"));
        assert!(!source_dir.path().join(".git/FETCH_HEAD").exists());
    }

    #[test]
    fn remote_operands_starting_with_a_dash_are_rejected_before_git_runs() {
        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        let err = remote_ref_exists(source_dir.path(), "--upload-pack=printf", "main")
            .expect_err("remote options must not be passed through to git");

        assert!(err.to_string().contains("cannot start with '-'"));
    }

    /// A commit built directly in `repo`'s object database, without updating
    /// any ref — the shape `sync` actually pushes (a bare oid, not a local
    /// branch).
    fn commit_on(repo: &Repository, parent: git2::Oid, file: (&str, &str)) -> git2::Oid {
        let parent_commit = repo.find_commit(parent).unwrap();
        let mut builder = repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        let blob = repo.blob(file.1.as_bytes()).unwrap();
        builder
            .insert(file.0, blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();

        repo.commit(
            None,
            &signature,
            &signature,
            "second",
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    #[test]
    fn push_fast_forwards_a_bare_dest_branch() {
        let dest_dir = tempdir().unwrap();
        Repository::init_bare(dest_dir.path()).unwrap();

        let source_dir = tempdir().unwrap();
        let base = repo_with_a_commit_on(source_dir.path(), "work");
        let source_repo = Repository::open(source_dir.path()).unwrap();
        let child = commit_on(&source_repo, base, ("b.txt", "b"));

        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            child,
            "main",
        )
        .expect("pushing a fresh branch to an empty bare repo should succeed");
        assert_eq!(outcome, PushOutcome::Accepted);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(dest_tip.id(), child);
    }

    #[test]
    fn push_reports_a_rejection_instead_of_erroring_on_a_lost_fast_forward_race() {
        let dest_dir = tempdir().unwrap();
        let dest_tip = repo_with_a_commit_on(dest_dir.path(), "main");
        // Make dest's checked-out branch something else, so pushing to
        // "main" isn't rejected merely for being the checked-out branch.
        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        dest_repo.set_head("refs/heads/unrelated").unwrap();

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let source_repo = Repository::open(source_dir.path()).unwrap();
        // A commit unrelated to dest's actual tip — pushing it as "main"
        // would discard dest_tip, which a non-forced push must refuse.
        let tree_oid = source_repo.treebuilder(None).unwrap().write().unwrap();
        let tree = source_repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let unrelated = source_repo
            .commit(None, &signature, &signature, "unrelated", &tree, &[])
            .unwrap();

        // decisions/0009: this is the one failure sync should treat as
        // retryable (refetch and recompute), so it comes back as a reported
        // outcome, not an `Err` — that distinction is what lets sync tell it
        // apart from a genuine, non-retryable failure below.
        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            unrelated,
            "main",
        )
        .expect("a lost fast-forward race is a reported outcome, not an error");
        assert_eq!(outcome, PushOutcome::RejectedNotFastForward);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let still = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still.id(),
            dest_tip,
            "a rejected push must not move dest's branch"
        );
    }

    #[test]
    fn push_fails_loudly_on_an_unrelated_error_instead_of_reporting_a_rejection() {
        let source_dir = tempdir().unwrap();
        let commit = repo_with_a_commit_on(source_dir.path(), "main");

        // Not a fast-forward race at all — there's no dest repository here
        // to race with. Decisions/0009 only calls for retrying a lost
        // fast-forward race; every other failure (this one included) must
        // surface as an `Err`, not get misreported as a retryable rejection.
        let err = push(
            source_dir.path(),
            "/nonexistent/not-a-remote",
            commit,
            "main",
        )
        .expect_err("an unreachable remote must not silently succeed");

        assert!(err.to_string().contains("git push"));
    }

    #[test]
    fn push_porcelain_rejection_parser_distinguishes_non_fast_forward() {
        assert!(is_non_fast_forward_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (fetch first)\nDone\n"
        ));
        assert!(is_non_fast_forward_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (non-fast-forward)\n"
        ));
        assert!(is_non_fast_forward_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (razlog lokalizovan)\n"
        ));
        assert!(!is_non_fast_forward_rejection(
            b"!\tHEAD:refs/heads/main\t[remote rejected] (hook declined)\n"
        ));
    }

    #[test]
    fn git_diagnostics_are_bounded_redacted_and_terminal_safe() {
        let raw = b"failed\x1b[31m to push\x1b[0m to 'https://user:secret@example.test/r.git'\n";
        let diagnostic = git_diagnostic(raw, Some("https://user:secret@example.test/r.git"));
        assert!(!diagnostic.contains("secret"));
        assert!(!diagnostic.contains('\x1b'));
        assert!(diagnostic.contains("\\x1B"));

        let large = vec![b'x'; MAX_DIAGNOSTIC_BYTES + 1];
        let diagnostic = git_diagnostic(&large, None);
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES + "…".len());
    }

    #[test]
    fn git_diagnostics_redact_raw_remote_bytes_before_escaping() {
        let remote = r"https://user:secret@example.test/C:\repo\git.git";
        let raw = format!("fatal: unable to access {remote}\n");
        let diagnostic = git_diagnostic(raw.as_bytes(), Some(remote));
        assert!(!diagnostic.contains("secret"));
        assert!(!diagnostic.contains("C:"));
        assert!(diagnostic.contains("<configured remote>"));
    }

    /// A non-bare repo with one commit on `branch` containing `files`,
    /// checked out — `cherry_pick`/`cherry_pick_continue` operate on a real
    /// working tree, unlike every other fixture in this module.
    fn checkout_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> git2::Oid {
        let repo = Repository::init(dir).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let oid = repo
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                "initial",
                &tree,
                &[],
            )
            .unwrap();
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(None).unwrap();
        oid
    }

    /// A commit on top of `parent`, moving `branch` to it — does *not* check
    /// it out, so tests can build a commit meant to be cherry-picked onto a
    /// different, already-checked-out branch.
    fn commit_on_branch(
        repo: &Repository,
        branch: &str,
        parent: git2::Oid,
        files: &[(&str, &str)],
    ) -> git2::Oid {
        let parent_commit = repo.find_commit(parent).unwrap();
        let mut builder = repo
            .treebuilder(Some(&parent_commit.tree().unwrap()))
            .unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a change",
            &tree,
            &[&parent_commit],
        )
        .unwrap()
    }

    #[test]
    fn cherry_pick_reports_clean_and_creates_a_commit_when_it_applies_without_conflict() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("user.name", "local-config-user")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "local-config@example.com")
            .unwrap();
        // A commit that touches a different file entirely — applies cleanly
        // onto "main", which is still checked out at `base`.
        let to_pick = commit_on_branch(&repo, "topic", base, &[("g.txt", "from topic")]);

        let outcome = cherry_pick(
            dir.path(),
            to_pick,
            None,
            "gitprism",
            "gitprism@example.com",
        )
        .expect("a non-conflicting pick should succeed");
        assert_eq!(outcome, CherryPickOutcome::Clean);

        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.parent_id(0).unwrap(), base);
        assert_eq!(head.author().name().unwrap(), "Test");
        assert_eq!(head.committer().name().unwrap(), "gitprism");
        assert_eq!(head.committer().email().unwrap(), "gitprism@example.com");
        assert!(dir.path().join("g.txt").exists());
    }

    #[test]
    fn cherry_pick_reports_conflict_and_leaves_real_conflict_markers() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("user.name", "local-config-user")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "local-config@example.com")
            .unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "from topic")]);
        // main itself diverges on the very same file/line, so picking
        // `to_pick` onto it is a real conflict.
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(base).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "diverging change",
            &tree,
            &[&parent],
        )
        .unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        let outcome = cherry_pick(
            dir.path(),
            to_pick,
            None,
            "gitprism",
            "gitprism@example.com",
        )
        .expect("a real conflict is a reported outcome");
        assert_eq!(outcome, CherryPickOutcome::Conflict);

        assert!(
            dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "a real git conflict must leave CHERRY_PICK_HEAD for ordinary git UX to find"
        );
        let contents = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert!(
            contents.contains("<<<<<<<"),
            "a real conflict must leave real conflict markers in the working tree, not just an in-memory index conflict"
        );
    }

    #[test]
    fn cherry_pick_continue_finishes_once_the_human_resolves_and_adds() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        repo.config()
            .unwrap()
            .set_str("user.name", "local-config-user")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "local-config@example.com")
            .unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "from topic")]);
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("f.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.find_commit(base).unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "diverging change",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        assert_eq!(
            cherry_pick(
                dir.path(),
                to_pick,
                None,
                "gitprism",
                "gitprism@example.com",
            )
            .unwrap(),
            CherryPickOutcome::Conflict
        );

        // The human resolves the conflict and stages it.
        std::fs::write(dir.path().join("f.txt"), "resolved").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        let outcome = cherry_pick_continue(dir.path(), "gitprism", "gitprism@example.com")
            .expect("finishing a fully-resolved cherry-pick should succeed");
        assert_eq!(outcome, CherryPickOutcome::Clean);
        let temporary_commit = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(temporary_commit.committer().name().unwrap(), "gitprism");
        assert_eq!(
            temporary_commit.committer().email().unwrap(),
            "gitprism@example.com"
        );
        assert!(
            !dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "a completed cherry-pick must not leave CHERRY_PICK_HEAD behind"
        );
        let contents = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(contents, "resolved");
    }

    #[test]
    fn cherry_pick_reports_clean_when_the_pick_nets_to_no_change() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "2")]);
        // main independently already has the exact content `to_pick` would
        // introduce — a clean but *empty* merge result, which without
        // `--empty=keep` git refuses to finish on its own (the "previous
        // cherry-pick is now empty" prompt), even though there was never a
        // conflict at all.
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let mut index = repo.index().unwrap();
        std::fs::write(dir.path().join("f.txt"), "2").unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.find_commit(base).unwrap();
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "already matches",
            &tree,
            &[&parent],
        )
        .unwrap();
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();

        let outcome = cherry_pick(
            dir.path(),
            to_pick,
            None,
            "gitprism",
            "gitprism@example.com",
        )
        .expect("an empty-result pick must still succeed thanks to --empty=keep");
        assert_eq!(outcome, CherryPickOutcome::Clean);
        assert!(!dir.path().join(".git/CHERRY_PICK_HEAD").exists());
    }

    #[test]
    fn cherry_pick_continue_finishes_an_empty_resolution_by_committing_it_directly() {
        let dir = tempdir().unwrap();
        let base = checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);
        let repo = Repository::open(dir.path()).unwrap();
        let to_pick = commit_on_branch(&repo, "topic", base, &[("f.txt", "from topic")]);
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("f.txt")).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parent = repo.find_commit(base).unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "diverging change",
                &tree,
                &[&parent],
            )
            .unwrap();
        }
        repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
            .unwrap();
        assert_eq!(
            cherry_pick(
                dir.path(),
                to_pick,
                None,
                "gitprism",
                "gitprism@example.com",
            )
            .unwrap(),
            CherryPickOutcome::Conflict
        );

        // The human resolves the conflict by keeping main's own content
        // exactly — `--continue` can't finish this on its own (`--empty=keep`
        // isn't accepted alongside `--continue`), and must not be
        // misreported as still-conflicted.
        std::fs::write(dir.path().join("f.txt"), "from main").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("f.txt")).unwrap();
        index.write().unwrap();

        let outcome = cherry_pick_continue(dir.path(), "gitprism", "gitprism@example.com")
            .expect("an empty-result resolution must still finish, not be reported as a conflict");
        assert_eq!(outcome, CherryPickOutcome::Clean);
        assert!(
            !dir.path().join(".git/CHERRY_PICK_HEAD").exists(),
            "finishing an empty resolution must clear CHERRY_PICK_HEAD just like a normal continue"
        );
        let contents = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(contents, "from main");
    }

    #[test]
    fn cherry_pick_continue_fails_loudly_when_nothing_is_in_progress() {
        let dir = tempdir().unwrap();
        checkout_with_a_commit_on(dir.path(), "main", &[("f.txt", "1")]);

        let err = cherry_pick_continue(dir.path(), "gitprism", "gitprism@example.com")
            .expect_err("continuing with no cherry-pick in progress must not silently succeed");

        assert!(err.to_string().contains("cherry-pick --continue"));
    }

    #[test]
    fn git_commands_scrub_gitprism_urls_and_state_but_preserve_auth_helpers() {
        let command = git_command();
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();

        assert_eq!(
            envs.get(std::ffi::OsStr::new("GITPRISM_STATE_KEY")),
            Some(&None)
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GITPRISM_SOURCE_URL")),
            Some(&None)
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GITPRISM_DEST_URL")),
            Some(&None)
        );
        assert!(
            !envs.contains_key(std::ffi::OsStr::new("GIT_ASKPASS")),
            "credential helper variables remain inherited rather than being scrubbed"
        );
    }

    /// A tree built directly via `repo.treebuilder`, no commit needed — the
    /// point of testing `merge_tree` against raw tree oids (decisions/0016).
    fn tree_with(repo: &Repository, files: &[(&str, &str)]) -> git2::Oid {
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        builder.write().unwrap()
    }

    #[cfg(unix)]
    fn tree_with_raw_path(repo: &Repository, path: &[u8], contents: &[u8]) -> git2::Oid {
        let blob = repo.blob(contents).unwrap();
        let mut input = format!("100644 blob {blob}\t").into_bytes();
        input.extend_from_slice(path);
        input.push(0);
        let mut child = Command::new("git")
            .current_dir(repo.workdir().unwrap())
            .arg("mktree")
            .arg("-z")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git mktree failed: {output:?}");
        git2::Oid::from_str(std::str::from_utf8(&output.stdout).unwrap().trim()).unwrap()
    }

    #[test]
    fn merge_tree_reports_the_merged_tree_when_the_two_sides_touch_different_files() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let base = tree_with(&repo, &[("a.txt", "1")]);
        let ours = tree_with(&repo, &[("a.txt", "1"), ("b.txt", "ours")]);
        let theirs = tree_with(&repo, &[("a.txt", "1"), ("c.txt", "theirs")]);

        let outcome = merge_tree(dir.path(), base, ours, theirs)
            .expect("merging changes to different files must not conflict");
        let oid = match outcome {
            MergeTreeOutcome::Clean(oid) => oid,
            other => panic!("expected a clean merge, got {other:?}"),
        };

        let merged = repo
            .find_tree(oid)
            .expect("the written tree must be readable back through git2");
        assert!(merged.get_name("a.txt").is_some());
        assert!(merged.get_name("b.txt").is_some());
        assert!(merged.get_name("c.txt").is_some());
    }

    #[test]
    fn merge_tree_reports_the_merged_tree_when_both_sides_made_the_same_change() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let base = tree_with(&repo, &[("a.txt", "1")]);
        let ours = tree_with(&repo, &[("a.txt", "2")]);
        let theirs = tree_with(&repo, &[("a.txt", "2")]);

        let outcome = merge_tree(dir.path(), base, ours, theirs)
            .expect("both sides making the identical change must not conflict");

        // Pins the idempotency property decisions/0016 rests on: the same
        // change on both sides merges to exactly ours' tree, not a new one.
        assert_eq!(outcome, MergeTreeOutcome::Clean(ours));
    }

    #[test]
    fn merge_tree_reports_the_conflicted_path_when_both_sides_changed_the_same_line() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let base = tree_with(&repo, &[("a.txt", "1\n")]);
        let ours = tree_with(&repo, &[("a.txt", "ours\n")]);
        let theirs = tree_with(&repo, &[("a.txt", "theirs\n")]);

        let outcome = merge_tree(dir.path(), base, ours, theirs)
            .expect("a real conflict is a reported outcome, not an error");

        assert_eq!(
            outcome,
            MergeTreeOutcome::Conflict {
                paths: vec!["a.txt".to_string()]
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_tree_escapes_invalid_bytes_in_conflicted_paths() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let path = b"bad-\xff.txt";
        let base = tree_with_raw_path(&repo, path, b"base\n");
        let ours = tree_with_raw_path(&repo, path, b"ours\n");
        let theirs = tree_with_raw_path(&repo, path, b"theirs\n");

        let outcome = merge_tree(dir.path(), base, ours, theirs).unwrap();

        assert_eq!(
            outcome,
            MergeTreeOutcome::Conflict {
                paths: vec!["bad-\\xFF.txt".to_string()]
            }
        );
    }

    #[test]
    fn merge_tree_fails_loudly_on_an_oid_that_isnt_in_the_repository() {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let base = tree_with(&repo, &[("a.txt", "1")]);
        let ours = tree_with(&repo, &[("a.txt", "1")]);
        // Syntactically valid but absent from this repository's object
        // database.
        let theirs = git2::Oid::from_str("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap();

        let err = merge_tree(dir.path(), base, ours, theirs)
            .expect_err("an absent tree oid must not silently succeed");

        assert!(err.to_string().contains("merge-tree"));
    }

    #[test]
    fn parse_git_version_reads_majors_and_minors_it_will_see_in_the_wild() {
        assert_eq!(
            parse_git_version("git version 2.50.1 (Apple Git-155)"),
            Some((2, 50))
        );
        assert_eq!(
            parse_git_version("git version 2.45.1.windows.1"),
            Some((2, 45))
        );
        assert_eq!(parse_git_version("git version 2.40.0"), Some((2, 40)));
        assert_eq!(parse_git_version("not a version"), None);
    }

    #[test]
    fn escape_bytes_preserves_unicode_and_escapes_controls_and_invalid_bytes() {
        assert_eq!(escape_bytes(b"caf\xc3\xa9"), "café");
        assert_eq!(
            escape_bytes(b"line\n\tesc\x1b\\"),
            "line\\x0A\\x09esc\\x1B\\\\"
        );
        assert_eq!(escape_bytes(b"bad\xff\xfe"), "bad\\xFF\\xFE");
    }

    #[cfg(unix)]
    #[test]
    fn path_from_git_bytes_preserves_invalid_unix_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let path = path_from_git_bytes(b"bad\xff").expect("Unix paths preserve raw bytes");
        assert_eq!(path.as_os_str().as_bytes(), b"bad\xff");
    }

    #[test]
    fn ensure_merge_tree_supported_accepts_the_git_on_this_machine() {
        ensure_merge_tree_supported().expect("the git on this dev/CI machine must be new enough");
    }
}
