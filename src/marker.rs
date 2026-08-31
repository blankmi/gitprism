//! Authenticated mapping state carried by gitprism-generated commits.
//!
//! The human-readable mapping trailers remain in commit messages for
//! inspection, but they are not trusted by themselves: a repository author
//! can write arbitrary commit messages. The final state block is an HMAC
//! over the complete commit shape and the pair-specific mapping, so copied,
//! edited, or forged trailers cannot move a resume boundary.

#[cfg(not(test))]
use std::env;

#[cfg(not(test))]
use anyhow::Context;
use anyhow::Result;
use git2::{Commit, Oid, Signature};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

use crate::limits;

const ENV_KEY: &str = "GITPRISM_STATE_KEY";
const VERSION: &str = "v1";
const KEY_BYTES: usize = 32;
const MAC_BYTES: usize = 32;

/// A validated pair key. Keeping the bytes in a private type prevents
/// callers from accidentally logging or passing the raw secret to git.
pub(crate) struct StateKey([u8; KEY_BYTES]);

/// The authenticated direction of a generated commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Setup,
    SourceToDest,
    DestToSource,
    ResolveSourceToDestState,
    ResolveSourceToDestPatch,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::SourceToDest => "source-to-dest",
            Self::DestToSource => "dest-to-source",
            Self::ResolveSourceToDestState => "resolve-source-to-dest-state",
            Self::ResolveSourceToDestPatch => "resolve-source-to-dest-patch",
        }
    }
}

/// The parsed, but not yet verified, final state block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedMarker {
    pub(crate) direction: Direction,
    pub(crate) branch: String,
    pub(crate) counterpart: Oid,
    mac: [u8; MAC_BYTES],
    pub(crate) body: String,
}

/// Read and validate the external state key. Tests use a fixed key so unit
/// and repository fixtures do not race on process-global environment state.
pub(crate) fn load_key() -> Result<StateKey> {
    #[cfg(test)]
    let raw = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    #[cfg(not(test))]
    let raw = env::var(ENV_KEY)
        .with_context(|| format!("{ENV_KEY} must be set to exactly 64 hexadecimal characters"))?;
    #[cfg(test)]
    {
        parse_key(raw)
    }
    #[cfg(not(test))]
    {
        parse_key(&raw)
    }
}

fn parse_key(raw: &str) -> Result<StateKey> {
    if raw.len() != KEY_BYTES * 2 {
        anyhow::bail!("{ENV_KEY} must contain exactly 64 hexadecimal characters")
    }
    let mut key = [0; KEY_BYTES];
    for (index, pair) in raw.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        key[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Ok(StateKey(key))
}

fn hex_value(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => anyhow::bail!("{ENV_KEY} must contain only hexadecimal characters"),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0xf) as usize] as char);
    }
    encoded
}

fn hex_decode_mac(raw: &str) -> Option<[u8; MAC_BYTES]> {
    if raw.len() != MAC_BYTES * 2 {
        return None;
    }
    let mut mac = [0; MAC_BYTES];
    for (index, pair) in raw.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        mac[index] = ((hex_value(pair[0]).ok()? << 4) | hex_value(pair[1]).ok()?) as u8;
    }
    Some(mac)
}

fn reserved_marker_line(line: &str) -> bool {
    [
        "Gitprism-State:",
        "Gitprism-Direction:",
        "Gitprism-Counterpart:",
        "Gitprism-Branch:",
        "Gitprism-MAC:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// Remove all reserved gitprism-looking trailers before preserving a user's
/// message. This keeps a normal commit's prose intact while preventing a
/// copied marker or mapping trailer from becoming a second trusted block.
pub(crate) fn sanitize_body(message: &str) -> String {
    message
        .lines()
        .filter(|line| !line.starts_with("Gitprism-"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

#[allow(clippy::too_many_arguments)]
fn payload(
    direction: Direction,
    branch: &str,
    counterpart: Oid,
    parents: &[Oid],
    tree: Oid,
    author: &Signature<'_>,
    committer: &Signature<'_>,
    body: &str,
) -> Vec<u8> {
    // Length-prefix every field so names, messages, and OIDs cannot create
    // an alternate interpretation of the same byte stream.
    let fields = [
        VERSION.to_owned(),
        direction.as_str().to_owned(),
        branch.to_owned(),
        counterpart.to_string(),
        parents
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(","),
        tree.to_string(),
        signature_field(author),
        signature_field(committer),
        body.to_owned(),
    ];
    let mut bytes = b"gitprism-state\0".to_vec();
    for field in fields {
        let field = field.as_bytes();
        bytes.extend_from_slice(&(field.len() as u64).to_be_bytes());
        bytes.extend_from_slice(field);
    }
    bytes
}

fn signature_field(signature: &Signature<'_>) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        hex_encode(signature.name_bytes()),
        hex_encode(signature.email_bytes()),
        signature.when().seconds(),
        signature.when().offset_minutes()
    )
}

fn calculate_mac(key: &StateKey, payload: &[u8]) -> [u8; MAC_BYTES] {
    let mut mac = HmacSha256::new_from_slice(&key.0).expect("HMAC accepts every key length");
    mac.update(payload);
    let bytes = mac.finalize().into_bytes();
    let mut result = [0; MAC_BYTES];
    result.copy_from_slice(&bytes);
    result
}

/// Build a commit message with the human-readable mapping trailer and the
/// canonical authenticated block at the very end.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_message(
    original: &str,
    direction: Direction,
    branch: &str,
    counterpart: Oid,
    mapping_key: &str,
    parents: &[Oid],
    tree: Oid,
    author: &Signature<'_>,
    committer: &Signature<'_>,
    key: &StateKey,
) -> String {
    let body = sanitize_body(original);
    let mapping = format!("{mapping_key}: {counterpart}");
    let mac = calculate_mac(
        key,
        &payload(
            direction,
            branch,
            counterpart,
            parents,
            tree,
            author,
            committer,
            &body,
        ),
    );
    let prefix = if body.is_empty() {
        mapping
    } else {
        format!("{body}\n\n{mapping}")
    };
    format!(
        "{prefix}\n\nGitprism-State: {VERSION}\nGitprism-Direction: {}\nGitprism-Branch: {branch}\nGitprism-Counterpart: {counterpart}\nGitprism-MAC: {}\n",
        direction.as_str(),
        hex_encode(&mac)
    )
}

/// Parse only the exact five-line final block. A malformed or duplicated
/// reserved field is ordinary user content and therefore returns `None`.
pub(crate) fn parse(message: &str) -> Option<ParsedMarker> {
    if message.len() > limits::MAX_COMMIT_MESSAGE_BYTES {
        return None;
    }
    let normalized = message.strip_suffix('\n').unwrap_or(message);
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines.len() < 5 {
        return None;
    }
    let final_lines = &lines[lines.len() - 5..];
    let version = final_lines[0].strip_prefix("Gitprism-State: ")?;
    if version != VERSION
        || lines[..lines.len() - 5]
            .iter()
            .any(|line| reserved_marker_line(line))
    {
        return None;
    }
    let direction = match final_lines[1].strip_prefix("Gitprism-Direction: ")? {
        "setup" => Direction::Setup,
        "source-to-dest" => Direction::SourceToDest,
        "dest-to-source" => Direction::DestToSource,
        "resolve-source-to-dest-state" => Direction::ResolveSourceToDestState,
        "resolve-source-to-dest-patch" => Direction::ResolveSourceToDestPatch,
        _ => return None,
    };
    let branch = final_lines[2].strip_prefix("Gitprism-Branch: ")?;
    if branch.is_empty() || branch.chars().any(char::is_control) {
        return None;
    }
    let counterpart = Oid::from_str(final_lines[3].strip_prefix("Gitprism-Counterpart: ")?).ok()?;
    let mac = hex_decode_mac(final_lines[4].strip_prefix("Gitprism-MAC: ")?)?;
    let prefix = lines[..lines.len() - 5].join("\n");
    let mapping_prefix = format!(
        "Gitprism-{}-Commit: {counterpart}",
        match direction {
            Direction::SourceToDest
            | Direction::ResolveSourceToDestState
            | Direction::ResolveSourceToDestPatch => "Source",
            Direction::Setup | Direction::DestToSource => "Dest",
        }
    );
    let body = prefix
        .trim_end()
        .strip_suffix(&mapping_prefix)?
        .trim_end()
        .to_owned();
    Some(ParsedMarker {
        direction,
        branch: branch.to_owned(),
        counterpart,
        mac,
        body,
    })
}

/// Verify a marker against the actual commit object and expected branch,
/// direction, and counterpart. HMAC verification is constant-time.
pub(crate) fn verify(
    commit: &Commit<'_>,
    branch: &str,
    expected: &[Direction],
    counterpart: Option<Oid>,
    key: &StateKey,
) -> Option<Oid> {
    verify_parsed(commit, Some(branch), expected, counterpart, key).map(|marker| marker.counterpart)
}

/// Same authentication as [`verify`], for a caller whose scanning context
/// has no independent branch expectation of its own — a marker inherited
/// as an ordinary ancestor commit on some other branch's history is
/// verified against its own recorded branch (decisions/0046), which the
/// two-argument branch check below always accepts by construction. Returns
/// the full [`ParsedMarker`] on success so a caller that already needs the
/// parsed fields (e.g. `branch`) doesn't parse the message a second time
/// (decisions/0046 addendum): parsing and the size gate both run exactly
/// once per commit here, not once in a caller's own pre-parse and again
/// inside verification.
pub(crate) fn verify_self(
    commit: &Commit<'_>,
    expected: &[Direction],
    counterpart: Option<Oid>,
    key: &StateKey,
) -> Option<ParsedMarker> {
    verify_parsed(commit, None, expected, counterpart, key)
}

/// Shared implementation for [`verify`] and [`verify_self`]. `branch` is
/// `None` for the self-verifying case, skipping the branch-name check
/// entirely rather than comparing a value against itself.
fn verify_parsed(
    commit: &Commit<'_>,
    branch: Option<&str>,
    expected: &[Direction],
    counterpart: Option<Oid>,
    key: &StateKey,
) -> Option<ParsedMarker> {
    if commit.message_bytes().len() > limits::MAX_COMMIT_MESSAGE_BYTES {
        return None;
    }
    let message = std::str::from_utf8(commit.message_bytes()).ok()?;
    let marker = parse(message)?;
    // A setup graft is deliberately inherited by every source branch cut
    // from it (decisions/0017). Later directional markers are branch-local.
    if branch.is_some_and(|branch| marker.direction != Direction::Setup && marker.branch != branch)
        || !expected.contains(&marker.direction)
        || counterpart.is_some_and(|expected| expected != marker.counterpart)
    {
        return None;
    }
    let parents = (0..commit.parent_count())
        .filter_map(|index| commit.parent_id(index).ok())
        .collect::<Vec<_>>();
    let author = commit.author();
    let committer = commit.committer();
    let mut verifier = HmacSha256::new_from_slice(&key.0).ok()?;
    verifier.update(&payload(
        marker.direction,
        &marker.branch,
        marker.counterpart,
        &parents,
        commit.tree_id(),
        &author,
        &committer,
        &marker.body,
    ));
    verifier.verify_slice(&marker.mac).ok()?;
    Some(marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn key_validation_requires_exactly_64_hex_chars() {
        assert!(parse_key(&"00".repeat(32)).is_ok());
        assert!(parse_key("00").is_err());
        assert!(parse_key(&format!("{}g", "0".repeat(63))).is_err());
    }

    #[test]
    fn forged_duplicate_and_malformed_blocks_are_not_parsed() {
        let oid = Oid::ZERO_SHA1;
        let message = build_message(
            "body",
            Direction::DestToSource,
            "main",
            oid,
            "Gitprism-Dest-Commit",
            &[],
            oid,
            &Signature::now("a", "a@example.com").unwrap(),
            &Signature::now("c", "c@example.com").unwrap(),
            &load_key().unwrap(),
        );
        assert!(parse(&format!("Gitprism-State: v1\n{message}")).is_none());
        assert!(parse("Gitprism-State: v1\nGitprism-State: v1\nGitprism-Direction: setup\nGitprism-Counterpart: 0000000000000000000000000000000000000000\nGitprism-MAC: 00").is_none());
    }

    #[test]
    fn sanitize_removes_reserved_and_mapping_lines() {
        assert_eq!(
            sanitize_body("hello\nGitprism-Dest-Commit: dead\nworld"),
            "hello\nworld"
        );
    }

    #[test]
    fn repository_forged_and_duplicate_markers_are_ignored() {
        let dir = tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let tree = {
            let blob = repo.blob(b"content").unwrap();
            let mut builder = repo.treebuilder(None).unwrap();
            builder
                .insert("file", blob, git2::FileMode::Blob.into())
                .unwrap();
            repo.find_tree(builder.write().unwrap()).unwrap()
        };
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let key = load_key().unwrap();
        let counterpart = Oid::ZERO_SHA1;
        let valid_message = build_message(
            "setup",
            Direction::DestToSource,
            "main",
            counterpart,
            "Gitprism-Dest-Commit",
            &[],
            tree.id(),
            &signature,
            &signature,
            &key,
        );
        let valid = repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                &valid_message,
                &tree,
                &[],
            )
            .unwrap();
        let valid_commit = repo.find_commit(valid).unwrap();
        assert_eq!(
            verify(
                &valid_commit,
                "main",
                &[Direction::DestToSource],
                None,
                &key
            ),
            Some(counterpart)
        );
        assert!(
            verify(
                &valid_commit,
                "release",
                &[Direction::DestToSource],
                None,
                &key
            )
            .is_none(),
            "an authenticated marker must not be reusable by another branch"
        );

        let forged = repo
            .commit(
                None,
                &signature,
                &signature,
                "ordinary user commit\n\nGitprism-Dest-Commit: 0000000000000000000000000000000000000000\n",
                &tree,
                &[],
            )
            .unwrap();
        assert!(
            verify(
                &repo.find_commit(forged).unwrap(),
                "main",
                &[Direction::DestToSource],
                None,
                &key
            )
            .is_none()
        );

        let duplicate = repo
            .commit(
                None,
                &signature,
                &signature,
                &format!("Gitprism-State: v1\n{valid_message}"),
                &tree,
                &[],
            )
            .unwrap();
        assert!(
            verify(
                &repo.find_commit(duplicate).unwrap(),
                "main",
                &[Direction::DestToSource],
                None,
                &key
            )
            .is_none()
        );
    }

    #[test]
    fn oversized_marker_messages_are_rejected_before_parsing() {
        let message = "x".repeat(limits::MAX_COMMIT_MESSAGE_BYTES + 1);
        assert!(parse(&message).is_none());
    }

    #[test]
    fn verify_self_rejects_an_oversized_message_before_parsing_it() {
        // decisions/0046 addendum: `verify_self` is the single parse+verify
        // pass a scan now uses instead of a caller's own pre-parse plus
        // `verify`'s internal re-parse — the size gate must still run
        // before any line-scanning, on this one remaining pass.
        let dir = tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        let key = load_key().unwrap();
        let valid_message = build_message(
            "body",
            Direction::DestToSource,
            "main",
            Oid::ZERO_SHA1,
            "Gitprism-Dest-Commit",
            &[],
            tree.id(),
            &signature,
            &signature,
            &key,
        );
        // Pad the message past the size limit while keeping the trailing
        // authenticated block intact and byte-identical, so an oversized
        // rejection here can only come from the size gate, never from the
        // MAC failing to verify over the now-longer message.
        let padding = "x".repeat(limits::MAX_COMMIT_MESSAGE_BYTES);
        let oversized_message = format!("{padding}\n\n{valid_message}");
        let oversized = repo
            .commit(None, &signature, &signature, &oversized_message, &tree, &[])
            .unwrap();
        assert!(
            verify_self(
                &repo.find_commit(oversized).unwrap(),
                &[Direction::DestToSource],
                None,
                &key,
            )
            .is_none()
        );
    }
}
