use std::collections::HashMap;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

use crate::marker::{self, Direction as MarkerDirection, StateKey};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MappingProvenance {
    pub(crate) branch: String,
    pub(crate) marker: Oid,
    pub(crate) raw_dest: Oid,
    pub(crate) canonical_dest: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedMapping {
    pub(crate) source: Oid,
    pub(crate) dest: Oid,
    pub(crate) provenance: Vec<MappingProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MappingLookup {
    None,
    Resolved(ResolvedMapping),
    Contradictory(String),
}

#[derive(Debug)]
pub(crate) struct MappingIndex {
    entries: HashMap<Oid, Vec<MappingProvenance>>,
    entry_count: usize,
    limit: usize,
}

impl MappingIndex {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            entry_count: 0,
            limit: crate::limits::MAX_MAPPING_ENTRIES,
        }
    }

    #[cfg(test)]
    fn with_limit(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            entry_count: 0,
            limit,
        }
    }

    pub(crate) fn reconstruct(
        repo: &Repository,
        source_heads: &[(String, Oid)],
        dest_heads: &[(String, Oid)],
        key: &StateKey,
    ) -> Result<Self> {
        let mut index = Self::new();
        for (branch, head) in source_heads {
            index.scan_source_history(repo, branch, *head, key)?;
        }
        for (branch, head) in dest_heads {
            index.scan_dest_history(repo, branch, *head, key)?;
        }
        Ok(index)
    }

    #[cfg(test)]
    pub(crate) fn resolve(
        &self,
        repo: &Repository,
        source: Oid,
    ) -> Result<Option<ResolvedMapping>> {
        match self.resolve_for_anchor(repo, source)? {
            MappingLookup::None => Ok(None),
            MappingLookup::Resolved(mapping) => Ok(Some(mapping)),
            MappingLookup::Contradictory(diagnostic) => anyhow::bail!(diagnostic),
        }
    }

    fn resolve_for_anchor(&self, repo: &Repository, source: Oid) -> Result<MappingLookup> {
        let Some(records) = self.entries.get(&source) else {
            return Ok(MappingLookup::None);
        };

        let mut by_dest: HashMap<Oid, Vec<MappingProvenance>> = HashMap::new();
        for record in records {
            by_dest
                .entry(record.canonical_dest)
                .or_default()
                .push(record.clone());
        }

        let mut destinations: Vec<Oid> = by_dest.keys().copied().collect();
        destinations.sort_by_key(ToString::to_string);
        let mut common = None;
        for candidate in &destinations {
            let mut is_common_ancestor = true;
            for other in &destinations {
                if *other != *candidate
                    && !repo
                        .graph_descendant_of(*other, *candidate)
                        .with_context(|| {
                            format!(
                                "comparing mapped dest commits {other} and {candidate} for source {source}"
                            )
                        })?
                {
                    is_common_ancestor = false;
                    break;
                }
            }
            if is_common_ancestor {
                common = Some(*candidate);
                break;
            }
        }

        let mut provenance = records.clone();
        provenance.sort_by(|left, right| {
            left.branch
                .cmp(&right.branch)
                .then_with(|| left.marker.to_string().cmp(&right.marker.to_string()))
        });
        let Some(dest) = common else {
            let details = destinations
                .iter()
                .map(|destination| {
                    let branches = by_dest
                        .get(destination)
                        .into_iter()
                        .flatten()
                        .map(|record| format!("{:?} (marker {})", record.branch, record.marker))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{destination} from branches {branches}")
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Ok(MappingLookup::Contradictory(format!(
                "source commit {source} has contradictory dest mappings: {details}"
            )));
        };

        Ok(MappingLookup::Resolved(ResolvedMapping {
            source,
            dest,
            provenance,
        }))
    }

    pub(crate) fn nearest_first_parent_mapping(
        &self,
        repo: &Repository,
        tip: Oid,
    ) -> Result<MappingLookup> {
        let Some((_, lookup)) = self.nearest_first_parent_mapping_with_distance(repo, tip)? else {
            return Ok(MappingLookup::None);
        };
        Ok(lookup)
    }

    pub(crate) fn nearest_first_parent_mapping_with_distance(
        &self,
        repo: &Repository,
        tip: Oid,
    ) -> Result<Option<(usize, MappingLookup)>> {
        let mut revwalk = first_parent_walk(repo, tip, "source")?;
        for (scanned, oid) in (&mut revwalk).enumerate() {
            if scanned >= crate::limits::MAX_MARKER_SCAN_COMMITS {
                anyhow::bail!(
                    "source anchor scan exceeds the {} commit limit",
                    crate::limits::MAX_MARKER_SCAN_COMMITS
                );
            }
            let oid = oid.context("walking source history for an exact mapping")?;
            match self.resolve_for_anchor(repo, oid)? {
                MappingLookup::None => {}
                mapping => return Ok(Some((scanned, mapping))),
            }
        }
        Ok(None)
    }

    pub(crate) fn add_source_commit(
        &mut self,
        repo: &Repository,
        branch: &str,
        oid: Oid,
        key: &StateKey,
    ) -> Result<()> {
        let commit = repo
            .find_commit(oid)
            .context("resolving a source mapping commit")?;
        let Some(parsed) = marker::parse(commit.message().unwrap_or("")) else {
            return Ok(());
        };
        let directions = [MarkerDirection::Setup, MarkerDirection::DestToSource];
        let Some(dest) = marker::verify(&commit, branch, &directions, None, key) else {
            return Ok(());
        };
        self.record(oid, dest, parsed.branch, oid, dest)
    }

    pub(crate) fn add_dest_commit(
        &mut self,
        repo: &Repository,
        branch: &str,
        oid: Oid,
        key: &StateKey,
    ) -> Result<()> {
        let commit = repo
            .find_commit(oid)
            .context("resolving a dest mapping commit")?;
        let Some(parsed) = marker::parse(commit.message().unwrap_or("")) else {
            return Ok(());
        };
        let Some(source) =
            marker::verify(&commit, branch, &[MarkerDirection::SourceToDest], None, key)
        else {
            return Ok(());
        };
        let canonical_dest = if source_to_dest_alias(&commit) {
            commit.parent_id(0).expect("alias has one parent")
        } else {
            oid
        };
        self.record(source, oid, parsed.branch, oid, canonical_dest)
    }

    fn scan_source_history(
        &mut self,
        repo: &Repository,
        branch: &str,
        head: Oid,
        key: &StateKey,
    ) -> Result<()> {
        let mut revwalk = first_parent_walk(repo, head, "source")?;
        for (scanned, oid) in (&mut revwalk).enumerate() {
            if scanned >= crate::limits::MAX_MARKER_SCAN_COMMITS {
                anyhow::bail!(
                    "source mapping history scan exceeds the {} commit limit",
                    crate::limits::MAX_MARKER_SCAN_COMMITS
                );
            }
            let oid = oid.context("walking source history for authenticated mappings")?;
            self.add_source_commit(repo, branch, oid, key)?;
        }
        Ok(())
    }

    fn scan_dest_history(
        &mut self,
        repo: &Repository,
        branch: &str,
        head: Oid,
        key: &StateKey,
    ) -> Result<()> {
        let mut revwalk = first_parent_walk(repo, head, "dest")?;
        for (scanned, oid) in (&mut revwalk).enumerate() {
            if scanned >= crate::limits::MAX_MARKER_SCAN_COMMITS {
                anyhow::bail!(
                    "dest mapping history scan exceeds the {} commit limit",
                    crate::limits::MAX_MARKER_SCAN_COMMITS
                );
            }
            let oid = oid.context("walking dest history for authenticated mappings")?;
            self.add_dest_commit(repo, branch, oid, key)?;
        }
        Ok(())
    }

    fn record(
        &mut self,
        source: Oid,
        raw_dest: Oid,
        branch: String,
        marker: Oid,
        canonical_dest: Oid,
    ) -> Result<()> {
        let record = MappingProvenance {
            branch,
            marker,
            raw_dest,
            canonical_dest,
        };
        if self
            .entries
            .get(&source)
            .is_some_and(|records| records.contains(&record))
        {
            return Ok(());
        }
        if self.entry_count >= self.limit {
            anyhow::bail!("mapping index exceeds the {} entry limit", self.limit);
        }
        self.entry_count += 1;
        self.entries.entry(source).or_default().push(record);
        Ok(())
    }
}

fn first_parent_walk<'repo>(
    repo: &'repo Repository,
    head: Oid,
    side: &str,
) -> Result<git2::Revwalk<'repo>> {
    let mut revwalk = repo
        .revwalk()
        .with_context(|| format!("starting {side} mapping history scan"))?;
    revwalk
        .push(head)
        .with_context(|| format!("seeding {side} mapping history scan"))?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .with_context(|| format!("ordering {side} mapping history scan"))?;
    revwalk
        .simplify_first_parent()
        .with_context(|| format!("restricting {side} mapping history scan to first-parent"))?;
    Ok(revwalk)
}

fn source_to_dest_alias(commit: &git2::Commit<'_>) -> bool {
    commit.parent_count() == 1
        && commit
            .parent(0)
            .ok()
            .is_some_and(|parent| parent.tree_id() == commit.tree_id())
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::marker;

    fn tree_with_file(repo: &Repository, parent: Option<Oid>, name: &str, body: &[u8]) -> Oid {
        let mut builder = repo
            .treebuilder(
                parent
                    .and_then(|oid| repo.find_commit(oid).ok())
                    .as_ref()
                    .and_then(|commit| commit.tree().ok())
                    .as_ref(),
            )
            .unwrap();
        let blob = repo.blob(body).unwrap();
        builder
            .insert(name, blob, git2::FileMode::Blob.into())
            .unwrap();
        builder.write().unwrap()
    }

    fn commit(
        repo: &Repository,
        reference: &str,
        parents: &[Oid],
        tree: Oid,
        message: &str,
    ) -> Oid {
        let signature = if message.contains("Gitprism-State:") {
            git2::Signature::now("gitprism", "gitprism@example.com").unwrap()
        } else {
            git2::Signature::now("test", "test@example.com").unwrap()
        };
        let parent_commits = parents
            .iter()
            .map(|oid| repo.find_commit(*oid).unwrap())
            .collect::<Vec<_>>();
        let parent_refs = parent_commits.iter().collect::<Vec<_>>();
        repo.commit(
            Some(reference),
            &signature,
            &signature,
            message,
            &repo.find_tree(tree).unwrap(),
            &parent_refs,
        )
        .unwrap()
    }

    fn marker_message(
        _repo: &Repository,
        direction: MarkerDirection,
        branch: &str,
        counterpart: Oid,
        parents: &[Oid],
        tree: Oid,
    ) -> String {
        let signature = git2::Signature::now("gitprism", "gitprism@example.com").unwrap();
        marker::build_message(
            "test mapping",
            direction,
            branch,
            counterpart,
            match direction {
                MarkerDirection::SourceToDest => "Gitprism-Source-Commit",
                _ => "Gitprism-Dest-Commit",
            },
            parents,
            tree,
            &signature,
            &signature,
            &marker::load_key().unwrap(),
        )
    }

    fn empty_repo() -> (tempfile::TempDir, Repository, Oid) {
        let dir = tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let tree = repo.treebuilder(None).unwrap().write().unwrap();
        let root = commit(&repo, "refs/heads/root", &[], tree, "root");
        (dir, repo, root)
    }

    #[test]
    fn mapping_index_reconstructs_verified_markers_and_normalizes_anchors() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let dest_tree = tree_with_file(&repo, Some(root), "dest.txt", b"dest");
        let dest_message = marker_message(
            &repo,
            MarkerDirection::SourceToDest,
            "feature",
            source,
            &[root],
            dest_tree,
        );
        let dest = commit(&repo, "refs/heads/dest", &[root], dest_tree, &dest_message);
        let setup_tree = tree_with_file(&repo, Some(root), "setup.txt", b"setup");
        let setup_message = marker_message(
            &repo,
            MarkerDirection::Setup,
            "feature",
            root,
            &[root],
            setup_tree,
        );
        let setup = commit(
            &repo,
            "refs/heads/feature-source",
            &[root],
            setup_tree,
            &setup_message,
        );
        let dest_to_source_tree = tree_with_file(&repo, Some(setup), "back.txt", b"back");
        let dest_to_source_message = marker_message(
            &repo,
            MarkerDirection::DestToSource,
            "feature",
            dest,
            &[setup],
            dest_to_source_tree,
        );
        let dest_to_source = commit(
            &repo,
            "refs/heads/feature-source",
            &[setup],
            dest_to_source_tree,
            &dest_to_source_message,
        );
        let invalid_tree = tree_with_file(&repo, Some(dest), "invalid.txt", b"invalid");
        let mut invalid_message = marker_message(
            &repo,
            MarkerDirection::SourceToDest,
            "feature",
            source,
            &[dest],
            invalid_tree,
        );
        let mac = invalid_message
            .rfind('0')
            .unwrap_or_else(|| invalid_message.len() - 2);
        invalid_message.replace_range(mac..=mac, "1");
        let invalid = commit(
            &repo,
            "refs/heads/dest",
            &[dest],
            invalid_tree,
            &invalid_message,
        );
        assert_eq!(
            marker::verify(
                &repo.find_commit(dest).unwrap(),
                "feature",
                &[MarkerDirection::SourceToDest],
                None,
                &marker::load_key().unwrap(),
            ),
            Some(source)
        );

        let index = MappingIndex::reconstruct(
            &repo,
            &[("feature".to_owned(), dest_to_source)],
            &[("feature".to_owned(), invalid)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let source_mapping = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(source_mapping.dest, dest);
        assert!(
            source_mapping
                .provenance
                .iter()
                .all(|record| record.branch == "feature")
        );
        assert_eq!(index.resolve(&repo, setup).unwrap().unwrap().dest, root);
        assert_eq!(
            index.resolve(&repo, dest_to_source).unwrap().unwrap().dest,
            dest
        );
        assert!(index.resolve(&repo, invalid).unwrap().is_none());
    }

    #[test]
    fn mapping_index_collapses_empty_aliases_and_chooses_a_comparable_ancestor() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let base_tree = tree_with_file(&repo, Some(root), "base.txt", b"base");
        let base = commit(
            &repo,
            "refs/heads/dest",
            &[root],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[root],
                base_tree,
            ),
        );
        let alias_one = commit(
            &repo,
            "refs/heads/dest",
            &[base],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[base],
                base_tree,
            ),
        );
        let alias_two = commit(
            &repo,
            "refs/heads/dest",
            &[alias_one],
            base_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[alias_one],
                base_tree,
            ),
        );
        let desc_tree = tree_with_file(&repo, Some(alias_two), "desc.txt", b"desc");
        let desc = commit(
            &repo,
            "refs/heads/dest",
            &[alias_two],
            desc_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "feature",
                source,
                &[alias_two],
                desc_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[("feature".to_owned(), desc)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let resolved = index.resolve(&repo, source).unwrap().unwrap();
        assert_eq!(resolved.dest, base);
        assert_eq!(resolved.provenance.len(), 4);
        assert!(
            resolved
                .provenance
                .iter()
                .any(|record| record.raw_dest == alias_one && record.canonical_dest == base)
        );
        assert!(
            resolved
                .provenance
                .iter()
                .any(|record| record.raw_dest == desc && record.canonical_dest == desc)
        );
    }

    #[test]
    fn mapping_index_reports_incomparable_dest_mappings_with_provenance() {
        let (_dir, repo, root) = empty_repo();
        let source_tree = tree_with_file(&repo, Some(root), "source.txt", b"source");
        let source = commit(&repo, "refs/heads/source", &[root], source_tree, "source");
        let left_tree = tree_with_file(&repo, Some(root), "left.txt", b"left");
        let left = commit(
            &repo,
            "refs/heads/left",
            &[root],
            left_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "left",
                source,
                &[root],
                left_tree,
            ),
        );
        let right_tree = tree_with_file(&repo, Some(root), "right.txt", b"right");
        let right = commit(
            &repo,
            "refs/heads/right",
            &[root],
            right_tree,
            &marker_message(
                &repo,
                MarkerDirection::SourceToDest,
                "right",
                source,
                &[root],
                right_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[],
            &[("left".to_owned(), left), ("right".to_owned(), right)],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let error = index.resolve(&repo, source).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&source.to_string()));
        assert!(message.contains(&left.to_string()));
        assert!(message.contains(&right.to_string()));
        assert!(message.contains("left"));
        assert!(message.contains("right"));
        assert!(message.contains("contradictory"));
    }

    #[test]
    fn mapping_index_rejects_more_than_the_aggregate_entry_limit() {
        let mut index = MappingIndex::with_limit(2);
        let source = Oid::from_bytes(&[1; 20]).unwrap();
        for n in 0..2 {
            let mut bytes = [0; 20];
            bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            index
                .record(
                    source,
                    Oid::from_bytes(&bytes).unwrap(),
                    format!("branch-{n}"),
                    source,
                    Oid::from_bytes(&bytes).unwrap(),
                )
                .unwrap();
        }
        assert_eq!(index.entry_count, 2);
        let error = index
            .record(
                source,
                Oid::from_bytes(&[9; 20]).unwrap(),
                "overflow".to_owned(),
                source,
                Oid::from_bytes(&[9; 20]).unwrap(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("entry limit"));
        assert_eq!(index.entry_count, 2);
        assert_eq!(index.entries.get(&source).unwrap().len(), 2);
    }

    #[test]
    fn mapping_index_counts_an_inherited_setup_marker_once_across_source_heads() {
        let (_dir, repo, root) = empty_repo();
        let setup_tree = tree_with_file(&repo, Some(root), "setup.txt", b"setup");
        let setup = commit(
            &repo,
            "refs/heads/setup",
            &[root],
            setup_tree,
            &marker_message(
                &repo,
                MarkerDirection::Setup,
                "origin",
                root,
                &[root],
                setup_tree,
            ),
        );
        let index = MappingIndex::reconstruct(
            &repo,
            &[("first".to_owned(), setup), ("second".to_owned(), setup)],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let mapping = index.resolve(&repo, setup).unwrap().unwrap();
        assert_eq!(mapping.dest, root);
        assert_eq!(mapping.provenance.len(), 1);
        assert_eq!(index.entry_count, 1);
    }

    #[test]
    fn nearest_mapping_walks_first_parent_and_ignores_mapped_side_parent() {
        // The ordinary and filtered-only commits have no authenticated
        // mapping; the merge's mapped side parent must not be considered.
        let (_dir, repo, root) = empty_repo();
        let mapped_tree = tree_with_file(&repo, Some(root), "mapped.txt", b"mapped");
        let mapped = commit(
            &repo,
            "refs/heads/main",
            &[root],
            mapped_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "main",
                root,
                &[root],
                mapped_tree,
            ),
        );
        let ordinary_tree = tree_with_file(&repo, Some(mapped), "ordinary.txt", b"ordinary");
        let ordinary = commit(
            &repo,
            "refs/heads/main",
            &[mapped],
            ordinary_tree,
            "ordinary",
        );
        let filtered_tree = tree_with_file(&repo, Some(ordinary), "secret.txt", b"filtered");
        let filtered = commit(
            &repo,
            "refs/heads/main",
            &[ordinary],
            filtered_tree,
            "filtered-only change",
        );
        let side_tree = tree_with_file(&repo, Some(filtered), "side.txt", b"side");
        let side = commit(
            &repo,
            "refs/heads/side",
            &[filtered],
            side_tree,
            &marker_message(
                &repo,
                MarkerDirection::DestToSource,
                "side",
                Oid::from_bytes(&[7; 20]).unwrap(),
                &[filtered],
                side_tree,
            ),
        );
        let merge = commit(
            &repo,
            "refs/heads/main",
            &[filtered, side],
            filtered_tree,
            "merge side",
        );

        let index = MappingIndex::reconstruct(
            &repo,
            &[("main".to_owned(), merge), ("side".to_owned(), side)],
            &[],
            &marker::load_key().unwrap(),
        )
        .unwrap();
        let side_mapping = index.resolve(&repo, side).unwrap().unwrap();
        assert_eq!(side_mapping.source, side);
        let nearest = match index.nearest_first_parent_mapping(&repo, merge).unwrap() {
            MappingLookup::Resolved(mapping) => mapping,
            other => panic!("expected a resolved mapping, got {other:?}"),
        };
        assert_eq!(nearest.source, mapped);
        assert_eq!(nearest.dest, root);
        assert!(
            nearest
                .provenance
                .iter()
                .all(|record| record.branch == "main")
        );
    }

    #[test]
    fn nearest_mapping_propagates_missing_tip_errors() {
        let (_dir, repo, _root) = empty_repo();
        let index = MappingIndex::new();
        let missing = Oid::from_bytes(&[8; 20]).unwrap();
        let error = index
            .nearest_first_parent_mapping(&repo, missing)
            .unwrap_err();
        assert!(!error.to_string().contains("contradictory"));
    }
}
