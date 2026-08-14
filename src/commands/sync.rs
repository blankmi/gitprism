//! `gitprism sync` — see design/playbooks/0001-gitlab-pipeline-triggers.md,
//! design/decisions/0003-mapping-state-in-commit-trailers.md,
//! design/decisions/0007-conflict-policy-hard-stop.md, and
//! design/decisions/0009-push-race-refetch-and-recompute.md.
//!
//! Source→dest only, for now: for every configured branch pair
//! (decisions/0005), independently, fetch dest's current tip, find every
//! source commit not yet reflected there, filter each one against the
//! *current* exclude-list (decisions/0004, 0011 — not a historical
//! reconstruction of what it looked like at that commit) and re-parent it
//! onto dest's tip, then push the result — fast-forward only, never forced
//! (requirements/0001).
//!
//! dest→source (the merge-back direction that can hit decisions/0007's
//! conflict hard-stop) is not implemented yet — so if dest's tip carries any
//! commit gitprism didn't put there since its own last push, this refuses to
//! sync that pair at all rather than fast-forwarding a snapshot that would
//! silently drop dest's independent content.
//!
//! Same discovery convention as `setup` (decisions/0012): `cwd` is a
//! starting point for git-style upward discovery, and a relative `--config`
//! resolves against the discovered root, not the invoking subdirectory. But
//! unlike `setup`, `sync` runs against a repo that already has history — its
//! `.gitprism.toml`/`.gitprismignore` are read from the working tree, which
//! mirrors the committed tree on an ordinary checkout.

use std::path::Path;

use anyhow::{Context, Result};
use git2::{Oid, Repository, Signature};

use crate::config::{BranchPair, Config};
use crate::exclude::{self, ExcludeList};
use crate::git;

/// A lost fast-forward race (decisions/0009) is refetched and recomputed
/// from scratch this many times before sync gives up and fails loudly. Exact
/// bound is an implementation detail, not a design fork.
const MAX_RACE_RETRIES: u32 = 3;

pub fn run(cwd: &Path, config_path: &Path) -> Result<()> {
    let repo = Repository::discover(cwd).with_context(|| {
        format!(
            "gitprism sync must be run inside an existing git repository (none found at or above {}) — has `gitprism setup` been run?",
            cwd.display()
        )
    })?;
    let source_root = repo
        .workdir()
        .context("gitprism sync requires a repo with a working tree, not a bare repo")?
        .to_path_buf();

    let config_path = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        source_root.join(config_path)
    };
    let config = Config::load(&config_path)?;

    for pair in &config.pairs {
        sync_pair_to_dest(&repo, &source_root, &config, pair).with_context(|| {
            format!("syncing {:?} -> {:?}", pair.source_branch, pair.dest_branch)
        })?;
    }

    Ok(())
}

/// Pushes `pair.source_branch`'s pending commits to `pair.dest_branch`,
/// filtered, one branch pair at a time. Recomputes from scratch (refetch,
/// rebuild, retry) on a lost fast-forward race rather than rebasing what it
/// already built (decisions/0009).
fn sync_pair_to_dest(
    repo: &Repository,
    source_root: &Path,
    config: &Config,
    pair: &BranchPair,
) -> Result<()> {
    let source_tip = repo
        .find_branch(&pair.source_branch, git2::BranchType::Local)
        .with_context(|| format!("resolving source branch {:?}", pair.source_branch))?
        .get()
        .peel_to_commit()
        .with_context(|| {
            format!(
                "resolving source branch {:?} to a commit",
                pair.source_branch
            )
        })?
        .id();

    // The exclude-list *current* as of this sync run, loaded once — not
    // reloaded per pending commit. Decisions/0004 is explicit that a change
    // to it applies to whatever's being processed right now, not a
    // historical reconstruction of what it looked like when each commit was
    // originally made.
    let exclude_list = load_current_exclude_list(repo, source_tip)?;

    let mut attempt = 0;
    loop {
        git::fetch(source_root, &config.dest.url, &pair.dest_branch).with_context(|| {
            format!(
                "fetching dest branch {:?} from {:?}",
                pair.dest_branch, config.dest.url
            )
        })?;
        let dest_tip = repo
            .find_reference("FETCH_HEAD")
            .context("reading FETCH_HEAD after fetch")?
            .peel_to_commit()
            .context("resolving fetched dest branch to a commit")?
            .id();

        // There is no safe way to build a new commit straight from source's
        // filtered snapshot and fast-forward dest onto it unless this
        // clone's source_tip is known to be caught up with whatever dest
        // last synced from — either because dest carries independent
        // content gitprism didn't create (dest→source isn't implemented
        // yet, so that must land in source first), or because this clone's
        // own source branch is behind or diverged from the source commit
        // dest was actually last synced from (e.g. another clone already
        // pushed for this pair). Either way, proceeding could silently drop
        // content some other commit already contributed, even though the
        // ref update itself would be a legitimate fast-forward.
        let boundary = dest_resume_point(repo, source_tip, dest_tip)?.with_context(|| {
            format!(
                "gitprism sync: dest branch {:?} isn't at a point this clone can safely build on — either dest has content gitprism didn't create (sync dest→source first, not yet implemented), or this clone's {:?} is behind or diverged from what dest was last synced from (fetch/pull the latest source history first)",
                pair.dest_branch, pair.source_branch
            )
        })?;

        match build_pending_dest_tip(repo, config, &exclude_list, boundary, dest_tip, source_tip)? {
            None => return Ok(()), // nothing pending, or everything filtered empty
            Some(new_dest_tip) => {
                match git::push(
                    source_root,
                    &config.dest.url,
                    new_dest_tip,
                    &pair.dest_branch,
                )? {
                    git::PushOutcome::Accepted => return Ok(()),
                    git::PushOutcome::RejectedNotFastForward if attempt < MAX_RACE_RETRIES => {
                        // dest's tip moved between fetch and push — refetch
                        // and recompute against its new state rather than
                        // rebasing what was already built (decisions/0009).
                        attempt += 1;
                        continue;
                    }
                    git::PushOutcome::RejectedNotFastForward => anyhow::bail!(
                        "gitprism sync: pushing {:?} kept losing a fast-forward race after {} retries",
                        pair.dest_branch,
                        MAX_RACE_RETRIES
                    ),
                }
            }
        }
    }
}

/// Builds, in `repo`'s object database, a chain of new commits reflecting
/// every source commit between `boundary` and `source_tip` that still has
/// content left after filtering — returning the chain's tip, or `None` if
/// there's nothing to push. `dest_tip` seeds the chain's first parent.
fn build_pending_dest_tip(
    repo: &Repository,
    config: &Config,
    exclude_list: &ExcludeList,
    boundary: Oid,
    dest_tip: Oid,
    source_tip: Oid,
) -> Result<Option<Oid>> {
    let pending = pending_source_commits(repo, boundary, source_tip)?;

    let mut parent = dest_tip;
    let mut built_any = false;
    for source_oid in pending {
        let source_commit = repo
            .find_commit(source_oid)
            .context("resolving a pending source commit")?;

        // Loop prevention (decisions/0003): a source commit that itself came
        // from dest (dest→source sync) already exists on dest — pushing it
        // back would loop.
        if trailer_value(
            source_commit.message().unwrap_or(""),
            "Gitprism-Dest-Commit",
        )
        .is_some()
        {
            continue;
        }

        let filtered_tree_oid =
            filter_tree(repo, &source_commit.tree()?, Path::new(""), exclude_list)?;
        let parent_commit = repo
            .find_commit(parent)
            .context("resolving the in-progress dest chain's parent")?;

        // Filtering removed everything this commit changed — must not push
        // an empty commit (requirements/0001).
        if filtered_tree_oid == parent_commit.tree_id() {
            continue;
        }

        parent = build_dest_commit(repo, config, parent, &source_commit, filtered_tree_oid)?;
        built_any = true;
    }

    Ok(built_any.then_some(parent))
}

/// Whether `dest_tip` is a point gitprism already accounts for — either it's
/// the tip of gitprism's own last push (carries a `Gitprism-Source-Commit`
/// trailer directly), or dest hasn't advanced at all since `setup`'s graft
/// (decisions/0006), i.e. no sync has landed yet and nothing independent has
/// landed either. Returns the source-space commit to resume from (for
/// [`pending_source_commits`]) on success, or `None` if dest carries history
/// gitprism doesn't recognize.
///
/// Deliberately only ever looks at `dest_tip` itself, not dest's whole
/// history: if `dest_tip` isn't itself a known sync point, dest has moved
/// past one — whether that extra commit came from an independent PR or a
/// still-unsynced-to-source change, it's exactly the case this must refuse
/// (see this module's doc comment).
///
/// A `Gitprism-Source-Commit` trailer is just a claim embedded in dest's
/// commit message, though — it names whatever source commit *some* clone
/// last synced from, not necessarily an ancestor of *this* clone's
/// `source_tip`. Trusting it unconditionally would let a source clone that's
/// behind or divergent from the one that produced it rebuild its own (older
/// or different) full snapshot on top of dest and silently drop content the
/// trusted commit already contributed — so it's only accepted once
/// `source_tip` is verified to actually descend from it.
fn dest_resume_point(repo: &Repository, source_tip: Oid, dest_tip: Oid) -> Result<Option<Oid>> {
    let dest_commit = repo
        .find_commit(dest_tip)
        .context("resolving dest's tip commit")?;
    if let Some(value) = trailer_value(
        dest_commit.message().unwrap_or(""),
        "Gitprism-Source-Commit",
    ) {
        let boundary = Oid::from_str(value)
            .with_context(|| format!("parsing Gitprism-Source-Commit trailer {value:?}"))?;

        // The trailer might name a commit this clone doesn't even have — a
        // sibling clone's own source commit is never transmitted to dest,
        // only the filtered commit it produced is, so an unrelated or
        // behind clone has no way to have fetched it. That's just as unsafe
        // to build on as a confirmed non-ancestor, so it's checked (and
        // rejected) before asking libgit2 to compare ancestry, whose own
        // error surface for a missing object isn't a clean `NotFound` here.
        if boundary != source_tip && repo.find_commit(boundary).is_err() {
            return Ok(None);
        }

        let source_tip_descends_from_it = boundary == source_tip
            || repo.graph_descendant_of(source_tip, boundary).with_context(|| {
                format!(
                    "checking whether {source_tip} descends from the Gitprism-Source-Commit trailer {boundary}"
                )
            })?;
        return Ok(source_tip_descends_from_it.then_some(boundary));
    }

    let graft_point = repo.merge_base(source_tip, dest_tip).context(
        "no shared history between source and dest for this pair — has `gitprism setup` been run?",
    )?;
    Ok((graft_point == dest_tip).then_some(graft_point))
}

/// Every source commit strictly after `boundary` up to and including
/// `tip`, oldest first — the order later commits may depend on must be
/// preserved (decisions/0007's "Consequences").
fn pending_source_commits(repo: &Repository, boundary: Oid, tip: Oid) -> Result<Vec<Oid>> {
    let mut revwalk = repo
        .revwalk()
        .context("starting source's pending-commit walk")?;
    revwalk
        .push(tip)
        .context("seeding source's pending-commit walk")?;
    revwalk
        .hide(boundary)
        .context("excluding already-synced source history")?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .context("ordering source's pending commits oldest-first")?;

    revwalk
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("walking source's pending commits")
}

/// Extracts `key`'s value from a `Key: value` line anywhere in `message` —
/// the same trailer shape `setup` already writes
/// (`Gitprism-Dest-Commit: <oid>`), read back here for
/// `Gitprism-Source-Commit` (and, for loop prevention,
/// `Gitprism-Dest-Commit`).
fn trailer_value<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    message
        .lines()
        .find_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::trim)
}

/// Loads the exclude-list *current* as of `source_tip` — the version this
/// whole sync run filters every pending commit with (decisions/0004: the
/// current list applies to whatever's being processed right now, not a
/// historical reconstruction of what it looked like at each commit).
fn load_current_exclude_list(repo: &Repository, source_tip: Oid) -> Result<ExcludeList> {
    let tree = repo
        .find_commit(source_tip)
        .context("resolving source's tip commit")?
        .tree()
        .context("reading source's tip tree")?;
    let ignore_raw = match tree.get_path(Path::new(exclude::FILENAME)) {
        Ok(entry) => {
            let blob = repo
                .find_blob(entry.id())
                .context("reading .gitprismignore's blob")?;
            String::from_utf8_lossy(blob.content()).into_owned()
        }
        Err(_) => String::new(),
    };
    ExcludeList::from_contents(&ignore_raw).context("parsing .gitprismignore")
}

fn filter_tree(
    repo: &Repository,
    tree: &git2::Tree,
    prefix: &Path,
    exclude_list: &ExcludeList,
) -> Result<Oid> {
    let mut builder = repo
        .treebuilder(None)
        .context("starting a filtered tree builder")?;

    for entry in tree.iter() {
        let name = entry
            .name()
            .context("a tree entry has a non-UTF-8 name gitprism can't filter by")?;
        let rel_path = prefix.join(name);
        let is_dir = entry.kind() == Some(git2::ObjectType::Tree);

        if exclude_list.is_excluded(&rel_path, is_dir) {
            continue;
        }

        if is_dir {
            let subtree = repo
                .find_tree(entry.id())
                .with_context(|| format!("reading subtree {}", rel_path.display()))?;
            let filtered_oid = filter_tree(repo, &subtree, &rel_path, exclude_list)?;
            // A directory that excluding left with nothing in it must not
            // appear at all — git doesn't track directories independently
            // of their contents.
            if repo
                .find_tree(filtered_oid)
                .context("reading a just-filtered subtree back")?
                .iter()
                .next()
                .is_none()
            {
                continue;
            }
            builder
                .insert(name, filtered_oid, git2::FileMode::Tree.into())
                .with_context(|| format!("inserting filtered subtree {}", rel_path.display()))?;
        } else {
            builder
                .insert(name, entry.id(), entry.filemode())
                .with_context(|| format!("inserting {}", rel_path.display()))?;
        }
    }

    builder.write().context("writing a filtered tree")
}

/// Builds one new dest-bound commit in `repo`'s object database — object
/// only, no ref update, since the chain is pushed by oid once it's complete.
/// Preserves the original author, stamps gitprism's own committer identity
/// (decisions/0010), and carries the `Gitprism-Source-Commit` trailer
/// (decisions/0003) that lets a future sync resume from here.
fn build_dest_commit(
    repo: &Repository,
    config: &Config,
    parent: Oid,
    source_commit: &git2::Commit,
    filtered_tree_oid: Oid,
) -> Result<Oid> {
    let parent_commit = repo
        .find_commit(parent)
        .context("resolving the dest chain's parent commit")?;
    let tree = repo
        .find_tree(filtered_tree_oid)
        .context("reading the filtered tree")?;
    let committer = Signature::now(&config.committer.name, &config.committer.email)
        .context("building gitprism's committer signature")?;
    let message = format!(
        "{}\n\nGitprism-Source-Commit: {}\n",
        source_commit.message().unwrap_or("").trim_end(),
        source_commit.id()
    );

    repo.commit(
        None,
        &source_commit.author(),
        &committer,
        &message,
        &tree,
        &[&parent_commit],
    )
    .with_context(|| {
        format!(
            "building dest commit for source commit {}",
            source_commit.id()
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    /// A bare repo with one commit on `branch` — dest is always reached over
    /// a remote URL in real use, so its fixture is bare here too, unlike
    /// `setup`'s (which only ever fetches from dest, never pushes to it).
    fn bare_repo_with_a_commit_on(dir: &Path, branch: &str, files: &[(&str, &str)]) -> Oid {
        let repo = Repository::init_bare(dir).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Dest Author", "author@example.com").unwrap();

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

    fn write_config(dest_url: &str, pairs: &[(&str, &str)]) -> NamedTempFile {
        let pairs_toml: String = pairs
            .iter()
            .map(|(source_branch, dest_branch)| {
                format!(
                    "[[pairs]]\nsource_branch = \"{source_branch}\"\ndest_branch = \"{dest_branch}\"\n"
                )
            })
            .collect();

        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
            [committer]
            name = "gitprism"
            email = "gitprism@example.com"

            [dest]
            url = "{dest_url}"

            {pairs_toml}
            "#,
        )
        .unwrap();
        file
    }

    /// Sets up a source repo already grafted onto `dest`'s tip — exactly
    /// `gitprism setup`'s output shape (decisions/0006) — without depending
    /// on `commands::setup` itself, so these tests exercise `sync` in
    /// isolation.
    fn source_grafted_onto(
        source_dir: &Path,
        branch: &str,
        dest_tip: Oid,
        dest_repo: &Repository,
    ) -> Repository {
        let repo = Repository::init(source_dir).unwrap();
        let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
        // Re-read the dest tip's tree into the source repo's own odb via a
        // round trip through the filesystem, same as a real fetch would.
        git::fetch(source_dir, &dest_repo.path().to_string_lossy(), branch).unwrap();
        let fetched_tip_id = repo
            .find_reference("FETCH_HEAD")
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id();
        {
            let fetched_tip = repo.find_commit(fetched_tip_id).unwrap();
            let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
            repo.commit(
                Some(&format!("refs/heads/{branch}")),
                &signature,
                &signature,
                &format!(
                    // `source_dir` makes this graft commit's content unique
                    // per test fixture — two independently-`init`'d clones
                    // grafted onto the same dest tip must not collapse into
                    // the same commit object just because they also share a
                    // timestamp (a real risk: same tree, parent, message,
                    // and signature otherwise).
                    "gitprism setup: graft ({})\n\nGitprism-Dest-Commit: {}\n",
                    source_dir.display(),
                    dest_tip_commit.id()
                ),
                &fetched_tip.tree().unwrap(),
                &[&fetched_tip],
            )
            .unwrap();
        }
        repo.set_head(&format!("refs/heads/{branch}")).unwrap();
        repo.checkout_head(None).unwrap();
        repo
    }

    fn add_commit(repo: &Repository, branch: &str, files: &[(&str, &str)]) -> Oid {
        let tip = repo
            .find_branch(branch, git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        for (name, contents) in files {
            let blob = repo.blob(contents.as_bytes()).unwrap();
            builder
                .insert(*name, blob, git2::FileMode::Blob.into())
                .unwrap();
        }
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("A Developer", "dev@example.com").unwrap();

        repo.commit(
            Some(&format!("refs/heads/{branch}")),
            &signature,
            &signature,
            "a real change",
            &tree,
            &[&tip],
        )
        .unwrap()
    }

    #[test]
    fn run_pushes_a_new_source_commit_to_dest_filtered() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // `.gitprismignore` is versioned in source (decisions/0011) — it has
        // to be part of the commit itself, not just written to the
        // filesystem, since `sync` filters using each commit's *own* tree.
        add_commit(
            &source_repo,
            "main",
            &[
                ("shared.txt", "v2"),
                ("secret.txt", "only for source"),
                (exclude::FILENAME, "secret.txt\n"),
            ],
        );

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(new_dest_tip.parent_id(0).unwrap(), dest_tip);
        assert_eq!(new_dest_tip.author().name().unwrap(), "A Developer");
        assert_eq!(
            new_dest_tip.committer().email().unwrap(),
            "gitprism@example.com"
        );
        assert!(
            new_dest_tip
                .message()
                .unwrap()
                .contains("Gitprism-Source-Commit:")
        );

        let tree = new_dest_tip.tree().unwrap();
        let shared = dest_repo
            .find_blob(tree.get_name("shared.txt").unwrap().id())
            .unwrap();
        assert_eq!(shared.content(), b"v2");
        assert!(
            tree.get_name("secret.txt").is_none(),
            "an excluded file must never reach dest"
        );
        assert!(
            tree.get_name(exclude::FILENAME).is_none(),
            ".gitprismignore itself must never reach dest"
        );
    }

    #[test]
    fn run_pushes_nothing_when_the_only_pending_commit_filters_to_empty() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &source_repo,
            "main",
            &[
                ("secret.txt", "only for source"),
                (exclude::FILENAME, "secret.txt\n"),
            ],
        );

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(source_dir.path(), config.path())
            .expect("sync should succeed even with nothing to push");

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip,
            "dest must not move when every pending commit filters to empty"
        );
    }

    #[test]
    fn run_resumes_from_the_last_synced_commit_not_the_graft() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(source_dir.path(), config.path()).expect("first sync should succeed");
        // A second run with nothing new on source must be a true no-op, not
        // re-walk all the way back to the graft and re-push v2 again.
        run(source_dir.path(), config.path()).expect("second, no-op sync should succeed");

        let mut revwalk = dest_repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            2,
            "the no-op run must not add another commit"
        );
    }

    #[test]
    fn run_does_not_reflect_a_dest_originated_commit_back_to_dest() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // Simulate dest→source having already reflected a dest commit back
        // onto source: a commit carrying Gitprism-Dest-Commit.
        let tip = source_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let mut builder = source_repo.treebuilder(Some(&tip.tree().unwrap())).unwrap();
        let blob = source_repo.blob(b"from dest").unwrap();
        builder
            .insert("shared.txt", blob, git2::FileMode::Blob.into())
            .unwrap();
        let tree = source_repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("gitprism", "gitprism@example.com").unwrap();
        source_repo
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "gitprism resolve\n\nGitprism-Dest-Commit: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
                &tree,
                &[&tip],
            )
            .unwrap();

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip,
            "a commit that already came from dest must not be pushed back to dest"
        );
    }

    #[test]
    fn run_applies_the_current_exclude_list_even_to_an_already_committed_secret() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        // First commit adds a secret with no exclude rule in effect yet.
        add_commit(
            &source_repo,
            "main",
            &[("shared.txt", "v2"), ("secret.txt", "leaked?")],
        );
        // A later commit adds the exclude rule, but never touches
        // secret.txt itself — decisions/0004 says the *current* list
        // governs everything being processed this run, not a per-commit
        // historical snapshot, so this must still catch the earlier commit.
        add_commit(&source_repo, "main", &[(exclude::FILENAME, "secret.txt\n")]);

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(source_dir.path(), config.path()).expect("sync should succeed");

        let new_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        let tree = new_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("secret.txt").is_none(),
            "the current exclude-list must apply retroactively to an already-committed secret, not just commits made after the rule existed"
        );
    }

    #[test]
    fn run_refuses_to_sync_a_pair_where_dest_has_independent_commits() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);
        let dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        let source_dir = tempdir().unwrap();
        let source_repo = source_grafted_onto(source_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(&source_repo, "main", &[("shared.txt", "v2")]);

        // Simulate an independent change landing directly on dest (e.g. a
        // PR merged straight to dest) — content gitprism never put there
        // and doesn't know about yet.
        let dest_only_tip = {
            let dest_tip_commit = dest_repo.find_commit(dest_tip).unwrap();
            let mut builder = dest_repo
                .treebuilder(Some(&dest_tip_commit.tree().unwrap()))
                .unwrap();
            let blob = dest_repo.blob(b"dest-only content").unwrap();
            builder
                .insert("dest-only.txt", blob, git2::FileMode::Blob.into())
                .unwrap();
            let tree = dest_repo.find_tree(builder.write().unwrap()).unwrap();
            let signature = Signature::now("Dest Maintainer", "maintainer@example.com").unwrap();
            dest_repo
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "an independent dest-side change",
                    &tree,
                    &[&dest_tip_commit],
                )
                .unwrap()
        };

        let config = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        let err = run(source_dir.path(), config.path())
            .expect_err("dest's independent commit must block source→dest, not get overwritten");
        // `run` wraps each pair's error in its own "syncing ... -> ..."
        // context, so the specific reason is further down the chain —
        // `{:#}` renders anyhow's full chain, not just the top frame.
        assert!(format!("{err:#}").contains("dest→source"));

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_only_tip,
            "a refused sync must not touch dest's branch at all"
        );
        let tree = dest_repo
            .find_commit(dest_only_tip)
            .unwrap()
            .tree()
            .unwrap();
        assert!(
            tree.get_name("dest-only.txt").is_some(),
            "dest's independent content must survive a refused sync"
        );
    }

    #[test]
    fn run_refuses_to_sync_a_divergent_clone_even_though_dest_has_a_trailer() {
        let dest_dir = tempdir().unwrap();
        let dest_repo = Repository::init_bare(dest_dir.path()).unwrap();
        let dest_tip = bare_repo_with_a_commit_on(dest_dir.path(), "main", &[("shared.txt", "v1")]);

        // Two independent clones of the same freshly-grafted source, exactly
        // like two checkouts of one real repo — clone A syncs first.
        let clone_a_dir = tempdir().unwrap();
        let clone_a = source_grafted_onto(clone_a_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_a,
            "main",
            &[("shared.txt", "vA"), ("only-a.txt", "from A")],
        );
        let config_a = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);
        run(clone_a_dir.path(), config_a.path()).expect("clone A's sync should succeed");

        let dest_tip_after_a = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();

        // Clone B was grafted from the *same* original dest tip, before A's
        // push — its own source_tip is a sibling of A's commit, not a
        // descendant of it. Dest's tip now carries a
        // `Gitprism-Source-Commit` trailer naming A's commit, which is not
        // an ancestor of clone B's source_tip at all.
        let clone_b_dir = tempdir().unwrap();
        let clone_b = source_grafted_onto(clone_b_dir.path(), "main", dest_tip, &dest_repo);
        add_commit(
            &clone_b,
            "main",
            &[("shared.txt", "vB"), ("only-b.txt", "from B")],
        );
        let config_b = write_config(&dest_dir.path().display().to_string(), &[("main", "main")]);

        let err = run(clone_b_dir.path(), config_b.path()).expect_err(
            "a divergent clone must not rebuild its own snapshot on top of dest just because dest's tip has *some* Gitprism-Source-Commit trailer",
        );
        assert!(format!("{err:#}").contains("diverged"));

        let still_dest_tip = dest_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        assert_eq!(
            still_dest_tip.id(),
            dest_tip_after_a,
            "a refused sync must not touch dest's branch at all"
        );
        let tree = still_dest_tip.tree().unwrap();
        assert!(
            tree.get_name("only-a.txt").is_some(),
            "clone A's already-synced content must survive clone B's refused sync"
        );
        assert!(
            tree.get_name("only-b.txt").is_none(),
            "clone B's content must never have been pushed"
        );
    }
}
