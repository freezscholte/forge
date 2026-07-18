//! Read a single blob's bytes at a path within a native content tree.
//!
//! Split out of `lib.rs` (ADR-0001 domain-module rule / the file's line-count
//! ceiling). The U6 blast secret-content scan uses [`read_blob_at_path`] to obtain a
//! MODIFIED file's pre-agent baseline content so only agent-ADDED lines are scanned
//! (diff-aware R16): scanning the whole post-state false-positived when a modified
//! file already carried a secret-shaped string signed into the base tree.

use anyhow::{anyhow, Result};
use forge_content::FORGE_TREE_PREFIX;

use crate::{NativeObjectStore, ObjectId, ObjectKind, TreeEntryKind, TreeObject, SYMLINK_MODE};

/// Parse a `forge-tree:<id>` content ref into its root [`ObjectId`]. Shared by the
/// diff/merge/materialize seams in `lib.rs` and by [`read_blob_at_path`].
pub(crate) fn object_id_from_content_ref(content_ref: &str) -> Result<ObjectId> {
    ObjectId::parse(
        content_ref
            .strip_prefix(FORGE_TREE_PREFIX)
            .ok_or_else(|| anyhow!("unsupported content ref"))?,
    )
}

/// Read the raw bytes of the blob at `path` within the tree named by `content_ref`,
/// returning `None` when `path` does not resolve to a regular-file leaf in that tree.
///
/// Used by the U6 blast secret-content scan to obtain a MODIFIED file's pre-agent
/// baseline content so only agent-ADDED lines are scanned (diff-aware R16). Without
/// this, the scan reads the whole post-state, so modifying a file that already
/// carries secret-shaped fixture strings signed into the base tree would false-
/// positive even though the agent added no secret.
///
/// `path` uses `/` separators. A missing component, a path that descends through a
/// file, a directory leaf, or a symlink leaf all yield `None` (none is a scannable
/// regular-file blob). Policy-excluded paths are NOT filtered here — the caller only
/// ever asks for paths already present in the (excluded-aware) diff.
pub fn read_blob_at_path(
    store: &NativeObjectStore,
    content_ref: &str,
    path: &str,
) -> Result<Option<Vec<u8>>> {
    let mut current = object_id_from_content_ref(content_ref)?;
    let mut components = path.split('/').filter(|c| !c.is_empty()).peekable();
    while let Some(name) = components.next() {
        if current.kind()? != ObjectKind::Tree {
            return Ok(None);
        }
        let payload = store.read_object(&current)?;
        let tree: TreeObject = serde_json::from_slice(&payload)?;
        let Some(entry) = tree.entries.into_iter().find(|e| e.name == name) else {
            return Ok(None);
        };
        let child = ObjectId::parse(&entry.object)?;
        let is_last = components.peek().is_none();
        match entry.kind {
            TreeEntryKind::File => {
                // A file component that is not the last path component (descending
                // through a file), a symlink leaf, or a non-blob object is not a
                // scannable regular-file blob.
                if !is_last || entry.mode == SYMLINK_MODE || child.kind()? != ObjectKind::Blob {
                    return Ok(None);
                }
                return Ok(Some(store.read_object(&child)?));
            }
            TreeEntryKind::Dir => {
                if is_last {
                    return Ok(None); // directory leaf, not a blob
                }
                current = child;
            }
        }
    }
    Ok(None)
}
