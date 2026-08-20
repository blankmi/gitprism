//! The versioned control files that govern a run.
//!
//! `.gitprism.toml` and `.gitprismignore` are repository-controlled input. A
//! deployment pins the exact pair of bytes it permits with
//! `GITPRISM_POLICY_SHA256`; operational commands verify that pin before they
//! parse the files or perform any Git or working-tree mutation.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::exclude::ExcludeList;
use crate::limits;

const ENV_DIGEST: &str = "GITPRISM_POLICY_SHA256";
const DOMAIN: &[u8] = b"gitprism-policy\0";

/// The already-authenticated policy used for one command invocation.
pub(crate) struct VerifiedPolicy {
    pub(crate) config_raw: String,
    pub(crate) ignore_raw: String,
    pub(crate) config: Config,
    pub(crate) exclude_list: ExcludeList,
}

/// Read the exact control-file bytes and return the canonical digest. This is
/// intentionally a raw-byte operation: `policy-hash` must not parse or act on
/// repository-controlled configuration.
pub(crate) fn hash_files(config_path: &Path, ignore_path: &Path) -> Result<String> {
    let config = read_control_file(config_path)?;
    let ignore = read_ignore(ignore_path)?;
    Ok(digest_bytes(&config, &ignore))
}

/// Read, authenticate, and only then parse the policy files.
pub(crate) fn load(config_path: &Path, ignore_path: &Path) -> Result<VerifiedPolicy> {
    let config_raw = read_control_file(config_path)?;
    let ignore_raw = read_ignore(ignore_path)?;
    load_from_bytes(config_path, ignore_path, config_raw, ignore_raw)
}

pub(crate) fn load_from_bytes(
    config_path: &Path,
    ignore_path: &Path,
    config_raw: Vec<u8>,
    ignore_raw: Vec<u8>,
) -> Result<VerifiedPolicy> {
    let digest = digest_bytes(&config_raw, &ignore_raw);
    verify_expected_digest(&digest)?;

    parse_verified_bytes(config_path, ignore_path, config_raw, ignore_raw)
}

#[cfg(test)]
fn load_from_bytes_with_expected(
    config_path: &Path,
    ignore_path: &Path,
    config_raw: Vec<u8>,
    ignore_raw: Vec<u8>,
    expected: &str,
) -> Result<VerifiedPolicy> {
    let digest = digest_bytes(&config_raw, &ignore_raw);
    verify_digest(&digest, expected)?;

    parse_verified_bytes(config_path, ignore_path, config_raw, ignore_raw)
}

fn parse_verified_bytes(
    config_path: &Path,
    ignore_path: &Path,
    config_raw: Vec<u8>,
    ignore_raw: Vec<u8>,
) -> Result<VerifiedPolicy> {
    let config_text = std::str::from_utf8(&config_raw)
        .with_context(|| format!("config at {} is not valid UTF-8", config_path.display()))?;
    let ignore_text = std::str::from_utf8(&ignore_raw)
        .with_context(|| format!("{} is not valid UTF-8", ignore_path.display()))?;
    let config = Config::parse(config_text, config_path)?;
    let exclude_list = ExcludeList::from_contents(ignore_text)
        .with_context(|| format!("parsing {}", ignore_path.display()))?;
    Ok(VerifiedPolicy {
        config_raw: config_text.to_owned(),
        ignore_raw: ignore_text.to_owned(),
        config,
        exclude_list,
    })
}

/// `.gitprism.toml` and `.gitprismignore` are authenticated by their exact
/// bytes (decisions/0026) — every other file in a checkout stays free to go
/// through git's ordinary text/CRLF filtering (`core.autocrlf`,
/// `.gitattributes`) exactly like any other tracked file. libgit2 applies
/// that same filtering to these two files during an ordinary checkout, so a
/// working tree materialized on a machine with, e.g., `core.autocrlf=true`
/// can silently change their on-disk bytes without their blob ever changing
/// — invalidating a deployment's pinned `GITPRISM_POLICY_SHA256` for reasons
/// that have nothing to do with an actual content change.
///
/// Re-materialize both straight from `tree`'s blobs after any checkout that
/// might have touched them, bypassing the working-tree filter pipeline
/// entirely. This is scoped to just these two files rather than disabling
/// filters for the whole checkout (`CheckoutBuilder::disable_filters`),
/// which would also strip filtering the repository owner legitimately
/// relies on for their own source/dest content.
pub(crate) fn restore_control_files_exact(
    repo: &git2::Repository,
    tree: &git2::Tree,
) -> Result<()> {
    let workdir = repo
        .workdir()
        .context("resolving the working directory to restore control files exactly")?;
    let mut restored = Vec::new();
    for filename in [crate::config::FILENAME, crate::exclude::FILENAME] {
        let Some(entry) = tree.get_name(filename) else {
            continue;
        };
        let blob = repo
            .find_blob(entry.id())
            .with_context(|| format!("reading the {filename} blob to restore it exactly"))?;
        // decisions/0033's recovery paths never trust a plain overwrite of an
        // existing path — something racing checkout could have replaced it
        // with a symlink or hardlink pointing outside the repository between
        // checkout finishing and this restore running. Remove whatever is
        // there first and recreate it fresh, the same no-follow pattern
        // `setup`'s own control-file recovery already uses. The tree's mode
        // also has to be applied here: the fresh file otherwise keeps
        // whatever default permissions `create_new` gave it, silently
        // dropping an executable control file's `100755` down to `100644`
        // on disk while the index still (correctly) points at the
        // executable blob — dirty under `core.filemode=true` despite the
        // content being byte-exact.
        let filemode = (entry.filemode() as u32) & 0o777;
        write_regular_file_no_follow(&workdir.join(filename), blob.content(), Some(filemode))
            .with_context(|| format!("restoring {filename} byte-exact after checkout"))?;
        restored.push((filename, entry.id(), entry.filemode()));
    }
    if restored.is_empty() {
        return Ok(());
    }
    // The raw write above bypasses git2's index entirely, so without
    // re-staging, the index still carries whatever checkout's own
    // (possibly filtered) write hashed to — leaving these paths reported
    // dirty forever after, even though nothing meaningful changed, tripping
    // every clean-working-tree guard `setup`/`sync` rely on.
    //
    // Re-staging via `Index::add_path` would reintroduce the same problem:
    // libgit2 hashes through the *clean* side of the working-tree filter
    // pipeline (`GIT_FILTER_TO_ODB`), not a raw hash of the on-disk bytes.
    // For an ordinary LF-stored control file that's a no-op, but a control
    // file whose blob itself already contains CRLF (e.g. authored on
    // Windows and committed byte-for-byte per decisions/0026) would get
    // clean-filtered back to LF before hashing, staging a *different* blob
    // than the one just written to disk and than the one `tree` already
    // has — the index would diverge from HEAD despite nothing having
    // changed. Bypass hashing entirely and point the index straight at the
    // oid/mode `tree` already has for this path.
    let mut index = repo
        .index()
        .context("opening the index to re-stage restored control files")?;
    for (filename, id, filemode) in restored {
        index
            .add(&git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: filemode as u32,
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags: 0,
                flags_extended: 0,
                path: filename.as_bytes().to_vec(),
            })
            .with_context(|| format!("re-staging {filename} after restoring it exactly"))?;
    }
    index
        .write()
        .context("writing the index after restoring control files exactly")?;
    Ok(())
}

/// Write `bytes` to `path`, refusing to follow whatever might already be
/// there, and — on Unix, when `mode` is given — set the resulting
/// permissions through the still-open handle rather than by path.
/// `fs::write` opens the path with ordinary create/truncate semantics, which
/// follows an existing symlink (or writes through an existing hardlink)
/// instead of replacing it — decisions/0033's recovery paths already reject
/// that for exactly this reason. A path-based `fs::set_permissions` call
/// after the fact reopens that same race, chmod'ing whatever now occupies
/// `path` rather than what this call just created. Remove whatever occupies
/// `path` first, create it fresh with `create_new`, and apply `mode` to the
/// open `File` before dropping it, so both the write and the permission
/// change can only ever land on the brand-new inode gitprism itself created.
pub(crate) fn write_regular_file_no_follow(
    path: &Path,
    bytes: &[u8],
    #[cfg(not(unix))] _mode: Option<u32>,
    #[cfg(unix)] mode: Option<u32>,
) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    use std::io::Write as _;
    (&file).write_all(bytes)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

fn read_ignore(path: &Path) -> Result<Vec<u8>> {
    match fs::symlink_metadata(path) {
        Ok(_) => limits::read_regular_file(path, limits::MAX_CONTROL_FILE_BYTES, "ignore file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn read_control_file(path: &Path) -> Result<Vec<u8>> {
    limits::read_regular_file(path, limits::MAX_CONTROL_FILE_BYTES, "config")
}

pub(crate) fn digest_bytes(config: &[u8], ignore: &[u8]) -> String {
    let mut input = Vec::with_capacity(DOMAIN.len() + config.len() + ignore.len() + 16);
    input.extend_from_slice(DOMAIN);
    for field in [config, ignore] {
        input.extend_from_slice(&(field.len() as u64).to_be_bytes());
        input.extend_from_slice(field);
    }
    let digest = Sha256::digest(input);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn verify_expected_digest(actual: &str) -> Result<()> {
    #[cfg(test)]
    let expected = actual.to_owned();
    #[cfg(not(test))]
    let expected = std::env::var(ENV_DIGEST).with_context(|| {
        format!("{ENV_DIGEST} must be set to the policy's 64-character SHA-256 digest")
    })?;

    verify_digest(actual, &expected)
}

fn verify_digest(actual: &str, expected: &str) -> Result<()> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("{ENV_DIGEST} must contain exactly 64 hexadecimal characters");
    }
    if !expected.eq_ignore_ascii_case(actual) {
        anyhow::bail!("{ENV_DIGEST} does not match the checked-out policy");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[cfg(unix)]
    fn restore_control_files_exact_preserves_the_tree_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let config_blob = repo
            .blob(b"branches = [\"main\"]\n\n[committer]\nname = \"gitprism\"\nemail = \"gitprism@example.com\"\n\n[dest]\nurl = \"unused\"\n")
            .unwrap();
        let ignore_blob = repo.blob(b"").unwrap();
        let mut tree_builder = repo.treebuilder(None).unwrap();
        tree_builder
            .insert(
                crate::config::FILENAME,
                config_blob,
                git2::FileMode::BlobExecutable.into(),
            )
            .unwrap();
        tree_builder
            .insert(
                crate::exclude::FILENAME,
                ignore_blob,
                git2::FileMode::Blob.into(),
            )
            .unwrap();
        let tree = repo.find_tree(tree_builder.write().unwrap()).unwrap();

        let signature = git2::Signature::now("gitprism", "gitprism@example.com").unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "control files",
            &tree,
            &[],
        )
        .unwrap();

        restore_control_files_exact(&repo, &tree).unwrap();

        let config_path = dir.path().join(crate::config::FILENAME);
        let disk_mode = fs::metadata(&config_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            disk_mode, 0o755,
            "the tree's executable bit must survive the raw restore, not fall back to create_new's default permissions"
        );

        assert!(
            repo.status_file(Path::new(crate::config::FILENAME))
                .unwrap()
                .is_empty(),
            "an executable control file must be reported clean, not dirty under core.filemode=true"
        );
    }

    #[test]
    fn digest_is_domain_separated_and_canonical() {
        let digest = digest_bytes(b"ab", b"c");
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(digest, digest_bytes(b"a", b"bc"));
    }

    #[test]
    fn missing_ignore_is_hashed_as_empty() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".gitprism.toml");
        fs::write(&config, b"config").unwrap();
        assert_eq!(
            hash_files(&config, &dir.path().join(crate::exclude::FILENAME)).unwrap(),
            digest_bytes(b"config", b"")
        );
    }

    #[test]
    fn policy_verifies_before_parsing() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".gitprism.toml");
        let ignore = dir.path().join(crate::exclude::FILENAME);
        fs::write(&config, b"not valid toml [").unwrap();
        fs::write(&ignore, b"*").unwrap();
        // In test builds the deterministic expected digest is injected by
        // `verify_expected_digest`, so this reaches the parser only after the
        // hash has been computed and checked.
        let error = match load(&config, &ignore) {
            Ok(_) => panic!("malformed config must fail after verification"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("parsing config"));
    }

    #[test]
    fn mismatched_policy_stops_before_parsing_repository_content() {
        let dir = tempdir().unwrap();
        let config = dir.path().join(".gitprism.toml");
        let ignore = dir.path().join(crate::exclude::FILENAME);
        let error = match load_from_bytes_with_expected(
            &config,
            &ignore,
            b"not valid toml [".to_vec(),
            Vec::new(),
            &"0".repeat(64),
        ) {
            Ok(_) => panic!("a mismatched policy digest must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("does not match"));
        assert!(!error.to_string().contains("parsing config"));
    }

    #[test]
    fn malformed_expected_digest_is_rejected_by_the_validation_helper() {
        let error = verify_digest("00", "not-a-digest").unwrap_err();
        assert!(error.to_string().contains(ENV_DIGEST));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_control_files_are_rejected_before_reading() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("policy.toml");
        fs::write(&target, b"[committer]\nname='x'\nemail='x@y'\n").unwrap();
        let link = dir.path().join(".gitprism.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = hash_files(&link, &dir.path().join(crate::exclude::FILENAME)).unwrap_err();
        assert!(error.to_string().contains("regular file"));
    }
}
