//! decisions/0037's per-commit `.gitprismignore` policy check, shared by
//! `sync`'s source→dest loop and `gitprism resolve`'s own source-to-dest
//! path (decisions/0037's addendum).

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::exclude;
use crate::limits;
use crate::marker;

/// A replayed commit whose control file disagreed with the pinned policy
/// (decisions/0037): which commit, and which of the two filenames — never
/// both conflated into one report, since the operator needs to know exactly
/// which file to look at. `pub(crate)` so `gitprism resolve`'s source-to-dest
/// path (decisions/0037's addendum) can run the same pre-pass.
pub(crate) struct PolicyMismatch {
    pub(super) commit: Oid,
    pub(super) filename: &'static str,
    pub(super) reason: PolicyMismatchReason,
}

pub(super) enum PolicyMismatchReason {
    DiffersFromPinnedPolicy,
    NotARegularFile,
    ExceedsSizeLimit,
}

/// Whether any commit in `pending` — already filtered the same way
/// [`super::build_pending_dest_tip`] filters its own loop-prevented commits,
/// so this only ever inspects commits that would actually be replayed —
/// carries a `.gitprismignore` that either can't be read and compared at all
/// (a non-blob entry, or a blob over the size limit) or whose bytes differ
/// from the digest-verified pinned policy (decisions/0037, with an
/// addendum: safety not being establishable is classified the same way as
/// bytes actively disagreeing). A commit whose tree has no entry is never a
/// mismatch: the trusted policy already governs that commit regardless of
/// what its own tree contains, so absence can't weaken anything. Only the
/// first such commit found (oldest first, matching replay order) is
/// reported.
///
/// `.gitprism.toml` is deliberately not checked here (decisions/0037's later
/// amendment): unlike `.gitprismignore`, it's self-excluded from dest and
/// never consulted per replayed commit — decisions/0026 loads and verifies
/// it exactly once, globally, before either sync phase runs — so an earlier
/// pending commit's differing bytes can't leak content the way an
/// unenforced exclusion can. Checking it per-commit only produced friction
/// (an operator iterating on `.gitprism.toml` before the first successful
/// sync accumulates several pending versions) with no matching security
/// benefit.
pub(crate) fn find_control_file_policy_mismatch(
    repo: &Repository,
    pending: &[Oid],
    branch: &str,
    key: &marker::StateKey,
    ignore_raw: &str,
) -> Result<Option<PolicyMismatch>> {
    for &commit_oid in pending {
        let commit = repo
            .find_commit(commit_oid)
            .context("resolving a pending commit to check its control files")?;

        // Same loop-prevention `build_pending_dest_tip` itself applies (see
        // `super::loop_prevented`): a commit that's already on dest — for
        // `branch` or, per decisions/0043's addendum, for whichever branch
        // its own `DestToSource` marker names — is never replayed onto dest
        // by this branch's sync, so it's not this check's business either.
        if super::loop_prevented(&commit, branch, key) {
            continue;
        }

        let tree = commit
            .tree()
            .context("reading a pending commit's tree to check its control files")?;
        let filename = exclude::FILENAME;
        let reason = match read_control_file_blob(repo, &tree, filename, commit_oid)? {
            ControlFileRead::Absent => continue,
            ControlFileRead::Blob(bytes) if bytes == ignore_raw.as_bytes() => continue,
            ControlFileRead::Blob(_) => PolicyMismatchReason::DiffersFromPinnedPolicy,
            ControlFileRead::NotARegularFile => PolicyMismatchReason::NotARegularFile,
            ControlFileRead::TooLarge => PolicyMismatchReason::ExceedsSizeLimit,
        };
        return Ok(Some(PolicyMismatch {
            commit: commit_oid,
            filename,
            reason,
        }));
    }
    Ok(None)
}

/// What [`read_control_file_blob`] found at a tree entry: whether it's
/// missing, a comparable blob, or something safety can't be established for
/// at all. The latter two cases (a non-blob entry, or a blob over the size
/// limit) are still this branch's problem to report, not this function's —
/// it never errors for them, since an unreadable entry in one pending
/// commit's tree must not abort every other branch's sync (decisions/0037's
/// addendum).
enum ControlFileRead {
    Absent,
    Blob(Vec<u8>),
    NotARegularFile,
    TooLarge,
}

/// Reads `filename`'s entry straight from `tree`'s root, bounded by
/// decisions/0032's existing `MAX_CONTROL_FILE_BYTES` — the same limit every
/// other control-file read in gitprism already respects. Absence is the
/// caller's concern, not this function's: decisions/0037 treats a missing
/// control file as "not a mismatch," never as an empty one. An entry that
/// isn't a blob (a directory or a submodule gitlink) is reported rather than
/// passed to `find_blob`, which would otherwise fail with an ODB type
/// mismatch for a condition that isn't actually an I/O error.
fn read_control_file_blob(
    repo: &Repository,
    tree: &git2::Tree,
    filename: &str,
    commit: Oid,
) -> Result<ControlFileRead> {
    let Some(entry) = tree.get_name(filename) else {
        return Ok(ControlFileRead::Absent);
    };
    if entry.kind() != Some(git2::ObjectType::Blob) {
        return Ok(ControlFileRead::NotARegularFile);
    }
    let blob = repo
        .find_blob(entry.id())
        .with_context(|| format!("reading {filename}'s blob at commit {commit}"))?;
    if blob.size() > limits::MAX_CONTROL_FILE_BYTES {
        return Ok(ControlFileRead::TooLarge);
    }
    Ok(ControlFileRead::Blob(blob.content().to_vec()))
}

/// The operator-facing message for a halted branch (decisions/0037): names
/// the branch, the offending commit, exactly which control file differs, and
/// the remedy — update the pinned digest to the approved policy, or
/// reconcile the branch. `pub(crate)` so `gitprism resolve` reports the same
/// halt in the same words as `sync` (decisions/0037's addendum).
pub(crate) fn policy_mismatch_message(branch: &str, mismatch: &PolicyMismatch) -> String {
    let problem = match mismatch.reason {
        PolicyMismatchReason::DiffersFromPinnedPolicy => {
            format!(
                "carries a {} that differs from the pinned policy",
                mismatch.filename
            )
        }
        PolicyMismatchReason::NotARegularFile => {
            format!("carries a {} that is not a regular file", mismatch.filename)
        }
        PolicyMismatchReason::ExceedsSizeLimit => format!(
            "carries a {} that exceeds the {} byte limit",
            mismatch.filename,
            limits::MAX_CONTROL_FILE_BYTES
        ),
    };
    format!(
        "{branch:?} halted — commit {} {problem}; \
         update GITPRISM_POLICY_SHA256 to the approved policy (via `gitprism policy-hash`) \
         or reconcile the branch so its {} matches exactly",
        mismatch.commit, mismatch.filename
    )
}
