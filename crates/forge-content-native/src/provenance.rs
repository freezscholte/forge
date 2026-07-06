//! Path provenance walk over native history (NER-362).
//!
//! Walks commits tip→genesis along the FIRST-parent chain (matching the `forge log`
//! walk convention) and emits one entry per commit whose tree diff against its first
//! parent touches the queried path. Read-only over the object store; ledger
//! enrichment is a separate concern (task 362-4, in `forge_store`).

use crate::{
    diff_native_trees, CommitObject, DiffOptions, NativeObjectStore, NativeRefStore, ObjectId,
};
use anyhow::{bail, Context, Result};

/// One commit's touch of the queried path, tip-first. Provenance fields are copied
/// verbatim from [`CommitObject`] — never synthesized or defaulted to non-`None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathProvenanceEntry {
    pub commit_id: String,
    pub change: String,
    pub intent_id: Option<String>,
    pub proposal_revision_id: Option<String>,
    pub decision_id: Option<String>,
    pub evidence_digest: Option<String>,
    pub actor: Option<String>,
    pub authored_time: Option<i64>,
}

/// Walk native history from `HEAD` back to genesis (first parent only, so a merge
/// commit is diffed against its first parent — the `forge log` convention) and return
/// the commits that touched `path`, tip first. Genesis counts as `"added"` when the
/// path exists in its tree. A rename reports the commit as `"renamed"` and the walk
/// continues following the OLD name further back. Empty when the path never existed
/// (or no `HEAD` has been written yet).
pub fn path_provenance(store: &NativeObjectStore, path: &str) -> Result<Vec<PathProvenanceEntry>> {
    let Some(head) = NativeRefStore::new(&store.root).read_head()? else {
        return Ok(Vec::new());
    };
    path_provenance_at(store, &head, path)
}

/// [`path_provenance`] rooted at an explicit `tip` commit instead of the ref-store
/// `HEAD`. Callers that already hold the authoritative tip (e.g. the ledger-derived
/// tip, which the ref-store HEAD lags by design) pass it here so the walk cannot see
/// a stale HEAD.
pub fn path_provenance_at(
    store: &NativeObjectStore,
    tip: &ObjectId,
    path: &str,
) -> Result<Vec<PathProvenanceEntry>> {
    let mut entries = Vec::new();
    // Hunks are never surfaced by provenance entries; skipping them keeps the walk
    // from reading every touched blob's content at each commit.
    let options = DiffOptions {
        include_hunks: false,
        ..DiffOptions::default()
    };
    let mut tracked = path.to_string();
    let mut cursor = Some(tip.clone());
    while let Some(commit_id) = cursor {
        let commit = store.read_commit(&commit_id)?;
        let tree = ObjectId::parse(&commit.tree)?;
        let Some(first_parent) = commit.parents.first() else {
            // Genesis: no parent to diff against; the path counts as added iff it
            // exists in the genesis tree.
            if store.tree_fingerprints(&tree)?.contains_key(&tracked) {
                entries.push(entry_for(&commit_id, &commit, "added"));
            }
            break;
        };
        let parent_id = ObjectId::parse(first_parent)?;
        let parent_tree = ObjectId::parse(&store.read_commit(&parent_id)?.tree)?;
        let diff = diff_native_trees(store, &parent_tree, &tree, &options)?;
        for file in &diff.files {
            if file.path != tracked {
                continue;
            }
            entries.push(entry_for(&commit_id, &commit, change_label(&file.status)?));
            if let Some(old_path) = &file.old_path {
                // Renamed here: keep following the OLD name further back.
                tracked = old_path.clone();
            }
            break;
        }
        cursor = Some(parent_id);
    }
    Ok(entries)
}

/// One HEAD line attributed to the most recent first-parent commit that introduced
/// or last changed its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineAttribution {
    pub line_number: usize,
    pub content: String,
    pub commit_id: String,
}

/// Attribute every line of the file at `HEAD` to the most recent commit (tip→genesis,
/// first parent only) in which that line's content was introduced or last changed,
/// computed from line matches (LCS) between consecutive blob versions of the path.
/// Renames are followed through the OLD name, matching [`path_provenance`]. Read-only
/// over the object store. Non-UTF-8 blobs and a path missing at `HEAD` are typed errors.
pub fn attribute_lines(store: &NativeObjectStore, path: &str) -> Result<Vec<LineAttribution>> {
    let Some(head) = NativeRefStore::new(&store.root).read_head()? else {
        bail!("path {path} does not exist at HEAD (no native HEAD yet)");
    };
    attribute_lines_at(store, &head, path)
}

/// [`attribute_lines`] rooted at an explicit `tip` commit instead of the ref-store
/// `HEAD`. Callers that already hold the authoritative tip (e.g. the ledger-derived
/// tip, which the ref-store HEAD lags by design) pass it here so attribution cannot
/// see a stale HEAD.
pub fn attribute_lines_at(
    store: &NativeObjectStore,
    tip: &ObjectId,
    path: &str,
) -> Result<Vec<LineAttribution>> {
    let head_tree = ObjectId::parse(&store.read_commit(tip)?.tree)?;
    let mut current_lines = match blob_lines(store, &head_tree, path)? {
        Some(lines) => lines,
        None => bail!("path {path} does not exist at {tip}"),
    };
    // pending[i] = Some(h): line i of the version at the walk cursor is (so far)
    // unchanged since HEAD line h and still awaits an owning commit.
    let mut pending: Vec<Option<usize>> = (0..current_lines.len()).map(Some).collect();
    let mut attributions: Vec<Option<String>> = vec![None; current_lines.len()];
    let head_lines = current_lines.clone();
    let options = DiffOptions {
        include_hunks: false,
        ..DiffOptions::default()
    };
    let mut tracked = path.to_string();
    let mut cursor = tip.clone();
    loop {
        let commit = store.read_commit(&cursor)?;
        let tree = ObjectId::parse(&commit.tree)?;
        let Some(first_parent) = commit.parents.first() else {
            // Genesis: every line still pending was introduced here.
            attribute_pending(&pending, &cursor, &mut attributions);
            break;
        };
        let parent_id = ObjectId::parse(first_parent)?;
        let parent_tree = ObjectId::parse(&store.read_commit(&parent_id)?.tree)?;
        let diff = diff_native_trees(store, &parent_tree, &tree, &options)?;
        if let Some(file) = diff.files.iter().find(|f| f.path == tracked) {
            match change_label(&file.status)? {
                "added" => {
                    attribute_pending(&pending, &cursor, &mut attributions);
                    break;
                }
                "modified" | "renamed" => {
                    let parent_path = file.old_path.as_deref().unwrap_or(&tracked).to_string();
                    let Some(parent_lines) = blob_lines(store, &parent_tree, &parent_path)? else {
                        bail!(
                            "native line attribution walk lost path {parent_path} \
                             in parent of commit {cursor}"
                        );
                    };
                    let matched = match_lines(&parent_lines, &current_lines);
                    let mut next_pending: Vec<Option<usize>> = vec![None; parent_lines.len()];
                    for (child_idx, head_idx) in pending.iter().enumerate() {
                        let Some(head_idx) = head_idx else { continue };
                        match matched[child_idx] {
                            // Unchanged here: keep waiting at the parent's position.
                            Some(parent_idx) => next_pending[parent_idx] = Some(*head_idx),
                            // Introduced or last changed by this commit.
                            None => attributions[*head_idx] = Some(cursor.to_string()),
                        }
                    }
                    tracked = parent_path;
                    current_lines = parent_lines;
                    pending = next_pending;
                }
                other => bail!(
                    "unsupported change {other} for path {tracked} \
                     in native line attribution walk at commit {cursor}"
                ),
            }
        }
        if attributions.iter().all(Option::is_some) {
            break;
        }
        cursor = parent_id;
    }
    head_lines
        .into_iter()
        .zip(attributions)
        .enumerate()
        .map(|(idx, (content, commit_id))| {
            let commit_id = commit_id.with_context(|| {
                format!(
                    "native line attribution walk left line {} of {path} unattributed",
                    idx + 1
                )
            })?;
            Ok(LineAttribution {
                line_number: idx + 1,
                content,
                commit_id,
            })
        })
        .collect()
}

fn attribute_pending(
    pending: &[Option<usize>],
    commit_id: &ObjectId,
    attributions: &mut [Option<String>],
) {
    for head_idx in pending.iter().flatten() {
        attributions[*head_idx] = Some(commit_id.to_string());
    }
}

/// Read the blob at `path` inside `tree` and split it into lines (trailing newline
/// dropped; a missing final newline still yields a last line). `None` when the path
/// has no file leaf in the tree; a typed error when the blob is not UTF-8 text.
fn blob_lines(
    store: &NativeObjectStore,
    tree: &ObjectId,
    path: &str,
) -> Result<Option<Vec<String>>> {
    let fingerprints = store.tree_fingerprints(tree)?;
    let Some((blob_id, _mode)) = fingerprints.get(path) else {
        return Ok(None);
    };
    let bytes = store.read_object(&ObjectId::parse(blob_id)?)?;
    let Ok(text) = String::from_utf8(bytes) else {
        bail!("cannot attribute lines of {path}: blob is binary (not UTF-8 text)");
    };
    Ok(Some(
        text.split_terminator('\n').map(str::to_string).collect(),
    ))
}

/// For each child line index, the parent line index it matches (same content, order
/// preserved), or `None` when the line has no match — i.e. it changed in the child.
/// Common prefix/suffix are matched directly; the middle uses a classic LCS table.
fn match_lines(parent: &[String], child: &[String]) -> Vec<Option<usize>> {
    let mut matched = vec![None; child.len()];
    let max_prefix = parent.len().min(child.len());
    let mut prefix = 0;
    while prefix < max_prefix && parent[prefix] == child[prefix] {
        matched[prefix] = Some(prefix);
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < max_prefix - prefix
        && parent[parent.len() - 1 - suffix] == child[child.len() - 1 - suffix]
    {
        matched[child.len() - 1 - suffix] = Some(parent.len() - 1 - suffix);
        suffix += 1;
    }
    let parent_mid = &parent[prefix..parent.len() - suffix];
    let child_mid = &child[prefix..child.len() - suffix];
    if parent_mid.is_empty() || child_mid.is_empty() {
        return matched;
    }
    // The LCS table below is O(parent_mid * child_mid) memory. A substantial
    // rewrite of a large file (little common prefix/suffix) would make it
    // quadratic in the file size at every walk step, so past this bound the
    // matcher degrades instead of allocating: the trimmed middle stays
    // unmatched and those lines attribute to the child commit — the same
    // degrade-not-fail convention as DiffOptions::rename_limit. ~64M cells
    // (8 bytes each) caps the table at ~512 MiB.
    const LCS_CELL_LIMIT: usize = 64 * 1024 * 1024;
    if (parent_mid.len() + 1).saturating_mul(child_mid.len() + 1) > LCS_CELL_LIMIT {
        return matched;
    }
    // LCS length table over the trimmed middle; lcs[i][j] covers parent_mid[i..],
    // child_mid[j..].
    let mut lcs = vec![vec![0usize; child_mid.len() + 1]; parent_mid.len() + 1];
    for i in (0..parent_mid.len()).rev() {
        for j in (0..child_mid.len()).rev() {
            lcs[i][j] = if parent_mid[i] == child_mid[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < parent_mid.len() && j < child_mid.len() {
        if parent_mid[i] == child_mid[j] {
            matched[prefix + j] = Some(prefix + i);
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matched
}

/// Map the diff engine's git name-status letter encoding (`A`/`M`/`D`, `R<score>`)
/// onto the provenance change vocabulary.
fn change_label(status: &str) -> Result<&'static str> {
    match status {
        "A" => Ok("added"),
        "M" => Ok("modified"),
        "D" => Ok("deleted"),
        _ if status.starts_with('R') => Ok("renamed"),
        _ => bail!("unsupported native diff status in provenance walk: {status}"),
    }
}

fn entry_for(commit_id: &ObjectId, commit: &CommitObject, change: &str) -> PathProvenanceEntry {
    PathProvenanceEntry {
        commit_id: commit_id.to_string(),
        change: change.to_string(),
        intent_id: commit.intent_id.clone(),
        proposal_revision_id: commit.proposal_revision_id.clone(),
        decision_id: commit.decision_id.clone(),
        evidence_digest: commit.evidence_digest.as_ref().map(|d| d.to_string()),
        actor: commit.actor.clone(),
        authored_time: commit.authored_time,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileEntry, Hex64, NativeObjectStore, NativeRefStore, COMMIT_SCHEMA_VERSION};
    use std::fs;
    use std::path::Path;

    fn write_tree(repo: &Path, files: &[(&str, &[u8])]) -> ObjectId {
        for (path, bytes) in files {
            let full = repo.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, bytes).unwrap();
        }
        let entries: Vec<FileEntry> = files
            .iter()
            .map(|(path, _)| FileEntry {
                path: (*path).to_string(),
                executable: false,
                symlink_target: None,
            })
            .collect();
        crate::write_tree(&NativeObjectStore::new(repo), repo, &entries, "").unwrap()
    }

    fn commit(store: &NativeObjectStore, tree: &ObjectId, parents: &[&ObjectId]) -> ObjectId {
        store
            .write_commit(&CommitObject {
                schema_version: COMMIT_SCHEMA_VERSION,
                tree: tree.to_string(),
                parents: parents.iter().map(|p| p.to_string()).collect(),
                intent_id: None,
                proposal_revision_id: None,
                decision_id: None,
                evidence_digest: None,
                actor: None,
                authored_time: None,
            })
            .unwrap()
    }

    fn set_head(repo: &Path, tip: &ObjectId) {
        NativeRefStore::new(repo).set_head(tip).unwrap();
    }

    fn changes(entries: &[PathProvenanceEntry]) -> Vec<(&str, &str)> {
        entries
            .iter()
            .map(|e| (e.commit_id.as_str(), e.change.as_str()))
            .collect()
    }

    #[test]
    fn match_lines_degrades_without_allocating_past_the_lcs_cell_limit() {
        // Two fully divergent inputs whose middle crosses LCS_CELL_LIMIT
        // (8200 * 8200 > 64M cells): the matcher must return all-unmatched
        // (attributing every line to the child commit) instead of building
        // the quadratic table — this returns instantly; the unbounded path
        // would allocate ~½ GiB here.
        let parent: Vec<String> = (0..8200).map(|i| format!("old line {i}")).collect();
        let child: Vec<String> = (0..8200).map(|i| format!("new line {i}")).collect();
        let matched = match_lines(&parent, &child);
        assert_eq!(matched.len(), child.len());
        assert!(matched.iter().all(Option::is_none));
    }

    #[test]
    fn match_lines_still_matches_shared_prefix_and_suffix_past_the_limit() {
        // Even past the cell limit, prefix/suffix matching still attributes
        // unchanged flanks to the parent; only the divergent middle degrades.
        let mut parent: Vec<String> = vec!["keep head".to_string()];
        parent.extend((0..8200).map(|i| format!("old line {i}")));
        parent.push("keep tail".to_string());
        let mut child: Vec<String> = vec!["keep head".to_string()];
        child.extend((0..8200).map(|i| format!("new line {i}")));
        child.push("keep tail".to_string());
        let matched = match_lines(&parent, &child);
        assert_eq!(matched[0], Some(0));
        assert_eq!(matched[child.len() - 1], Some(parent.len() - 1));
        assert!(matched[1..child.len() - 1].iter().all(Option::is_none));
    }

    #[test]
    fn provenance_walks_added_then_modified_tip_first() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"one\n"), ("other.txt", b"x\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let modified_tree = write_tree(repo, &[("app.txt", b"two\n"), ("other.txt", b"x\n")]);
        let tip = commit(&store, &modified_tree, &[&genesis]);
        set_head(repo, &tip);

        let entries = path_provenance(&store, "app.txt").unwrap();

        assert_eq!(
            changes(&entries),
            vec![
                (tip.to_string().as_str(), "modified"),
                (genesis.to_string().as_str(), "added"),
            ]
        );
    }

    #[test]
    fn provenance_skips_commits_that_do_not_touch_the_path() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("other.txt", b"x\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let added_tree = write_tree(repo, &[("other.txt", b"x\n"), ("app.txt", b"one\n")]);
        let added = commit(&store, &added_tree, &[&genesis]);
        let unrelated_tree = write_tree(repo, &[("other.txt", b"y\n"), ("app.txt", b"one\n")]);
        let tip = commit(&store, &unrelated_tree, &[&added]);
        set_head(repo, &tip);

        let entries = path_provenance(&store, "app.txt").unwrap();

        assert_eq!(
            changes(&entries),
            vec![(added.to_string().as_str(), "added")]
        );
    }

    #[test]
    fn provenance_reports_deletion_and_the_prior_history() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"one\n"), ("keep.txt", b"k\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let deleted_tree = write_tree(repo, &[("keep.txt", b"k\n")]);
        let tip = commit(&store, &deleted_tree, &[&genesis]);
        set_head(repo, &tip);

        let entries = path_provenance(&store, "app.txt").unwrap();

        assert_eq!(
            changes(&entries),
            vec![
                (tip.to_string().as_str(), "deleted"),
                (genesis.to_string().as_str(), "added"),
            ]
        );
    }

    #[test]
    fn provenance_follows_the_old_name_back_through_a_rename() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let content = b"line one\nline two\nline three\n";
        let genesis_tree = write_tree(repo, &[("old.txt", content), ("keep.txt", b"k\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let renamed_tree = write_tree(repo, &[("new.txt", content), ("keep.txt", b"k\n")]);
        let renamed = commit(&store, &renamed_tree, &[&genesis]);
        let modified_tree = write_tree(
            repo,
            &[
                ("new.txt", b"line one\nline TWO\nline three\n".as_slice()),
                ("keep.txt", b"k\n"),
            ],
        );
        let tip = commit(&store, &modified_tree, &[&renamed]);
        set_head(repo, &tip);

        let entries = path_provenance(&store, "new.txt").unwrap();

        assert_eq!(
            changes(&entries),
            vec![
                (tip.to_string().as_str(), "modified"),
                (renamed.to_string().as_str(), "renamed"),
                (genesis.to_string().as_str(), "added"),
            ]
        );
    }

    #[test]
    fn provenance_is_empty_when_the_path_never_existed_or_head_is_unset() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        assert!(path_provenance(&store, "app.txt").unwrap().is_empty());

        let genesis_tree = write_tree(repo, &[("other.txt", b"x\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        set_head(repo, &genesis);
        assert!(path_provenance(&store, "app.txt").unwrap().is_empty());
    }

    #[test]
    fn provenance_diffs_merge_commits_against_first_parent_only() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let ours_tree = write_tree(repo, &[("app.txt", b"ours\n")]);
        let ours = commit(&store, &ours_tree, &[]);
        let theirs_tree = write_tree(repo, &[("app.txt", b"theirs\n")]);
        let theirs = commit(&store, &theirs_tree, &[]);
        // The merge keeps the first parent's tree: against `ours` the path is
        // untouched, so the merge must not emit an entry even though the diff
        // against `theirs` would classify it as modified.
        let merge = commit(&store, &ours_tree, &[&ours, &theirs]);
        set_head(repo, &merge);

        let entries = path_provenance(&store, "app.txt").unwrap();

        assert_eq!(
            changes(&entries),
            vec![(ours.to_string().as_str(), "added")]
        );
    }

    #[test]
    fn provenance_copies_justification_fields_verbatim() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("keep.txt", b"k\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let added_tree = write_tree(repo, &[("keep.txt", b"k\n"), ("app.txt", b"one\n")]);
        let digest = "a".repeat(64);
        let tip = store
            .write_commit(&CommitObject {
                schema_version: COMMIT_SCHEMA_VERSION,
                tree: added_tree.to_string(),
                parents: vec![genesis.to_string()],
                intent_id: Some("intent_1".to_string()),
                proposal_revision_id: Some("rev_1".to_string()),
                decision_id: Some("decision_1".to_string()),
                evidence_digest: Some(Hex64::new(&digest).unwrap()),
                actor: Some("agent".to_string()),
                authored_time: Some(1_234_567_890),
            })
            .unwrap();
        set_head(repo, &tip);

        let entries = path_provenance(&store, "app.txt").unwrap();

        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.commit_id, tip.to_string());
        assert_eq!(entry.change, "added");
        assert_eq!(entry.intent_id.as_deref(), Some("intent_1"));
        assert_eq!(entry.proposal_revision_id.as_deref(), Some("rev_1"));
        assert_eq!(entry.decision_id.as_deref(), Some("decision_1"));
        assert_eq!(entry.evidence_digest.as_deref(), Some(digest.as_str()));
        assert_eq!(entry.actor.as_deref(), Some("agent"));
        assert_eq!(entry.authored_time, Some(1_234_567_890));
    }

    fn blame(entries: &[LineAttribution]) -> Vec<(usize, &str, &str)> {
        entries
            .iter()
            .map(|e| (e.line_number, e.content.as_str(), e.commit_id.as_str()))
            .collect()
    }

    #[test]
    fn attribution_keeps_unchanged_lines_on_the_introducing_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"alpha\nbeta\ngamma\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let modified_tree = write_tree(repo, &[("app.txt", b"alpha\nBETA\ngamma\n")]);
        let tip = commit(&store, &modified_tree, &[&genesis]);
        set_head(repo, &tip);

        let lines = attribute_lines(&store, "app.txt").unwrap();

        let genesis_id = genesis.to_string();
        let tip_id = tip.to_string();
        assert_eq!(
            blame(&lines),
            vec![
                (1, "alpha", genesis_id.as_str()),
                (2, "BETA", tip_id.as_str()),
                (3, "gamma", genesis_id.as_str()),
            ]
        );
    }

    #[test]
    fn attribution_assigns_inserted_lines_to_the_inserting_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"one\ntwo\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let inserted_tree = write_tree(repo, &[("app.txt", b"zero\none\nmid\ntwo\n")]);
        let inserted = commit(&store, &inserted_tree, &[&genesis]);
        // A commit that touches another file must not steal attribution.
        let unrelated_tree = write_tree(
            repo,
            &[("app.txt", b"zero\none\nmid\ntwo\n"), ("other.txt", b"x\n")],
        );
        let tip = commit(&store, &unrelated_tree, &[&inserted]);
        set_head(repo, &tip);

        let lines = attribute_lines(&store, "app.txt").unwrap();

        let genesis_id = genesis.to_string();
        let inserted_id = inserted.to_string();
        assert_eq!(
            blame(&lines),
            vec![
                (1, "zero", inserted_id.as_str()),
                (2, "one", genesis_id.as_str()),
                (3, "mid", inserted_id.as_str()),
                (4, "two", genesis_id.as_str()),
            ]
        );
    }

    #[test]
    fn attribution_follows_renames_back_to_the_original_lines() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let content = b"line one\nline two\nline three\n";
        let genesis_tree = write_tree(repo, &[("old.txt", content), ("keep.txt", b"k\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let renamed_tree = write_tree(repo, &[("new.txt", content), ("keep.txt", b"k\n")]);
        let renamed = commit(&store, &renamed_tree, &[&genesis]);
        let modified_tree = write_tree(
            repo,
            &[
                ("new.txt", b"line one\nline TWO\nline three\n".as_slice()),
                ("keep.txt", b"k\n"),
            ],
        );
        let tip = commit(&store, &modified_tree, &[&renamed]);
        set_head(repo, &tip);

        let lines = attribute_lines(&store, "new.txt").unwrap();

        let genesis_id = genesis.to_string();
        let tip_id = tip.to_string();
        assert_eq!(
            blame(&lines),
            vec![
                (1, "line one", genesis_id.as_str()),
                (2, "line TWO", tip_id.as_str()),
                (3, "line three", genesis_id.as_str()),
            ]
        );
    }

    #[test]
    fn attribution_assigns_everything_to_a_readd_after_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"one\ntwo\n"), ("keep.txt", b"k\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let deleted_tree = write_tree(repo, &[("keep.txt", b"k\n")]);
        let deleted = commit(&store, &deleted_tree, &[&genesis]);
        let readded_tree = write_tree(repo, &[("app.txt", b"one\ntwo\n"), ("keep.txt", b"k\n")]);
        let tip = commit(&store, &readded_tree, &[&deleted]);
        set_head(repo, &tip);

        let lines = attribute_lines(&store, "app.txt").unwrap();

        let tip_id = tip.to_string();
        assert_eq!(
            blame(&lines),
            vec![(1, "one", tip_id.as_str()), (2, "two", tip_id.as_str())]
        );
    }

    #[test]
    fn attribution_counts_a_final_line_without_trailing_newline() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"one\ntwo")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        set_head(repo, &genesis);

        let lines = attribute_lines(&store, "app.txt").unwrap();

        let genesis_id = genesis.to_string();
        assert_eq!(
            blame(&lines),
            vec![
                (1, "one", genesis_id.as_str()),
                (2, "two", genesis_id.as_str())
            ]
        );
    }

    #[test]
    fn attribution_rejects_binary_blobs_with_a_typed_error() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.bin", b"\xff\xfe\x00\x01".as_slice())]);
        let genesis = commit(&store, &genesis_tree, &[]);
        set_head(repo, &genesis);

        let err = attribute_lines(&store, "app.bin").unwrap_err();

        assert!(
            err.to_string().contains("binary"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn attribution_errors_when_the_path_is_missing_at_head() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let missing_before_head = attribute_lines(&store, "app.txt").unwrap_err();
        assert!(
            missing_before_head.to_string().contains("app.txt"),
            "unexpected error: {missing_before_head}"
        );

        let genesis_tree = write_tree(repo, &[("other.txt", b"x\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        set_head(repo, &genesis);

        let err = attribute_lines(&store, "app.txt").unwrap_err();
        assert!(
            err.to_string().contains("app.txt"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn attribution_is_deterministic_for_a_given_store_state() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let genesis_tree = write_tree(repo, &[("app.txt", b"a\nb\nc\nd\n")]);
        let genesis = commit(&store, &genesis_tree, &[]);
        let tip_tree = write_tree(repo, &[("app.txt", b"a\nB\nc\nd\ne\n")]);
        let tip = commit(&store, &tip_tree, &[&genesis]);
        set_head(repo, &tip);

        let first = attribute_lines(&store, "app.txt").unwrap();
        let second = attribute_lines(&store, "app.txt").unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|l| l.line_number).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
    }
}
