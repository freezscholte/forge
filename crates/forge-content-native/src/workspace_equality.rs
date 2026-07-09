//! Read-only equality of an on-disk directory against a recorded native tree
//! (NER-382 workspace drift guard).
//!
//! This is an EQUALITY primitive, not a diff: it never writes anything (no
//! status cache — deliberately not `diff_working_vs_tree`), so a caller can
//! probe a workspace dir and refuse without leaving a trace in it.
//!
//! The actual side is enumerated with the SAME exclusion contract as the
//! native snapshot scanner (`walk_worktree`): `.gitignore` (nested, with
//! negation) and `.forgeignore` are honored **rooted at `scan_root`** — so a
//! `.gitignore` materialized inside the workspace dir applies to workspace
//! files exactly as it did when the recorded tree was built — with the
//! non-negatable `is_ignored_by_policy` backstop on top. Without this parity,
//! ignored build artifacts in the workspace would surface as permanent false
//! drift that re-materialization can never clear (they are never deleted by
//! the re-materialization pass, which honors the same ignore semantics).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use forge_content::is_ignored_by_policy;

use crate::{walk_worktree, NativeObjectStore, ObjectId, ObjectKind, SYMLINK_MODE};

/// Compare the directory at `scan_root` against the recorded native `tree` (read
/// from the store under `repo_root`), returning the sorted list of drifted
/// relative paths — empty means equal. `excluded_paths` (relative, `/`-separated,
/// e.g. an attempt's local private path labels) are dropped from BOTH sides.
///
/// Per-file semantics (bytes/paths only, mode excluded — the recorded tree pins
/// content identity, not permissions):
/// - a recorded symlink compares its link *target* bytes (never followed);
/// - a recorded regular file hashes its bytes to a blob [`ObjectId`] and
///   compares ids — no file content ever leaves this function;
/// - a path is drifted when it is missing, added, of the wrong type, or its
///   bytes hash to a different blob id.
///
/// Expected side: the recorded tree walked via the store's validating read
/// primitives ([`NativeObjectStore::tree_fingerprints`], which enforces the
/// tree schema and entry-name safety), filtered by `is_ignored_by_policy`
/// exactly like materialization (those entries were never written into the
/// workspace, so they cannot have drifted). Read-only and cache-free: the
/// filesystem under `scan_root` is only ever read.
pub fn tree_equality_drift(
    repo_root: &Path,
    scan_root: &Path,
    tree: &ObjectId,
    excluded_paths: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let store = NativeObjectStore::new(repo_root);
    let expected: BTreeMap<String, (String, u32)> = store
        .tree_fingerprints(tree)?
        .into_iter()
        .filter(|(path, _)| !is_ignored_by_policy(path) && !excluded_paths.contains(path))
        .collect();
    let mut actual = BTreeSet::new();
    if scan_root.is_dir() {
        for path in walk_worktree(scan_root)? {
            if is_ignored_by_policy(&path) || excluded_paths.contains(&path) {
                continue;
            }
            actual.insert(path);
        }
    }
    let mut drifted = BTreeSet::new();
    for path in &actual {
        if !expected.contains_key(path) {
            drifted.insert(path.clone());
        }
    }
    for (path, (object, mode)) in &expected {
        // Short-circuit: a recorded path absent from the walk is a deletion — no
        // per-file compare needed.
        let matches =
            actual.contains(path) && file_matches_blob(&scan_root.join(path), object, *mode)?;
        if !matches {
            drifted.insert(path.clone());
        }
    }
    Ok(drifted.into_iter().collect())
}

/// Per-file equality against one recorded tree entry. A symlink entry (mode
/// `120000`) compares the link target bytes (never following the link); a
/// regular-file entry hashes the file bytes into a blob [`ObjectId`] and
/// compares ids. A path that vanished since the walk, or is of the wrong type,
/// simply differs (`Ok(false)`) — benign under a concurrent agent fleet.
fn file_matches_blob(full: &Path, object: &str, mode: u32) -> Result<bool> {
    let metadata = match fs::symlink_metadata(full) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(false),
    };
    let bytes = if mode == SYMLINK_MODE {
        if !metadata.file_type().is_symlink() {
            return Ok(false);
        }
        fs::read_link(full)
            .map_err(|error| anyhow!("read workspace symlink: {}", error.kind()))?
            .to_string_lossy()
            .into_owned()
            .into_bytes()
    } else {
        if !metadata.is_file() {
            return Ok(false); // dir or symlink where a regular file was recorded
        }
        fs::read(full).map_err(|error| anyhow!("read workspace file: {}", error.kind()))?
    };
    Ok(ObjectId::new(ObjectKind::Blob, &bytes).to_string() == object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    /// Build a native store under `repo_root/.forge` holding a tree with the given
    /// `path -> bytes` files, returning the tree id. Materializes the files into a
    /// scratch dir and snapshots it through the production scanner
    /// (`snapshot_worktree_into_store`) so the recorded tree matches real trees.
    fn store_tree(repo_root: &Path, files: &BTreeMap<&str, &[u8]>) -> ObjectId {
        let scratch = TempDir::new().unwrap();
        for (path, bytes) in files {
            let full = scratch.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, bytes).unwrap();
        }
        let content = crate::snapshot_worktree_into_store(repo_root, scratch.path()).unwrap();
        let tree = content
            .content_ref
            .strip_prefix(forge_content::FORGE_TREE_PREFIX)
            .unwrap()
            .to_string();
        ObjectId::parse(&tree).unwrap()
    }

    #[test]
    fn equal_dir_reports_no_drift_and_gitignored_strays_are_not_drift() {
        let repo = TempDir::new().unwrap();
        let files: BTreeMap<&str, &[u8]> = BTreeMap::from([
            ("README.md", b"hello\n" as &[u8]),
            (".gitignore", b"target/\n"),
        ]);
        let tree = store_tree(repo.path(), &files);

        let scan = TempDir::new().unwrap();
        fs::write(scan.path().join("README.md"), b"hello\n").unwrap();
        fs::write(scan.path().join(".gitignore"), b"target/\n").unwrap();
        // A gitignored build artifact inside the scanned dir must not drift: the
        // ignore semantics are rooted at scan_root, mirroring the scanner that
        // built the recorded tree (NER-382 fix 1).
        fs::create_dir_all(scan.path().join("target")).unwrap();
        fs::write(scan.path().join("target/artifact.o"), b"obj").unwrap();

        let drifted =
            tree_equality_drift(repo.path(), scan.path(), &tree, &BTreeSet::new()).unwrap();
        assert!(drifted.is_empty(), "unexpected drift: {drifted:?}");

        // A NON-ignored stray file still drifts.
        fs::write(scan.path().join("stray.txt"), b"stray").unwrap();
        let drifted =
            tree_equality_drift(repo.path(), scan.path(), &tree, &BTreeSet::new()).unwrap();
        assert_eq!(drifted, vec!["stray.txt".to_string()]);
    }

    #[test]
    fn modified_deleted_and_excluded_paths() {
        let repo = TempDir::new().unwrap();
        let files: BTreeMap<&str, &[u8]> = BTreeMap::from([
            ("README.md", b"hello\n" as &[u8]),
            ("src/lib.rs", b"pub fn f() {}\n"),
        ]);
        let tree = store_tree(repo.path(), &files);

        let scan = TempDir::new().unwrap();
        fs::write(scan.path().join("README.md"), b"changed\n").unwrap();
        // src/lib.rs deleted; NOTES.md added but excluded (e.g. a private path label).
        fs::write(scan.path().join("NOTES.md"), b"private\n").unwrap();

        let excluded = BTreeSet::from(["NOTES.md".to_string()]);
        let drifted = tree_equality_drift(repo.path(), scan.path(), &tree, &excluded).unwrap();
        assert_eq!(
            drifted,
            vec!["README.md".to_string(), "src/lib.rs".to_string()],
            "modified + deleted drift, sorted; the excluded added path does not"
        );

        // Excluding a recorded path drops it from the expected side too.
        let excluded = BTreeSet::from(["NOTES.md".to_string(), "src/lib.rs".to_string()]);
        let drifted = tree_equality_drift(repo.path(), scan.path(), &tree, &excluded).unwrap();
        assert_eq!(drifted, vec!["README.md".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn chmod_only_change_is_not_drift_but_type_change_is() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempDir::new().unwrap();
        let files: BTreeMap<&str, &[u8]> =
            BTreeMap::from([("run.sh", b"#!/bin/sh\n" as &[u8]), ("data.txt", b"d\n")]);
        let tree = store_tree(repo.path(), &files);

        let scan = TempDir::new().unwrap();
        fs::write(scan.path().join("run.sh"), b"#!/bin/sh\n").unwrap();
        fs::write(scan.path().join("data.txt"), b"d\n").unwrap();
        fs::set_permissions(
            scan.path().join("run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let drifted =
            tree_equality_drift(repo.path(), scan.path(), &tree, &BTreeSet::new()).unwrap();
        assert!(
            drifted.is_empty(),
            "bytes-only semantics: chmod must not drift: {drifted:?}"
        );

        // Replacing a recorded regular file with a symlink (even to identical
        // bytes) IS drift: the recorded entry pins the regular-file type.
        fs::remove_file(scan.path().join("data.txt")).unwrap();
        std::os::unix::fs::symlink("run.sh", scan.path().join("data.txt")).unwrap();
        let drifted =
            tree_equality_drift(repo.path(), scan.path(), &tree, &BTreeSet::new()).unwrap();
        assert_eq!(drifted, vec!["data.txt".to_string()]);
    }
}
