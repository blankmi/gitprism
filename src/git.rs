//! Push/fetch go through a real `git` subprocess, not `git2-rs` — see
//! design/decisions/0002-hybrid-git-backend.md. This is the fast-forward-only
//! network path, so it should inherit the same credential helpers/SSH
//! agent/`GIT_ASKPASS` handling a human running `git` would get, rather than
//! a library reimplementation of it.

use std::env;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const DEFAULT_GIT_TIMEOUT_SECONDS: u64 = 300;
const MIN_GIT_TIMEOUT_SECONDS: u64 = 1;
const MAX_GIT_TIMEOUT_SECONDS: u64 = 3_600;
const MAX_PARSE_STDOUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_SMALL_STDOUT_BYTES: usize = 64 * 1024;
const MAX_DIAGNOSTIC_STDERR_BYTES: usize = 1024 * 1024;
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);
const CAPTURE_CHUNK_BYTES: usize = 16 * 1024;
const CAPTURE_CHANNEL_CAPACITY: usize = 16;

#[derive(Clone, Copy)]
struct OutputLimits {
    stdout: usize,
    stderr: usize,
    total: usize,
}

const SMALL_OUTPUT: OutputLimits = OutputLimits {
    stdout: MAX_SMALL_STDOUT_BYTES,
    stderr: MAX_DIAGNOSTIC_STDERR_BYTES,
    total: MAX_SMALL_STDOUT_BYTES + MAX_DIAGNOSTIC_STDERR_BYTES,
};

const PARSE_OUTPUT: OutputLimits = OutputLimits {
    stdout: MAX_PARSE_STDOUT_BYTES,
    stderr: MAX_DIAGNOSTIC_STDERR_BYTES,
    total: MAX_PARSE_STDOUT_BYTES + MAX_DIAGNOSTIC_STDERR_BYTES,
};

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
    // Refuse `ext::`/`fd::` transports independent of ambient protocol.allow
    // config. `file` shares the same "user" default and is a supported remote
    // shape, so it is allowed back explicitly.
    command.env("GIT_PROTOCOL_FROM_USER", "0");
    command.arg("-c").arg("protocol.file.allow=always");
    // The repo we operate on (via -C) may be a CI checkout whose .git/config
    // sets credential.interactive=false/never (GitLab Runner does this on its
    // own clones to avoid hangs). That setting makes git refuse to invoke
    // GIT_ASKPASS at all, failing with "unable to get password from user"
    // before our credential helper ever gets a chance to run. Force it back
    // on for our own invocations, overriding whatever the ambient repo config
    // says -- command-line -c takes precedence over any file-level config.
    command.arg("-c").arg("credential.interactive=true");
    #[cfg(test)]
    isolate_test_git_command(&mut command);
    command
}

/// Test isolation, not a production concern: every subprocess `git`
/// invocation in the test suite is pointed at an empty global config with
/// system config disabled, so tests never read the developer's or CI
/// runner's real `~/.gitconfig`/`/etc/gitconfig`. The empty config file is
/// created once per test binary and shared by every test via `OnceLock`
/// rather than per-test, since it's never written to.
#[cfg(test)]
static TEST_GIT_CONFIG_GLOBAL: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn isolate_test_git_command(command: &mut Command) {
    let path = TEST_GIT_CONFIG_GLOBAL.get_or_init(|| {
        let dir = tempfile::Builder::new()
            .prefix("gitprism-test-global-config")
            .tempdir()
            .expect("creating an isolated git config directory");
        let path = dir.path().join("gitconfig");
        std::fs::write(&path, b"").expect("creating an empty isolated global git config");
        // Leaked deliberately: this directory must outlive every test that
        // shares this path for the rest of the test process.
        std::mem::forget(dir);
        path
    });
    command.env("GIT_CONFIG_GLOBAL", path);
    command.env("GIT_CONFIG_NOSYSTEM", "1");
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
            character if character.is_control() || is_forging_risk(character) => {
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

/// Unicode separator (Zl/Zp) and format (Cf) characters `char::is_control`
/// does not cover (it's exactly the Cc category, which already includes the
/// C1 controls U+0080-U+009F) — a terminal or a log line can still be split,
/// reordered, or hidden by these even though `git check-ref-format` permits
/// them in a branch name. Scoped to the characters with real spoofing value
/// (line/paragraph separators, zero-width and bidi-override/isolate
/// controls, the BOM); the full Cf category also contains obscure
/// deprecated-annotation and musical-notation codepoints not enumerated
/// here.
fn is_forging_risk(character: char) -> bool {
    matches!(character as u32,
        0x2028 | 0x2029 // Zl, Zp: LINE SEPARATOR, PARAGRAPH SEPARATOR
        | 0x00AD // Cf: SOFT HYPHEN
        | 0x0600..=0x0605 | 0x06DD | 0x070F | 0x08E2 // Cf: Arabic number/format signs
        | 0x180E // Cf: MONGOLIAN VOWEL SEPARATOR
        | 0x200B..=0x200F // Cf: zero-width space/non-joiner/joiner, LRM, RLM
        | 0x202A..=0x202E // Cf: bidi embedding/override controls
        | 0x2060..=0x2064 // Cf: word joiner, invisible operators
        | 0x2066..=0x206F // Cf: bidi isolates, other format controls
        | 0xFEFF // Cf: BOM / zero-width no-break space
        | 0xFFF9..=0xFFFB // Cf: interlinear annotation controls
        | 0x1D173..=0x1D17A // Cf: musical notation format controls
        | 0xE0001 | 0xE0020..=0xE007F // Cf: language tag / tag characters
    )
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

/// Whether `raw` (a `git push --porcelain` byte stream) reports a ref update
/// rejected because the remote ref had moved since gitprism last looked at
/// it — the porcelain status field `!` plus a last field starting
/// `[rejected]`. This covers both a plain non-fast-forward rejection
/// (decisions/0009) and a stale `--force-with-lease` rejection
/// (decisions/0040: reason `(stale info)`), which git reports with the exact
/// same status/reason shape — verified against real git, not assumed.
fn is_ref_moved_rejection(raw: &[u8]) -> bool {
    raw.split(|byte| *byte == b'\n').any(|line| {
        let mut fields = line.split(|byte| *byte == b'\t');
        let status = fields.next();
        let reason = fields.next_back().unwrap_or_default();
        status == Some(b"!") && reason.starts_with(b"[rejected]")
    })
}

fn run_git_output(command: Command, limits: OutputLimits) -> Result<std::process::Output> {
    run_git_output_with_timeout(command, limits, configured_git_timeout()?)
}

fn configured_git_timeout() -> Result<Duration> {
    let Some(raw) = env::var_os("GITPRISM_GIT_TIMEOUT_SECONDS") else {
        return Ok(Duration::from_secs(DEFAULT_GIT_TIMEOUT_SECONDS));
    };
    let raw = raw
        .to_str()
        .context("GITPRISM_GIT_TIMEOUT_SECONDS must be valid UTF-8")?;
    parse_timeout_seconds(raw)
}

fn parse_timeout_seconds(raw: &str) -> Result<Duration> {
    let seconds = raw
        .parse::<u64>()
        .with_context(|| "GITPRISM_GIT_TIMEOUT_SECONDS must be an integer number of seconds")?;
    if !(MIN_GIT_TIMEOUT_SECONDS..=MAX_GIT_TIMEOUT_SECONDS).contains(&seconds) {
        anyhow::bail!(
            "GITPRISM_GIT_TIMEOUT_SECONDS must be between {MIN_GIT_TIMEOUT_SECONDS} and {MAX_GIT_TIMEOUT_SECONDS} seconds"
        );
    }
    Ok(Duration::from_secs(seconds))
}

#[derive(Clone, Copy)]
enum CaptureStream {
    Stdout,
    Stderr,
}

enum CaptureEvent {
    Chunk(CaptureStream, Vec<u8>),
    Eof(CaptureStream),
    Error(CaptureStream, String),
}

fn run_git_output_with_timeout(
    mut command: Command,
    limits: OutputLimits,
    timeout: Duration,
) -> Result<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0");
    let mut child = command.spawn().context("starting git subprocess")?;
    let stdout = child
        .stdout
        .take()
        .context("capturing git subprocess stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("capturing git subprocess stderr")?;
    let (sender, receiver) = mpsc::sync_channel(CAPTURE_CHANNEL_CAPACITY);
    spawn_capture(stdout, CaptureStream::Stdout, sender.clone());
    spawn_capture(stderr, CaptureStream::Stderr, sender);

    let started = Instant::now();
    let mut status = None;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut total_bytes: usize = 0;
    let mut drain_deadline = None;

    loop {
        loop {
            match receiver.try_recv() {
                Ok(CaptureEvent::Chunk(stream, bytes)) => {
                    let (captured, limit) = match stream {
                        CaptureStream::Stdout => (&mut stdout_bytes, limits.stdout),
                        CaptureStream::Stderr => (&mut stderr_bytes, limits.stderr),
                    };
                    if captured.len().saturating_add(bytes.len()) > limit {
                        kill_and_reap(&mut child);
                        anyhow::bail!(
                            "git subprocess exceeded its {} byte {} output limit",
                            limit,
                            stream_label(stream)
                        );
                    }
                    if total_bytes.saturating_add(bytes.len()) > limits.total {
                        kill_and_reap(&mut child);
                        anyhow::bail!(
                            "git subprocess exceeded its {} byte total output limit",
                            limits.total
                        );
                    }
                    captured.extend_from_slice(&bytes);
                    total_bytes = total_bytes.saturating_add(bytes.len());
                }
                Ok(CaptureEvent::Eof(stream)) => match stream {
                    CaptureStream::Stdout => stdout_done = true,
                    CaptureStream::Stderr => stderr_done = true,
                },
                Ok(CaptureEvent::Error(stream, error)) => {
                    kill_and_reap(&mut child);
                    anyhow::bail!(
                        "reading git subprocess {} failed: {error}",
                        stream_label(stream)
                    );
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if stdout_done && stderr_done {
                        break;
                    }
                    kill_and_reap(&mut child);
                    anyhow::bail!("git subprocess output capture disconnected unexpectedly");
                }
            }
        }

        if status.is_none() {
            status = child.try_wait().context("waiting for git subprocess")?;
            if status.is_some() && !(stdout_done && stderr_done) {
                drain_deadline = Some(Instant::now() + PIPE_DRAIN_GRACE);
            }
        }
        if stdout_done
            && stderr_done
            && let Some(status) = status
        {
            return Ok(std::process::Output {
                status,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            });
        }

        let now = Instant::now();
        if now.duration_since(started) >= timeout {
            kill_and_reap(&mut child);
            anyhow::bail!(
                "git subprocess exceeded its {} second deadline",
                timeout.as_secs()
            );
        }
        if let Some(deadline) = drain_deadline
            && now >= deadline
        {
            kill_and_reap(&mut child);
            anyhow::bail!(
                "git subprocess exited but its output pipes did not close; refusing to continue with incomplete output"
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn spawn_capture(
    mut reader: impl Read + Send + 'static,
    stream: CaptureStream,
    sender: SyncSender<CaptureEvent>,
) {
    thread::spawn(move || {
        let mut buffer = vec![0; CAPTURE_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = sender.send(CaptureEvent::Eof(stream));
                    return;
                }
                Ok(count) => {
                    if sender
                        .send(CaptureEvent::Chunk(stream, buffer[..count].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(CaptureEvent::Error(stream, error.to_string()));
                    return;
                }
            }
        }
    });
}

fn stream_label(stream: CaptureStream) -> &'static str {
    match stream {
        CaptureStream::Stdout => "stdout",
        CaptureStream::Stderr => "stderr",
    }
}

fn kill_and_reap(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Shared by [`fetch`] and [`fetch_shallow`]: validation, argument-building,
/// and diagnostics for a `git fetch` landing at `FETCH_HEAD`. `depth` is
/// `None` for an ordinary full fetch, `Some(n)` for a `--depth=n` truncated
/// one.
fn fetch_with_depth(repo_dir: &Path, url: &str, branch: &str, depth: Option<u32>) -> Result<()> {
    validate_remote(url)?;
    validate_branch_name(branch)?;
    let source_ref = format!("refs/heads/{branch}");
    let mut command = git_command();
    command.arg("-C").arg(repo_dir).arg("fetch").arg("-q");
    if let Some(depth) = depth {
        command.arg(format!("--depth={depth}"));
    }
    command
        .arg("--")
        .arg(url)
        .arg(&source_ref)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_git_output(command, SMALL_OUTPUT)
        .context("running git fetch from configured remote")?;

    if !output.status.success() {
        let diagnostic = git_diagnostic(&output.stderr, Some(url));
        anyhow::bail!(
            "git fetch branch {branch:?} from configured remote failed ({}): {diagnostic}",
            output.status
        );
    }

    Ok(())
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
    fetch_with_depth(repo_dir, url, branch, None)
}

/// Same as [`fetch`], truncated to `depth` — a real `--depth`-limited `git
/// fetch`, not a hand-rolled subprocess, so it goes through the same
/// validation and diagnostics. gitprism itself never calls this: it exists
/// so tests can produce a genuinely shallow clone (`Repository::is_shallow`)
/// without bypassing [`fetch`]'s `validate_remote`/`validate_branch_name`.
#[cfg(test)]
pub(crate) fn fetch_shallow(repo_dir: &Path, url: &str, branch: &str, depth: u32) -> Result<()> {
    fetch_with_depth(repo_dir, url, branch, Some(depth))
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
    let output = run_git_output(command, SMALL_OUTPUT)
        .context("running git ls-remote against configured remote")?;

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

/// Every branch name currently on `url` — a real `git ls-remote --heads`
/// subprocess, parsed for `refs/heads/<name>` lines. decisions/0046, F-A:
/// mapping-index reconstruction must see a mirror-only branch's own dest
/// ref even after its local source branch is deleted (decisions/0018 Case
/// 2's routine post-merge cleanup) — `source`'s own branch listing cannot
/// name a branch source no longer has, so dest itself is asked directly.
/// Bounded the same way `commands::sync::list_source_branches` bounds its
/// own local listing, and a name that doesn't pass
/// [`validate_branch_name`] is skipped rather than failing the whole
/// listing — decisions/0024's precedent for a discovered oddity gitprism
/// can't act on. decisions/0046 Addendum 2, Finding F: hitting
/// `MAX_SOURCE_BRANCHES` truncates the listing rather than failing it — dest's
/// branch count is not something a source-side operator can necessarily
/// reduce, so the caller degrades to a per-branch refusal instead of a
/// whole-run abort. A genuine `ls-remote` failure is still an `Err`.
pub(crate) fn remote_branch_names(repo_dir: &Path, url: &str) -> Result<RemoteBranchListing> {
    validate_remote(url)?;
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("ls-remote")
        .arg("--heads")
        .arg("--")
        .arg(url)
        .env("GIT_TERMINAL_PROMPT", "0");
    // A SHA-1 ref advertisement is at least dozens of bytes per branch, so
    // SMALL_OUTPUT would reject the valid 4,096-branch boundary before the
    // explicit branch-count guard below gets a chance to run.  PARSE_OUTPUT
    // is still a fixed cap (not an unbounded capture) and comfortably covers
    // the Git refname limit multiplied by MAX_SOURCE_BRANCHES.
    let output = run_git_output(command, PARSE_OUTPUT)
        .context("running git ls-remote --heads against configured remote")?;
    if !output.status.success() {
        let diagnostic = git_diagnostic(&output.stderr, Some(url));
        anyhow::bail!(
            "git ls-remote --heads against configured remote failed ({}): {diagnostic}",
            output.status
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut names = Vec::new();
    let mut truncated = false;
    for line in stdout.lines() {
        let Some(refname) = line.split_ascii_whitespace().nth(1) else {
            continue;
        };
        let Some(name) = refname.strip_prefix("refs/heads/") else {
            continue;
        };
        if validate_branch_name(name).is_err() {
            continue;
        }
        if names.len() >= crate::limits::MAX_SOURCE_BRANCHES {
            truncated = true;
            break;
        }
        names.push(name.to_string());
    }
    Ok(RemoteBranchListing { names, truncated })
}

/// [`remote_branch_names`]'s result: the branch names read up to
/// `MAX_SOURCE_BRANCHES`, plus whether the listing was cut short there
/// (decisions/0046 Addendum 2, Finding F). `truncated` is the caller's
/// signal to treat reconstruction as incomplete rather than to assume dest
/// has no more branches than `names` lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteBranchListing {
    pub(crate) names: Vec<String>,
    pub(crate) truncated: bool,
}

/// What happened to a [`push`] attempt: either it landed, or it was
/// rejected because the remote ref had moved since gitprism last looked at
/// it — a plain non-fast-forward rejection (decisions/0009) or a stale
/// `--force-with-lease` compare-and-swap (decisions/0040), the two failures
/// worth refetching dest and recomputing for. Every other failure (auth,
/// hooks, network, ...) is a plain `Err`.
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    RejectedRefMoved,
}

/// Required at every [`push`] call site (decisions/0038, decisions/0039,
/// decisions/0040) so intent is declared where the push happens, not
/// inferred from which function got called or from how many times a push was
/// retried. `FastForwardOnly` is today's only behavior, unchanged: no
/// `--force`, no `--force-with-lease`, no `+`-prefixed refspec.
/// `ForceMirrorOnly { expected_dest }` performs a compare-and-swap force via
/// `--force-with-lease=refs/heads/<branch>:<expected_dest>` — never an
/// unconditional force — so `expected_dest` must be the dest tip actually
/// fetched this run, not a stale or recomputed value (decisions/0040's
/// "Consequences"). Only ever requested by `sync_pair_to_dest` after
/// positively detecting a rewritten mirror-only source branch
/// (decisions/0039); every other call site stays `FastForwardOnly`
/// permanently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    FastForwardOnly,
    ForceMirrorOnly { expected_dest: git2::Oid },
}

/// The `<new-tip>:<ref>` refspec [`push`] sends to git — always plain, never
/// `+`-prefixed, regardless of `mode`: an authorized mirror force is now
/// expressed entirely by [`push_args`]'s `--force-with-lease` flag
/// (decisions/0040), not by a `+`-prefixed refspec layered on top of it.
fn push_refspec(commit: git2::Oid, dest_branch: &str) -> String {
    format!("{commit}:refs/heads/{dest_branch}")
}

/// The arguments [`push`] passes to `git push` after `--porcelain` for
/// `mode` — pulled out on its own so the exact wire shape (whether a
/// `--force-with-lease` flag is present, and that the refspec itself is
/// never `+`-prefixed) is unit-testable without a real remote. Any
/// `--force-with-lease` flag must precede the `--` operand separator: `--`
/// tells git everything after it is positional, so a flag placed after it
/// would be parsed as a literal refspec instead of an option.
fn push_args(mode: PushMode, url: &str, commit: git2::Oid, dest_branch: &str) -> Vec<String> {
    let mut args = Vec::new();
    if let PushMode::ForceMirrorOnly { expected_dest } = mode {
        args.push(format!(
            "--force-with-lease=refs/heads/{dest_branch}:{expected_dest}"
        ));
    }
    args.push("--".to_string());
    args.push(url.to_string());
    args.push(push_refspec(commit, dest_branch));
    args
}

/// Push `commit` (a local oid already in `repo_dir`'s object database) to
/// `dest_branch` on `url` — same as `git push <url> <commit>:<dest_branch>`
/// by hand, or, for [`PushMode::ForceMirrorOnly`], `git push
/// --force-with-lease=refs/heads/<dest_branch>:<expected_dest> <url>
/// <commit>:<dest_branch>` (decisions/0040). `PushMode::FastForwardOnly`
/// passes no `--force`/`--force-with-lease` flag and no `+`-prefixed
/// refspec: git's own default refuses a non-fast-forward update, which is
/// exactly the fast-forward-only constraint requirements/0001 and
/// decisions/0009 require for every push except a positively detected
/// mirror-only rewrite (decisions/0038, decisions/0039).
pub fn push(
    repo_dir: &Path,
    url: &str,
    commit: git2::Oid,
    dest_branch: &str,
    mode: PushMode,
) -> Result<PushOutcome> {
    validate_remote(url)?;
    validate_branch_name(dest_branch)?;
    let args = push_args(mode, url, commit, dest_branch);
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("push")
        .arg("--porcelain")
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output =
        run_git_output(command, PARSE_OUTPUT).context("running git push to configured remote")?;

    if output.status.success() {
        return Ok(PushOutcome::Accepted);
    }

    // git's own wording for "the ref moved since we last looked" — the only
    // case decisions/0009 and decisions/0040 want recomputed and retried.
    // Everything else (bad credentials, a rejecting pre-receive hook, a
    // dropped connection, ...) must surface immediately instead of being
    // silently retried.
    if is_ref_moved_rejection(&output.stdout) {
        return Ok(PushOutcome::RejectedRefMoved);
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
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(subprocess_path(worktree))
        .arg(commit.to_string());
    let output = run_git_output(command, SMALL_OUTPUT).context("running git worktree add")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!("git worktree add failed ({}): {stderr}", output.status);
    }
    Ok(())
}

pub(crate) fn worktree_remove(repo_dir: &Path, worktree: &Path) -> Result<()> {
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(subprocess_path(worktree));
    let output = run_git_output(command, SMALL_OUTPUT).context("running git worktree remove")?;
    if !output.status.success() {
        let stderr = git_diagnostic(&output.stderr, None);
        anyhow::bail!("git worktree remove failed ({}): {stderr}", output.status);
    }
    Ok(())
}

/// `fs::canonicalize` on Windows returns an extended-length (`\\?\`-prefixed,
/// "verbatim") path. `worktree_add`/`worktree_remove` above run through Git
/// for Windows' MSYS-based git, which does not reliably accept that prefix
/// as a path argument — strip it back to an ordinary absolute path for
/// exactly this subprocess-argument boundary. This must never leak into a
/// caller's own copy of the path: decisions/0033's symlink-substitution
/// check authenticates a resolution worktree path by requiring it to equal
/// its own `fs::canonicalize` output exactly, which on Windows is always
/// verbatim-prefixed.
#[cfg(windows)]
fn subprocess_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(windows))]
fn subprocess_path(path: &Path) -> PathBuf {
    path.to_path_buf()
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

    let output = run_git_output(cmd, SMALL_OUTPUT)
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
    let output = run_git_output(cmd, SMALL_OUTPUT)
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
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("cherry-pick")
        .arg("--continue")
        .env("GIT_EDITOR", "true");
    let output =
        run_git_output(command, SMALL_OUTPUT).context("running git cherry-pick --continue")?;

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
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("ls-files")
        .arg("--unmerged");
    let output = run_git_output(command, PARSE_OUTPUT).context("checking for unmerged paths")?;
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
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("commit")
        .arg("--allow-empty")
        .arg("--no-edit");
    let output = run_git_output(command, SMALL_OUTPUT)
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
    let mut command = git_command();
    command
        .arg("-C")
        .arg(repo_dir)
        .arg("merge-tree")
        .arg("--write-tree")
        .arg("-z")
        .arg("--name-only")
        .arg("--no-messages")
        .arg(&merge_base_arg)
        .arg(ours.to_string())
        .arg(theirs.to_string());
    let output = run_git_output(command, PARSE_OUTPUT).with_context(|| {
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
            let mut paths = Vec::new();
            let mut record_count = 0;
            let mut raw_path_bytes: usize = 0;
            for record in records.skip(1) {
                record_count += 1;
                if record_count > crate::limits::MAX_CONFLICT_RECORDS {
                    anyhow::bail!(
                        "merge-tree conflict output exceeds the {} record limit",
                        crate::limits::MAX_CONFLICT_RECORDS
                    );
                }
                raw_path_bytes = raw_path_bytes.saturating_add(record.len());
                if raw_path_bytes > crate::limits::MAX_CONFLICT_PATH_BYTES {
                    anyhow::bail!(
                        "merge-tree conflict paths exceed the {} byte limit",
                        crate::limits::MAX_CONFLICT_PATH_BYTES
                    );
                }
                paths.push(escape_bytes(record));
            }
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
    let mut command = git_command();
    command.arg("--version");
    let output = run_git_output(command, SMALL_OUTPUT).context("running git --version")?;
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
    use std::env;
    use std::io::Write;
    use std::time::Duration;

    use git2::Repository;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn subprocess_runner_child() {
        match env::var("GITPRISM_TEST_RUNNER_MODE").as_deref() {
            Ok("sleep") => thread::sleep(Duration::from_secs(60)),
            Ok("stdout") => {
                print!("{}", "x".repeat(4096));
                std::io::stdout().flush().unwrap();
            }
            Ok("stderr") => {
                std::io::stderr().write_all(&vec![b'x'; 4096]).unwrap();
                std::io::stderr().flush().unwrap();
            }
            Ok("normal") => {
                print!("normal stdout");
                std::io::stdout().flush().unwrap();
                eprint!("normal stderr");
                std::io::stderr().flush().unwrap();
            }
            Ok("large") => {
                print!("{}", "o".repeat(128 * 1024));
                std::io::stdout().flush().unwrap();
                std::io::stderr()
                    .write_all(&vec![b'e'; 128 * 1024])
                    .unwrap();
                std::io::stderr().flush().unwrap();
            }
            Ok("env") => {
                let prompt = env::var("GIT_TERMINAL_PROMPT").unwrap_or_default();
                print!("prompt={prompt}");
                std::io::stdout().flush().unwrap();
            }
            Ok("stdin") => {
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).unwrap();
                print!("stdin-bytes={}", input.len());
                std::io::stdout().flush().unwrap();
            }
            _ => {}
        }
    }

    fn runner_child(mode: &str) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("git::tests::subprocess_runner_child")
            .arg("--nocapture")
            .env("GITPRISM_TEST_RUNNER_MODE", mode);
        command
    }

    #[test]
    fn subprocess_runner_captures_both_streams_and_disables_prompting() {
        let output = run_git_output_with_timeout(
            runner_child("normal"),
            OutputLimits {
                stdout: 1024,
                stderr: 1024,
                total: 2048,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(
            output
                .stdout
                .windows(b"normal stdout".len())
                .any(|window| { window == b"normal stdout" })
        );
        assert!(
            output
                .stderr
                .windows(b"normal stderr".len())
                .any(|window| { window == b"normal stderr" })
        );

        let output =
            run_git_output_with_timeout(runner_child("env"), SMALL_OUTPUT, Duration::from_secs(5))
                .unwrap();
        assert!(
            output
                .stdout
                .windows(b"prompt=0".len())
                .any(|window| { window == b"prompt=0" })
        );

        let output = run_git_output_with_timeout(
            runner_child("stdin"),
            SMALL_OUTPUT,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            output
                .stdout
                .windows(b"stdin-bytes=0".len())
                .any(|window| { window == b"stdin-bytes=0" })
        );
    }

    #[test]
    fn subprocess_runner_times_out_and_reaps_direct_child() {
        // The deadline itself (well under the child's 60s sleep) is not what
        // makes this flaky on a loaded CI runner; it's process-spawn/schedule
        // latency for re-execing the test binary. 300ms leaves comfortable
        // headroom for that without weakening what's asserted: the child
        // still sleeps orders of magnitude longer than the deadline, so this
        // still only passes if the timeout path actually fires.
        let error = run_git_output_with_timeout(
            runner_child("sleep"),
            SMALL_OUTPUT,
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.to_string().contains("deadline"));
    }

    #[test]
    fn subprocess_runner_captures_large_concurrent_streams() {
        let output = run_git_output_with_timeout(
            runner_child("large"),
            OutputLimits {
                stdout: 256 * 1024,
                stderr: 256 * 1024,
                total: 512 * 1024,
            },
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.len() >= 128 * 1024);
        assert!(output.stderr.len() >= 128 * 1024);
    }

    #[test]
    fn subprocess_runner_rejects_stdout_overflow_without_truncating_parse_output() {
        let error = run_git_output_with_timeout(
            runner_child("stdout"),
            OutputLimits {
                stdout: 1024,
                stderr: 1024,
                total: 2048,
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("stdout output limit"));
    }

    #[test]
    fn subprocess_runner_rejects_stderr_overflow_without_truncating_diagnostics() {
        let error = run_git_output_with_timeout(
            runner_child("stderr"),
            OutputLimits {
                stdout: 1024,
                stderr: 1024,
                total: 2048,
            },
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().contains("stderr output limit"));
    }

    #[test]
    fn git_timeout_override_is_strictly_parsed_and_bounded() {
        assert_eq!(
            parse_timeout_seconds("17").unwrap(),
            Duration::from_secs(17)
        );
        assert!(parse_timeout_seconds("0").is_err());
        assert!(parse_timeout_seconds("3601").is_err());
        assert!(parse_timeout_seconds("not-a-number").is_err());
    }

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
    fn fetch_shallow_lands_the_remote_branch_at_fetch_head_and_produces_a_shallow_clone() {
        let dest_dir = tempdir().unwrap();
        let expected = repo_with_a_commit_on(dest_dir.path(), "main");

        let source_dir = tempdir().unwrap();
        let source_repo = Repository::init(source_dir.path()).unwrap();

        fetch_shallow(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            "main",
            1,
        )
        .expect("a depth-limited fetch of an existing branch should succeed");

        let fetched = source_repo
            .find_reference("FETCH_HEAD")
            .expect("FETCH_HEAD should exist after a successful fetch")
            .peel_to_commit()
            .unwrap();
        assert_eq!(fetched.id(), expected);
        assert!(
            source_repo.is_shallow(),
            "a --depth=1 fetch must leave the clone shallow"
        );
    }

    #[test]
    fn fetch_shallow_rejects_a_raw_refspec_before_touching_fetch_head() {
        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();

        let err = fetch_shallow(
            source_dir.path(),
            "/does/not/matter",
            "refs/heads/main:refs/heads/other",
            1,
        )
        .expect_err("fetch_shallow accepts branch names, not caller-provided refspecs");

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
            PushMode::FastForwardOnly,
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
            PushMode::FastForwardOnly,
        )
        .expect("a lost fast-forward race is a reported outcome, not an error");
        assert_eq!(outcome, PushOutcome::RejectedRefMoved);

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
    fn push_force_mirror_only_uses_a_correct_lease_to_force_a_non_fast_forward_update() {
        let dest_dir = tempdir().unwrap();
        let dest_tip = repo_with_a_commit_on(dest_dir.path(), "main");
        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        dest_repo.set_head("refs/heads/unrelated").unwrap();

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let source_repo = Repository::open(source_dir.path()).unwrap();
        // Unrelated to dest_tip, so pushing it plainly would be refused —
        // isolates that the lease alone is what authorizes the force.
        let tree_oid = source_repo.treebuilder(None).unwrap().write().unwrap();
        let tree = source_repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let rewritten = source_repo
            .commit(None, &signature, &signature, "rewritten", &tree, &[])
            .unwrap();

        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            rewritten,
            "main",
            PushMode::ForceMirrorOnly {
                expected_dest: dest_tip,
            },
        )
        .expect("a lease matching dest's actual tip must force a non-fast-forward update");
        assert_eq!(outcome, PushOutcome::Accepted);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let now = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            now.id(),
            rewritten,
            "a correct lease must move dest's branch to the new tip even though it discards dest_tip"
        );
        assert_ne!(now.id(), dest_tip);
    }

    #[test]
    fn push_force_mirror_only_rejects_a_stale_lease_and_leaves_the_remote_untouched() {
        let dest_dir = tempdir().unwrap();
        let stale_expected = repo_with_a_commit_on(dest_dir.path(), "main");
        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        dest_repo.set_head("refs/heads/unrelated").unwrap();
        // Stands in for another writer advancing dest between gitprism's
        // fetch (which observed `stale_expected`) and its push.
        let advanced = commit_on(&dest_repo, stale_expected, ("advance.txt", "x"));
        dest_repo
            .reference("refs/heads/main", advanced, true, "advance")
            .unwrap();

        let source_dir = tempdir().unwrap();
        Repository::init(source_dir.path()).unwrap();
        let source_repo = Repository::open(source_dir.path()).unwrap();
        let tree_oid = source_repo.treebuilder(None).unwrap().write().unwrap();
        let tree = source_repo.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let rewritten = source_repo
            .commit(None, &signature, &signature, "rewritten", &tree, &[])
            .unwrap();

        let outcome = push(
            source_dir.path(),
            &dest_dir.path().display().to_string(),
            rewritten,
            "main",
            PushMode::ForceMirrorOnly {
                expected_dest: stale_expected,
            },
        )
        .expect("a stale lease is a reported rejection, not an error");
        assert_eq!(outcome, PushOutcome::RejectedRefMoved);

        let dest_repo = Repository::open(dest_dir.path()).unwrap();
        let still = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still.id(),
            advanced,
            "a rejected lease must not move dest's branch — the whole point of the compare-and-swap"
        );
    }

    #[test]
    fn push_args_fast_forward_only_carries_neither_a_lease_flag_nor_a_force_prefix() {
        let commit = git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
        let args = push_args(PushMode::FastForwardOnly, "the-url", commit, "main");
        assert_eq!(
            args,
            vec![
                "--".to_string(),
                "the-url".to_string(),
                format!("{commit}:refs/heads/main")
            ]
        );
        assert!(!args.iter().any(|arg| arg.starts_with('+')));
        assert!(!args.iter().any(|arg| arg.contains("force")));
    }

    #[test]
    fn push_args_force_mirror_only_is_a_lease_against_expected_dest_with_a_plain_refspec() {
        let commit = git2::Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap();
        let expected_dest =
            git2::Oid::from_str("fedcba9876543210fedcba9876543210fedcba98").unwrap();
        let args = push_args(
            PushMode::ForceMirrorOnly { expected_dest },
            "the-url",
            commit,
            "main",
        );
        assert_eq!(
            args,
            vec![
                format!("--force-with-lease=refs/heads/main:{expected_dest}"),
                "--".to_string(),
                "the-url".to_string(),
                format!("{commit}:refs/heads/main"),
            ]
        );
        assert!(
            !args.iter().any(|arg| arg.starts_with('+')),
            "the lease flag alone must force the update, not a `+`-prefixed refspec: {args:?}"
        );
        assert_eq!(
            args.iter().position(|arg| arg == "--"),
            Some(1),
            "the force-with-lease flag must precede `--`, or git parses it as a literal positional refspec instead of an option: {args:?}"
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
            PushMode::FastForwardOnly,
        )
        .expect_err("an unreachable remote must not silently succeed");

        assert!(err.to_string().contains("git push"));
    }

    #[test]
    fn push_porcelain_rejection_parser_distinguishes_ref_moved_rejections() {
        assert!(is_ref_moved_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (fetch first)\nDone\n"
        ));
        assert!(is_ref_moved_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (non-fast-forward)\n"
        ));
        assert!(is_ref_moved_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (razlog lokalizovan)\n"
        ));
        // decisions/0040: a stale `--force-with-lease` reports the same `!` /
        // `[rejected]` porcelain shape as a plain non-fast-forward rejection,
        // just with a different human-readable reason — this is what lets a
        // stale lease already route into decisions/0009's retry loop with no
        // new plumbing.
        assert!(is_ref_moved_rejection(
            b"!\tHEAD:refs/heads/main\t[rejected] (stale info)\n"
        ));
        assert!(!is_ref_moved_rejection(
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

    #[test]
    fn git_commands_refuse_ext_and_fd_transports_independent_of_user_config() {
        let command = git_command();
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();

        assert_eq!(
            envs.get(std::ffi::OsStr::new("GIT_PROTOCOL_FROM_USER")),
            Some(&Some(std::ffi::OsStr::new("0"))),
            "GIT_PROTOCOL_FROM_USER=0 must be set on every git invocation so \
             ext::/fd:: transports are refused regardless of the operator's \
             own protocol.allow config"
        );
    }

    #[test]
    fn git_commands_are_isolated_from_the_hosts_global_and_system_git_config() {
        let command = git_command();
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();

        let global = envs
            .get(std::ffi::OsStr::new("GIT_CONFIG_GLOBAL"))
            .and_then(|value| *value)
            .expect("every test git invocation must pin GIT_CONFIG_GLOBAL");
        assert_eq!(
            std::fs::read(global).expect("the isolated global config file must exist"),
            b"",
            "the isolated global config must stay empty"
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GIT_CONFIG_NOSYSTEM")),
            Some(&Some(std::ffi::OsStr::new("1"))),
            "system git config must be disabled for every test git invocation"
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
        let mut command = Command::new("git");
        isolate_test_git_command(&mut command);
        let mut child = command
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

    #[test]
    fn escape_bytes_escapes_c1_controls_and_line_paragraph_separators() {
        // U+009B CSI is a C1 control (already covered by `char::is_control`,
        // Cc) — `git check-ref-format` permits it in a branch name.
        assert_eq!(
            escape_bytes("caf\u{9b}e".as_bytes()),
            "caf\\x9Be",
            "a C1 control (e.g. CSI) must not reach the terminal raw"
        );
        // U+2028/U+2029 (Zl/Zp) are not Cc, so `char::is_control` alone
        // wouldn't catch them.
        assert_eq!(
            escape_bytes("line1\u{2028}line2".as_bytes()),
            "line1\\u{2028}line2"
        );
        assert_eq!(escape_bytes("a\u{2029}b".as_bytes()), "a\\u{2029}b");
    }

    #[test]
    fn escape_bytes_escapes_bidi_override_and_zero_width_format_characters() {
        // U+202E RIGHT-TO-LEFT OVERRIDE and U+200B ZERO WIDTH SPACE are both
        // Cf (format), not Cc — real terminal-spoofing vectors in a branch
        // name that `git check-ref-format` still permits.
        assert_eq!(escape_bytes("a\u{202e}b".as_bytes()), "a\\u{202E}b");
        assert_eq!(escape_bytes("a\u{200b}b".as_bytes()), "a\\u{200B}b");
    }

    #[test]
    fn escape_bytes_leaves_plain_ascii_and_ordinary_unicode_unchanged() {
        assert_eq!(escape_bytes(b"feature/my-branch"), "feature/my-branch");
        assert_eq!(escape_bytes("héllo".as_bytes()), "héllo");
    }

    #[cfg(unix)]
    #[test]
    fn path_from_git_bytes_preserves_invalid_unix_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let path = path_from_git_bytes(b"bad\xff").expect("Unix paths preserve raw bytes");
        assert_eq!(path.as_os_str().as_bytes(), b"bad\xff");
    }

    // The four tests below cover the `#[cfg(windows)]` arms of
    // `path_from_git_bytes` and `subprocess_path`, mirroring the `cfg(unix)`
    // tests' structure above. They only compile on a Windows target and were
    // NOT executed anywhere in developing this change (this machine is
    // darwin) — verified by careful reading of the windows code paths only,
    // not by a real run.

    #[cfg(windows)]
    #[test]
    fn path_from_git_bytes_round_trips_utf8_paths() {
        let path = path_from_git_bytes("café.txt".as_bytes())
            .expect("valid UTF-8 bytes must convert to a path on Windows");
        assert_eq!(path, Path::new("café.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn path_from_git_bytes_rejects_invalid_utf8_on_windows() {
        let error = path_from_git_bytes(b"bad\xff")
            .expect_err("non-UTF-8 git path bytes have no Windows-native representation");
        let message = error.to_string();
        assert!(
            message.contains("is not valid UTF-8"),
            "expected a UTF-8 rejection message, got: {message}"
        );
        assert!(
            message.contains("bad\\xFF"),
            "the offending bytes must be shown escaped, got: {message}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn subprocess_path_strips_the_verbatim_prefix_for_msys_git() {
        assert_eq!(
            subprocess_path(Path::new(r"\\?\C:\Users\test\repo")),
            PathBuf::from(r"C:\Users\test\repo")
        );
    }

    #[cfg(windows)]
    #[test]
    fn subprocess_path_strips_the_verbatim_unc_prefix_for_msys_git() {
        assert_eq!(
            subprocess_path(Path::new(r"\\?\UNC\server\share\repo")),
            PathBuf::from(r"\\server\share\repo")
        );
    }

    #[cfg(windows)]
    #[test]
    fn subprocess_path_leaves_an_ordinary_path_unchanged() {
        let path = Path::new(r"C:\Users\test\repo");
        assert_eq!(subprocess_path(path), path.to_path_buf());
    }

    #[test]
    fn ensure_merge_tree_supported_accepts_the_git_on_this_machine() {
        ensure_merge_tree_supported().expect("the git on this dev/CI machine must be new enough");
    }

    #[test]
    fn remote_branch_names_accepts_the_declared_branch_limit() {
        let dir = tempdir().unwrap();
        let source = Repository::init(dir.path().join("source")).unwrap();
        let dest = Repository::init_bare(dir.path().join("dest")).unwrap();
        let tree = dest
            .find_tree(dest.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("test", "test@example.com").unwrap();
        let oid = dest
            .commit(None, &signature, &signature, "root", &tree, &[])
            .unwrap();
        for index in 0..crate::limits::MAX_SOURCE_BRANCHES {
            dest.reference(&format!("refs/heads/b{index:04}"), oid, true, "test")
                .unwrap();
        }

        let listing = remote_branch_names(
            source
                .workdir()
                .expect("non-bare source repository has a worktree"),
            dest.path().to_str().unwrap(),
        )
        .expect("the valid branch boundary must not hit the small output cap");
        assert_eq!(listing.names.len(), crate::limits::MAX_SOURCE_BRANCHES);
        assert!(!listing.truncated);
    }

    #[test]
    fn remote_branch_names_truncates_the_listing_instead_of_failing_past_the_declared_limit() {
        let dir = tempdir().unwrap();
        let source = Repository::init(dir.path().join("source")).unwrap();
        let dest = Repository::init_bare(dir.path().join("dest")).unwrap();
        let tree = dest
            .find_tree(dest.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = git2::Signature::now("test", "test@example.com").unwrap();
        let oid = dest
            .commit(None, &signature, &signature, "root", &tree, &[])
            .unwrap();
        for index in 0..crate::limits::MAX_SOURCE_BRANCHES + 1 {
            dest.reference(&format!("refs/heads/b{index:05}"), oid, true, "test")
                .unwrap();
        }

        let listing = remote_branch_names(
            source
                .workdir()
                .expect("non-bare source repository has a worktree"),
            dest.path().to_str().unwrap(),
        )
        .expect("exceeding the branch limit must truncate the listing, not fail the call");
        assert_eq!(listing.names.len(), crate::limits::MAX_SOURCE_BRANCHES);
        assert!(listing.truncated);
    }
}
