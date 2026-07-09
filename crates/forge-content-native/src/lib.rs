//! Native content object store, tree materialization, diff, and merge engine.
//!
//! ADR-0001's 3,000-line ceiling allows exceptions when cohesion beats size: object framing,
//! tree walking/materialization, diff fingerprinting, and three-way merge share private
//! invariants that mechanical splitting would widen. New domains land in sibling modules.

use anyhow::{anyhow, bail, Context, Result};
use forge_content::{
    is_ignored_by_policy, ContentBackend, DiffLine, DiffLineTag, DiffWarning, FileDiff, HunkDiff,
    SnapshotContent, TreeDiff, FORGE_TREE_PREFIX,
};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
// `Command` is used only by the `#[cfg(test)]` differential harness (slice-1 parity proofs); native base/changed-paths no longer shell git (NER-138 Phase 7 slice 2).
#[cfg(test)]
use std::process::Command;

mod pack;
pub mod provenance;
mod status_cache;
mod workspace_equality;
pub use workspace_equality::tree_equality_drift;

pub use provenance::{attribute_lines, path_provenance, LineAttribution, PathProvenanceEntry};

const SCHEMA_VERSION: u32 = 1;
const HUNK_LIMIT: usize = 4096;
const BINARY_SCAN_LIMIT: usize = 8000;
const DIFF_CONTEXT_LINES: usize = 3;
const DEFAULT_RENAME_THRESHOLD: u8 = 50;
const DEFAULT_RENAME_LIMIT: usize = 1000;
const LARGE_BLOB_STREAM_THRESHOLD_BYTES: u64 = 1024 * 1024;

/// The on-object `CommitObject::schema_version` value to stamp when building a commit in
/// another crate (slice 3's `forge_store::decide`). Exposed so callers need not hard-code
/// the version; it stays 1 (genesis-hash stability — see `CommitObject`).
pub const COMMIT_SCHEMA_VERSION: u32 = SCHEMA_VERSION;

/// Re-exported from `forge_content` so `forge_store::doctor` keeps referencing
/// `forge_content_native::RESTORE_TEMP_PREFIX`, while the canonical definition and its
/// matching `is_restore_temp_path` exclusion predicate live in the shared base crate both backends depend on (NER-132 U4).
pub use forge_content::RESTORE_TEMP_PREFIX;

#[derive(Debug, Clone)]
pub struct DiffOptions {
    pub include_hunks: bool,
    pub detect_renames: bool,
    pub rename_threshold: u8,
    pub rename_limit: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            include_hunks: true,
            detect_renames: true,
            rename_threshold: DEFAULT_RENAME_THRESHOLD,
            rename_limit: DEFAULT_RENAME_LIMIT,
        }
    }
}

pub fn diff_native_trees(
    store: &NativeObjectStore,
    root_a: &ObjectId,
    root_b: &ObjectId,
    options: &DiffOptions,
) -> Result<TreeDiff> {
    diff_fingerprint_maps(
        store,
        store.tree_fingerprints(root_a)?,
        store.tree_fingerprints(root_b)?,
        &BTreeMap::new(),
        options,
    )
}

pub fn diff_native_content_refs(
    store: &NativeObjectStore,
    content_ref_a: &str,
    content_ref_b: &str,
    options: &DiffOptions,
) -> Result<TreeDiff> {
    let root_a = object_id_from_content_ref(content_ref_a)?;
    let root_b = object_id_from_content_ref(content_ref_b)?;
    diff_native_trees(store, &root_a, &root_b, options)
}

pub fn diff_working_vs_tree(
    store: &NativeObjectStore,
    repo_root: &Path,
    tree_ref: &str,
    options: &DiffOptions,
) -> Result<TreeDiff> {
    let root = object_id_from_content_ref(tree_ref)?;
    let old = store.tree_fingerprints(&root)?;
    let (worktree, mut overlay) = status_cache::working_fingerprints(repo_root)?;
    hydrate_working_overlay_for_diff(repo_root, &old, &worktree, &mut overlay)?;
    diff_fingerprint_maps(store, old, worktree, &overlay, options)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeMergeResult {
    pub merged_content_ref: Option<String>,
    pub conflicts: Vec<NativeMergeConflict>,
    pub dropped_secret_paths: Vec<String>,
}

impl NativeMergeResult {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeMergeConflict {
    pub path: String,
    pub kind: NativeMergeConflictKind,
    pub base_ref: Option<String>,
    pub ours_ref: Option<String>,
    pub theirs_ref: Option<String>,
    pub base_status: Option<String>,
    pub ours_status: Option<String>,
    pub theirs_status: Option<String>,
    pub base_mode: Option<u32>,
    pub ours_mode: Option<u32>,
    pub theirs_mode: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeMergeConflictKind {
    Content,
    Binary,
    DeleteModify,
    Rename,
    DirFile,
    Mode,
    Symlink,
}

pub fn merge_native_content_refs(
    store: &NativeObjectStore,
    base_ref: &str,
    ours_ref: &str,
    theirs_ref: &str,
) -> Result<NativeMergeResult> {
    let base = object_id_from_content_ref(base_ref)?;
    let ours = object_id_from_content_ref(ours_ref)?;
    let theirs = object_id_from_content_ref(theirs_ref)?;
    merge_native_trees(store, &base, &ours, &theirs)
}

pub fn merge_native_trees(
    store: &NativeObjectStore,
    base: &ObjectId,
    ours: &ObjectId,
    theirs: &ObjectId,
) -> Result<NativeMergeResult> {
    let base_map = store.tree_fingerprints(base)?;
    let ours_map = store.tree_fingerprints(ours)?;
    let theirs_map = store.tree_fingerprints(theirs)?;
    merge_fingerprint_maps(store, &base_map, &ours_map, &theirs_map)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectKind {
    Blob,
    Tree,
    /// An intent-aware commit/Change node (NER-138 Phase 7 slice 2). The handoff
    /// names this "Commit/Change"; it is implemented as a single kind — there is no
    /// separate `Change` kind. Domain-separated from `Blob`/`Tree` via the `as_str`
    /// tag below so the same payload hashes to a distinct id per kind.
    Commit,
}

impl ObjectKind {
    fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Blob => "blob",
            ObjectKind::Tree => "tree",
            ObjectKind::Commit => "commit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    kind: String,
    digest: String,
}

/// The domain-separated, length-prefixed preimage an object id hashes over:
/// `b"forge-object\n" + kind + "\n" + SCHEMA_VERSION + "\n" + payload.len() + "\n" + payload`.
///
/// NER-138 Phase 7 slice 3 stores this preimage *as the object file* (rather than the raw
/// payload), so the file is self-verifying (`hash(file) == id`) and self-describing (the
/// kind is a parsed header field) — letting `all_object_ids` read each object's kind instead
/// of re-hashing under every kind. `ObjectId::new` hashes exactly these bytes, so the id is
/// unchanged from slice 1/2 (no re-addressing). Single source of truth for the framing so
/// the write/read/hash paths can never disagree.
const OBJECT_MAGIC: &[u8] = b"forge-object\n";

fn object_preimage_header(kind: ObjectKind, payload_len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(OBJECT_MAGIC.len() + 24);
    buf.extend_from_slice(OBJECT_MAGIC);
    buf.extend_from_slice(kind.as_str().as_bytes());
    buf.push(b'\n');
    buf.extend_from_slice(SCHEMA_VERSION.to_string().as_bytes());
    buf.push(b'\n');
    buf.extend_from_slice(payload_len.to_string().as_bytes());
    buf.push(b'\n');
    buf
}

fn object_preimage(kind: ObjectKind, payload: &[u8]) -> Vec<u8> {
    let mut buf = object_preimage_header(kind, payload.len());
    buf.reserve(payload.len());
    buf.extend_from_slice(payload);
    buf
}

/// Parse a stored object file as a slice-3 self-describing preimage, returning its kind and
/// payload slice. Returns `None` when the bytes are not a well-formed preimage (a slice-1/2
/// legacy raw-payload object, or — astronomically — a blob whose raw content coincidentally
/// starts with the magic but does not parse). Callers MUST still verify `hash(file) == id`
/// before trusting the parsed kind, so the headered-vs-legacy decision is hash-resolved, not
/// format-guessed (a legacy blob starting with the magic fails that check and falls back).
fn parse_object_preimage(bytes: &[u8]) -> Option<(ObjectKind, &[u8])> {
    let rest = bytes.strip_prefix(OBJECT_MAGIC)?;
    let nl1 = rest.iter().position(|&b| b == b'\n')?;
    let kind = match std::str::from_utf8(&rest[..nl1]).ok()? {
        "blob" => ObjectKind::Blob,
        "tree" => ObjectKind::Tree,
        "commit" => ObjectKind::Commit,
        _ => return None,
    };
    let rest = &rest[nl1 + 1..];
    let nl2 = rest.iter().position(|&b| b == b'\n')?;
    let rest = &rest[nl2 + 1..]; // skip the schema_version line
    let nl3 = rest.iter().position(|&b| b == b'\n')?;
    let len: usize = std::str::from_utf8(&rest[..nl3]).ok()?.parse().ok()?;
    let payload = &rest[nl3 + 1..];
    if payload.len() != len {
        return None;
    }
    Some((kind, payload))
}

impl ObjectId {
    pub fn new(kind: ObjectKind, payload: &[u8]) -> Self {
        Self {
            kind: kind.as_str().to_string(),
            digest: hex_lower(&Sha256::digest(object_preimage(kind, payload))),
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        let parts: Vec<&str> = value.split(':').collect();
        if parts.len() != 4 || parts[0] != "f1" || parts[2] != "sha256" {
            bail!("malformed native object id");
        }
        match parts[1] {
            "blob" | "tree" | "commit" => {}
            _ => bail!("unsupported native object type"),
        }
        if parts[3].len() != 64 || !parts[3].bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("malformed native object digest");
        }
        Ok(Self {
            kind: parts[1].to_string(),
            digest: parts[3].to_ascii_lowercase(),
        })
    }

    pub fn kind(&self) -> Result<ObjectKind> {
        match self.kind.as_str() {
            "blob" => Ok(ObjectKind::Blob),
            "tree" => Ok(ObjectKind::Tree),
            "commit" => Ok(ObjectKind::Commit),
            _ => bail!("unsupported native object type"),
        }
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "f1:{}:sha256:{}", self.kind, self.digest)
    }
}

#[derive(Debug, Clone)]
pub struct NativeContentBackend;

impl ContentBackend for NativeContentBackend {
    fn snapshot_worktree(&self, repo_root: &Path) -> Result<SnapshotContent> {
        snapshot_worktree_into_store(repo_root, repo_root)
    }

    fn restore_snapshot(&self, repo_root: &Path, content_ref: &str) -> Result<()> {
        restore_content_ref_to_worktree(repo_root, repo_root, content_ref)
    }

    // NER-138 Phase 7 slice 2: native base anchoring. `current_base` returns the native
    // history tip — the ref store's `HEAD` — lazily creating a genesis root commit over
    // the start-time worktree on first call (see `ensure_head`). The git delegation (and
    // the Phase-3 "do NOT emit forge-tree: base refs yet" guard) are gone: a native repo
    // now anchors on its own `f1:commit:` id, not a git commit, stable across worktree
    // edits. S1: returns an opaque revision id; no filesystem paths in error context.
    fn current_base(&self, repo_root: &Path) -> Result<String> {
        Ok(ensure_head(repo_root)?.to_string())
    }

    // NER-138 Phase 7 slice 2: resolve a native base commit to the restorable content ref
    // that materializes its tree (for `attempt attach`). S2: the tree was built by the
    // policy-filtered walker, so it already excludes `is_ignored_by_policy` paths
    // (.env/keys never materialized). S1: a parse/read failure surfaces a path-free error
    // (`read_commit` carries only the opaque object id).
    fn base_content_ref(&self, repo_root: &Path, base: &str) -> Result<String> {
        let store = NativeObjectStore::new(repo_root);
        let commit = store.read_commit(&ObjectId::parse(base)?)?;
        Ok(format!("{FORGE_TREE_PREFIX}{}", commit.tree))
    }
}

pub fn snapshot_worktree_into_store(
    repo_root: &Path,
    worktree_root: &Path,
) -> Result<SnapshotContent> {
    snapshot_worktree_into_store_excluding(repo_root, worktree_root, &[])
}

pub fn snapshot_worktree_into_store_excluding(
    repo_root: &Path,
    worktree_root: &Path,
    excluded_paths: &[String],
) -> Result<SnapshotContent> {
    let store = NativeObjectStore::new(repo_root);
    let excluded: BTreeSet<&str> = excluded_paths.iter().map(String::as_str).collect();
    let files: Vec<FileEntry> = scan_worktree(worktree_root)?
        .into_iter()
        .filter(|entry| !excluded.contains(entry.path.as_str()))
        .collect();
    let root = write_tree(&store, worktree_root, &files, "")?;
    // NER-138 Phase 7 slice 2: changed_paths is a native name-level diff of the
    // owner repo's base HEAD tree against the freshly-built effective worktree tree.
    let changed = changed_paths_excluding(&store, repo_root, &root, &excluded)?;
    Ok(SnapshotContent {
        content_ref: format!("{FORGE_TREE_PREFIX}{root}"),
        changed_paths: changed,
    })
}

pub fn restore_content_ref_to_worktree(
    repo_root: &Path,
    worktree_root: &Path,
    content_ref: &str,
) -> Result<()> {
    let root = object_id_from_content_ref(content_ref)?;
    let store = NativeObjectStore::new(repo_root);
    store.verify_content_ref(content_ref)?;
    let mut target_paths = BTreeSet::new();
    let mut synced_dirs = BTreeSet::new();
    materialize_tree(
        &store,
        worktree_root,
        &root,
        "",
        &mut target_paths,
        &mut synced_dirs,
    )?;
    for path in materialized_paths(worktree_root)? {
        if !target_paths.contains(&path) {
            let full = worktree_root.join(&path);
            if full.is_file() || full.is_symlink() {
                fs::remove_file(&full)
                    .map_err(|error| anyhow!("remove worktree entry: {}", error.kind()))?;
            }
        }
    }
    Ok(())
}

/// Ensure the native ref store has a `HEAD` and return it (NER-138 Phase 7 slice 2).
///
/// If a tip already exists, return it. Otherwise create the **genesis root commit** over
/// the current worktree tree (parentless, null justification), persist it, point `HEAD` at
/// it, and return its id. This is an intentional "ensure the base anchor exists" side
/// effect, not a pure read: in the normal lifecycle the first `current_base` caller is
/// `start`, so the genesis captures the *start-time* worktree as the base — never a
/// mid-`save` dirty tree, because `save` requires an active attempt that `start` created.
/// Every `current_base` caller is a mutating command holding the advisory lock
/// (acquire-once); `ensure_head` itself never acquires the lock, so genesis creation
/// cannot deadlock or race. Idempotent: the commit is content-addressed and `set_head` is
/// an atomic overwrite, so a repeated call returns the same id.
fn ensure_head(repo_root: &Path) -> Result<ObjectId> {
    let refs = NativeRefStore::new(repo_root);
    if let Some(head) = refs.read_head()? {
        return Ok(head);
    }
    let store = NativeObjectStore::new(repo_root);
    let files = scan_worktree(repo_root)?;
    let tree = write_tree(&store, repo_root, &files, "")?;
    let genesis = CommitObject {
        schema_version: SCHEMA_VERSION,
        tree: tree.to_string(),
        parents: Vec::new(),
        intent_id: None,
        proposal_revision_id: None,
        decision_id: None,
        evidence_digest: None,
        // Genesis has no decider; `actor`/`authored_time` stay `None` and (via
        // skip_serializing_if) are omitted, so the genesis hash is byte-identical to slice 2.
        actor: None,
        authored_time: None,
    };
    // Store-before-DB: the genesis object + HEAD are durable before `current_base`
    // returns, so the `attempts.base_head` row that records this id is written only after
    // its referent is on disk.
    let id = store.write_commit(&genesis)?;
    refs.set_head(&id)?;
    Ok(id)
}

pub fn materialize_content_ref(
    repo_root: &Path,
    destination: &Path,
    content_ref: &str,
) -> Result<()> {
    let root = object_id_from_content_ref(content_ref)?;
    let store = NativeObjectStore::new(repo_root);
    let mut target_paths = BTreeSet::new();
    let mut synced_dirs = BTreeSet::new();
    materialize_tree(
        &store,
        destination,
        &root,
        "",
        &mut target_paths,
        &mut synced_dirs,
    )
}

#[derive(Debug, Clone)]
pub struct NativeObjectStore {
    root: PathBuf,
}

impl NativeObjectStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            root: repo_root.to_path_buf(),
        }
    }

    pub fn write_object(&self, kind: ObjectKind, payload: &[u8]) -> Result<ObjectId> {
        let id = ObjectId::new(kind, payload);
        let path = self.object_path(&id);
        if path.exists() {
            // Dedup: the object is already on disk. `read_object` re-verifies it (headered
            // OR legacy raw-payload — both valid for this id), so a slice-3 write over an
            // existing slice-1/2 legacy file is a verified no-op, never a "hash mismatch".
            self.read_object(&id)?;
            return Ok(id);
        }

        // Slice 3: store the self-describing domain-separated preimage (kind in a header),
        // not the raw payload. `hash(file) == id`, so the file is self-verifying and
        // `all_object_ids` reads its kind instead of re-hashing under every kind.
        let framed = object_preimage(kind, payload);
        let parent = path.parent().context("object path has no parent")?;
        // Record which ancestor directories do not yet exist so their creation can be
        // made durable after the object is written: a freshly created shard directory's
        // own entry is not durable until the directory it lives in is fsynced.
        let newly_created = missing_dirs(parent);
        fs::create_dir_all(parent)?;
        fs::create_dir_all(self.tmp_dir())?;
        let mut temp = tempfile::NamedTempFile::new_in(self.tmp_dir())?;
        temp.write_all(&framed)?;
        temp.as_file_mut().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        // The object's directory entry must reach disk before any DB row references it.
        // A swallowed failure here is exactly the durability hole this fix closes.
        sync_dir(parent)?;
        // Make each newly created ancestor directory durable by fsyncing the directory
        // that gained the new entry.
        for dir in &newly_created {
            if let Some(grandparent) = dir.parent() {
                sync_dir(grandparent)?;
            }
        }
        Ok(id)
    }

    pub fn write_blob_from_path(&self, path: &Path) -> Result<ObjectId> {
        let metadata = fs::metadata(path)?;
        if metadata.len() < LARGE_BLOB_STREAM_THRESHOLD_BYTES {
            return self.write_object(ObjectKind::Blob, &fs::read(path)?);
        }

        let payload_len = usize::try_from(metadata.len())
            .map_err(|_| anyhow!("native blob is too large for this platform"))?;
        let header = object_preimage_header(ObjectKind::Blob, payload_len);
        fs::create_dir_all(self.tmp_dir())?;
        let mut source = BufReader::new(File::open(path)?);
        let mut temp = tempfile::NamedTempFile::new_in(self.tmp_dir())?;
        let mut hasher = Sha256::new();
        hasher.update(&header);
        let copied = {
            let mut writer = BufWriter::new(temp.as_file_mut());
            writer.write_all(&header)?;
            let copied = copy_hashing(&mut source, &mut writer, &mut hasher)?;
            writer.flush()?;
            copied
        };
        if copied != metadata.len() {
            bail!("native blob changed while snapshotting");
        }
        temp.as_file_mut().sync_all()?;

        let id = ObjectId {
            kind: ObjectKind::Blob.as_str().to_string(),
            digest: hex_lower(&hasher.finalize()),
        };
        let target = self.object_path(&id);
        if target.exists() {
            self.read_object(&id)?;
            return Ok(id);
        }

        let parent = target.parent().context("object path has no parent")?;
        let newly_created = missing_dirs(parent);
        fs::create_dir_all(parent)?;
        temp.persist(&target).map_err(|error| error.error)?;
        sync_dir(parent)?;
        for dir in &newly_created {
            if let Some(grandparent) = dir.parent() {
                sync_dir(grandparent)?;
            }
        }
        Ok(id)
    }

    pub fn read_object(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let path = self.object_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return pack::read_packed_object(&self.root, id);
            }
            Err(error) => return Err(error).with_context(|| format!("read native object {}", id)),
        };
        parse_stored_object_bytes(id, &bytes)
    }

    pub fn write_object_payload_to<W: Write>(&self, id: &ObjectId, writer: &mut W) -> Result<()> {
        let path = self.object_path(id);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.len() >= LARGE_BLOB_STREAM_THRESHOLD_BYTES => {
                return stream_loose_headered_payload_to_writer(id, &path, writer);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("read native object {}", id)),
        }
        writer.write_all(&self.read_object(id)?)?;
        Ok(())
    }
}

fn copy_hashing<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    hasher: &mut Sha256,
) -> Result<u64> {
    let mut copied = 0;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
    }
    Ok(copied)
}

fn read_header_line<R: BufRead>(
    reader: &mut R,
    hasher: &mut Sha256,
    label: &str,
) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 || line.last() != Some(&b'\n') {
        bail!("malformed native object header: missing {label}");
    }
    hasher.update(&line);
    line.pop();
    Ok(line)
}

fn stream_loose_headered_payload_to_writer<W: Write>(
    id: &ObjectId,
    path: &Path,
    writer: &mut W,
) -> Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut magic = vec![0_u8; OBJECT_MAGIC.len()];
    reader.read_exact(&mut magic)?;
    if magic != OBJECT_MAGIC {
        return stream_legacy_payload_to_writer(id, path, writer);
    }
    hasher.update(&magic);

    let kind = read_header_line(&mut reader, &mut hasher, "kind")?;
    if kind.as_slice() != id.kind.as_bytes() {
        bail!("native object kind header mismatch for {}", id);
    }
    let schema = read_header_line(&mut reader, &mut hasher, "schema")?;
    if schema.as_slice() != SCHEMA_VERSION.to_string().as_bytes() {
        bail!("unsupported native object schema version");
    }
    let payload_len_line = read_header_line(&mut reader, &mut hasher, "payload length")?;
    let payload_len: u64 = std::str::from_utf8(&payload_len_line)?
        .parse()
        .context("malformed native object payload length")?;

    let mut remaining = payload_len;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let read = reader.read(&mut buffer[..limit])?;
        if read == 0 {
            bail!("truncated native content object {}", id);
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }

    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        bail!("native content object has trailing bytes {}", id);
    }
    if hex_lower(&hasher.finalize()) != id.digest {
        bail!("hash mismatch for native content object {}", id);
    }
    Ok(())
}

fn stream_legacy_payload_to_writer<W: Write>(
    id: &ObjectId,
    path: &Path,
    writer: &mut W,
) -> Result<()> {
    let metadata = fs::metadata(path)?;
    let payload_len = usize::try_from(metadata.len())
        .map_err(|_| anyhow!("native blob is too large for this platform"))?;
    let header = object_preimage_header(id.kind()?, payload_len);
    let mut hasher = Sha256::new();
    hasher.update(&header);
    let mut reader = BufReader::new(File::open(path)?);
    let copied = copy_hashing(&mut reader, writer, &mut hasher)?;
    if copied != metadata.len() {
        bail!("native object changed while restoring");
    }
    if hex_lower(&hasher.finalize()) != id.digest {
        bail!("hash mismatch for native content object {}", id);
    }
    Ok(())
}

fn parse_stored_object_bytes(id: &ObjectId, bytes: &[u8]) -> Result<Vec<u8>> {
    // Slice-3 headered object: the file IS the preimage, so `hash(file) == id` and its
    // kind is a parsed header field. The hash check disambiguates from a legacy blob
    // that merely starts with the magic (its hash will not match) — hash-resolved, not
    // format-guessed. A header kind that disagrees with the id's kind is corruption.
    if let Some((kind, payload)) = parse_object_preimage(bytes) {
        if hex_lower(&Sha256::digest(bytes)) == id.digest {
            if kind != id.kind()? {
                bail!("native object kind header mismatch for {}", id);
            }
            return Ok(payload.to_vec());
        }
    }
    // Legacy slice-1/2 raw-payload fallback: the id hashes the framed preimage of the
    // raw bytes. Kept alive (prove-before-delete) so existing repos stay readable.
    let actual = ObjectId::new(id.kind()?, bytes);
    if &actual != id {
        bail!("hash mismatch for native content object {}", id);
    }
    Ok(bytes.to_vec())
}

fn parse_headered_object_frame(id: &ObjectId, bytes: &[u8]) -> Result<Vec<u8>> {
    let Some((kind, payload)) = parse_object_preimage(bytes) else {
        bail!("malformed packed native object {}", id);
    };
    if hex_lower(&Sha256::digest(bytes)) != id.digest {
        bail!("hash mismatch for packed native object {}", id);
    }
    if kind != id.kind()? {
        bail!("packed native object kind header mismatch for {}", id);
    }
    Ok(payload.to_vec())
}

impl NativeObjectStore {
    pub(crate) fn tree_fingerprints(
        &self,
        root: &ObjectId,
    ) -> Result<BTreeMap<String, FileFingerprint>> {
        flatten_tree(self, root)
    }

    /// Write a native commit object (NER-138 Phase 7 slice 2), returning its
    /// content-addressed id. Inherits `write_object`'s store-before-DB durability
    /// (temp + fsync + atomic rename + parent-dir fsync) verbatim.
    pub fn write_commit(&self, commit: &CommitObject) -> Result<ObjectId> {
        let payload = serde_json::to_vec(commit)?;
        self.write_object(ObjectKind::Commit, &payload)
    }

    /// Read and validate a native commit object. S1: never interpolates a filesystem
    /// path — `read_object`'s context carries only the opaque object id.
    pub fn read_commit(&self, id: &ObjectId) -> Result<CommitObject> {
        if id.kind()? != ObjectKind::Commit {
            bail!("native object is not a commit");
        }
        let payload = self.read_object(id)?;
        let commit: CommitObject = serde_json::from_slice(&payload)
            .with_context(|| format!("malformed native commit object {}", id))?;
        if commit.schema_version != SCHEMA_VERSION {
            bail!("unsupported native commit schema version");
        }
        Ok(commit)
    }

    pub fn verify_content_ref(&self, content_ref: &str) -> Result<BTreeSet<ObjectId>> {
        let root = object_id_from_content_ref(content_ref)?;
        let mut seen = BTreeSet::new();
        self.verify_reachable(&root, &mut seen)?;
        Ok(seen)
    }

    /// All native objects reachable from the ref-store `HEAD` (NER-138 Phase 7 slice 2):
    /// every commit on HEAD's ancestry (commit → parents), each commit's tree, and every
    /// blob/subtree those trees reach. Returns an empty set when no `HEAD` exists yet (a
    /// git-backend repo, or a native repo before its first base anchoring). Used by gc
    /// reachability so the live history tip and the base anchor that every attempt's
    /// `base_head` points at are never reported as unreachable garbage. A `seen` set guards
    /// against revisiting a commit (diamond/merge ancestry).
    pub fn reachable_from_head(&self) -> Result<BTreeSet<ObjectId>> {
        match NativeRefStore::new(&self.root).read_head()? {
            Some(head) => self.reachable_from(&head),
            None => Ok(BTreeSet::new()),
        }
    }

    /// All native objects reachable from a given commit (NER-138 Phase 7 slice 3): the commit,
    /// its ancestry (commit → parents), each commit's tree, and every blob/subtree those trees
    /// reach. Generalizes `reachable_from_head` so gc can seed reachability from the
    /// authoritative ledger tip (and every accepted / checkout-target commit), not only the
    /// ref-store HEAD — which a lock-free, never-reconciled gc could otherwise read stale. A
    /// `seen` set guards against revisiting a commit (diamond/merge ancestry).
    pub fn reachable_from(&self, tip: &ObjectId) -> Result<BTreeSet<ObjectId>> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![tip.clone()];
        while let Some(commit_id) = stack.pop() {
            if !reachable.insert(commit_id.clone()) {
                continue; // already visited
            }
            let commit = self.read_commit(&commit_id)?;
            // Mark the commit's tree and everything it reaches.
            self.verify_reachable(&ObjectId::parse(&commit.tree)?, &mut reachable)?;
            for parent in &commit.parents {
                stack.push(ObjectId::parse(parent)?);
            }
        }
        Ok(reachable)
    }

    pub fn all_object_ids(&self) -> Result<BTreeSet<ObjectId>> {
        let mut ids = self.loose_object_ids()?;
        ids.extend(pack::all_packed_object_ids(&self.root)?);
        Ok(ids)
    }

    pub fn loose_object_ids(&self) -> Result<BTreeSet<ObjectId>> {
        let mut ids = BTreeSet::new();
        let dir = self.root.join(".forge/objects/sha256");
        if dir.exists() {
            for prefix in fs::read_dir(dir)? {
                let prefix = prefix?;
                if !prefix.file_type()?.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(prefix.path())? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let digest = entry.file_name().to_string_lossy().into_owned();
                    let bytes = fs::read(entry.path())?;
                    // Slice-3 headered object: read its kind from the preimage header, verified
                    // by `hash(file) == digest` — a single hash, no multi-kind re-hash. This is
                    // the primary path that kills the triple-hash scan.
                    if let Some((kind, _payload)) = parse_object_preimage(&bytes) {
                        if hex_lower(&Sha256::digest(&bytes)) == digest {
                            ids.insert(ObjectId {
                                kind: kind.as_str().to_string(),
                                digest,
                            });
                            continue;
                        }
                    }
                    // Legacy slice-1/2 raw-payload fallback (prove-before-delete; kept alive so a
                    // mixed store still enumerates fully): recover the kind by re-hashing the raw
                    // bytes under every kind and matching the digest — domain separation
                    // guarantees at most one match.
                    for kind in [ObjectKind::Blob, ObjectKind::Tree, ObjectKind::Commit] {
                        let id = ObjectId::new(kind, &bytes);
                        if id.digest == digest {
                            ids.insert(id);
                        }
                    }
                }
            }
        }
        Ok(ids)
    }

    pub fn packed_object_infos(&self) -> Result<Vec<PackedObjectInfo>> {
        pack::packed_object_infos(&self.root).map(|infos| {
            infos
                .into_iter()
                .map(|info| PackedObjectInfo {
                    object_id: info.object_id,
                    pack_id: info.pack_id,
                    packed_at_ms: info.packed_at_ms,
                    loose_mtime_ms: info.loose_mtime_ms,
                })
                .collect()
        })
    }

    pub fn write_pack_from_loose_objects(
        &self,
        ids: &[ObjectId],
    ) -> Result<Option<PackWriteReport>> {
        pack::write_pack_from_loose_objects(&self.root, ids).map(|summary| {
            summary.map(|summary| PackWriteReport {
                pack_id: summary.pack_id,
                object_ids: summary.object_ids,
            })
        })
    }

    pub fn has_verified_packed_object(&self, id: &ObjectId) -> Result<bool> {
        pack::has_verified_packed_object(&self.root, id)
    }

    pub fn delete_loose_duplicate(&self, id: &ObjectId) -> Result<()> {
        if !self.has_verified_packed_object(id)? {
            bail!("cannot delete native loose object without verified packed copy");
        }
        self.delete_object(id)
    }

    pub fn delete_pack(&self, pack_id: &str) -> Result<()> {
        pack::delete_pack(&self.root, pack_id)
    }

    pub fn validate_packs(&self) -> Vec<String> {
        pack::validate_packs(&self.root)
    }

    pub fn object_modified_time(&self, id: &ObjectId) -> Result<std::time::SystemTime> {
        Ok(fs::metadata(self.object_path(id))?.modified()?)
    }

    pub fn delete_object(&self, id: &ObjectId) -> Result<()> {
        let path = self.object_path(id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("delete native object {}", id)),
        }
    }

    fn verify_reachable(&self, id: &ObjectId, seen: &mut BTreeSet<ObjectId>) -> Result<()> {
        if !seen.insert(id.clone()) {
            return Ok(());
        }
        let payload = self.read_object(id)?;
        if id.kind()? == ObjectKind::Tree {
            let tree: TreeObject = serde_json::from_slice(&payload)
                .with_context(|| format!("malformed native tree object {}", id))?;
            if tree.schema_version != SCHEMA_VERSION {
                bail!("unsupported native tree schema version");
            }
            for entry in tree.entries {
                validate_tree_entry(&entry)?;
                let child = ObjectId::parse(&entry.object)?;
                ensure_child_kind(&entry, &child)?;
                self.verify_reachable(&child, seen)?;
            }
        }
        Ok(())
    }

    fn object_path(&self, id: &ObjectId) -> PathBuf {
        self.root
            .join(".forge/objects/sha256")
            .join(&id.digest()[..2])
            .join(id.digest())
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(".forge/tmp")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedObjectInfo {
    pub object_id: ObjectId,
    pub pack_id: String,
    pub packed_at_ms: Option<u64>,
    pub loose_mtime_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackWriteReport {
    pub pack_id: String,
    pub object_ids: Vec<ObjectId>,
}

/// The native ref store (NER-138 Phase 7 slice 2): a small, crash-atomic, lock-agnostic
/// holder for the history tip (`HEAD` → a commit id). Slice 2 writes `HEAD` exactly once
/// (the genesis commit); advancing it as commits are recorded at `accept` is slice 3.
///
/// Writes inherit `NativeObjectStore::write_object`'s durability discipline verbatim
/// (temp in `.forge/tmp` + `sync_all` + atomic rename + parent-dir fsync incl.
/// newly-created ancestors; a swallowed dir fsync is the durability hole this avoids).
/// The store NEVER acquires `.forge/forge.lock` — its callers (mutating commands) already
/// hold it (acquire-once-never-nested), so creating the genesis from inside `current_base`
/// cannot deadlock or race. The ref file lives under `.forge/`, so `is_ignored_by_policy`
/// already excludes it from every snapshot/export.
#[derive(Debug, Clone)]
pub struct NativeRefStore {
    root: PathBuf,
}

impl NativeRefStore {
    pub fn new(repo_root: &Path) -> Self {
        Self {
            root: repo_root.to_path_buf(),
        }
    }

    fn head_path(&self) -> PathBuf {
        self.root.join(".forge/refs/HEAD")
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join(".forge/tmp")
    }

    /// The current history tip, or `None` if no tip has been written yet. S1: a read or
    /// parse failure surfaces a path-free error (only the `io::ErrorKind` or the malformed
    /// contents/kind, never the filesystem path).
    pub fn read_head(&self) -> Result<Option<ObjectId>> {
        let raw = match fs::read_to_string(self.head_path()) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(anyhow!("failed to read native HEAD: {}", error.kind())),
        };
        let id = ObjectId::parse(raw.trim())?;
        if id.kind()? != ObjectKind::Commit {
            bail!("native HEAD does not name a commit");
        }
        Ok(Some(id))
    }

    /// Atomically set the history tip. Crash-atomic (temp + fsync + rename + dir fsync) so
    /// a committed `base_head`/`decisions.commit_id` row never references a HEAD that did
    /// not reach disk. S1: the underlying `io::Error`s are path-free by construction.
    pub fn set_head(&self, id: &ObjectId) -> Result<()> {
        let path = self.head_path();
        let parent = path.parent().context("native HEAD path has no parent")?;
        // Record newly-created ancestors so their own dir entries can be made durable
        // below (mirrors write_object): a freshly created dir's entry is not durable
        // until the dir it lives in is fsynced.
        let newly_created = missing_dirs(parent);
        fs::create_dir_all(parent)?;
        fs::create_dir_all(self.tmp_dir())?;
        let mut temp = tempfile::NamedTempFile::new_in(self.tmp_dir())?;
        temp.write_all(id.to_string().as_bytes())?;
        temp.as_file_mut().sync_all()?;
        temp.persist(&path).map_err(|error| error.error)?;
        sync_dir(parent)?;
        for dir in &newly_created {
            if let Some(grandparent) = dir.parent() {
                sync_dir(grandparent)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreeObject {
    schema_version: u32,
    entries: Vec<TreeEntry>,
}

/// An opaque lowercase 64-hex digest (e.g. an evidence `content_hash`). Constructing one
/// validates the shape, so the commit-build path (slice 3's `accept`) can only assign a
/// real digest — excerpt text is structurally unrepresentable in
/// [`CommitObject::evidence_digest`] (the commit payload is written via `write_object` and
/// never passes through `redact_evidence_excerpt`, so this newtype is the secret-hygiene
/// guard). `#[serde(transparent)]` so it serializes/deserializes as the bare hex string —
/// byte-identical to the prior `Option<String>` field, preserving genesis-hash stability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Hex64(String);

impl Hex64 {
    /// Validate and wrap a lowercase 64-hex digest. Errors (path-free) on any other shape,
    /// so a non-digest (e.g. excerpt text) can never reach the commit payload.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            bail!("evidence digest must be exactly 64 lowercase hex characters");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Hex64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A native commit/Change object (NER-138 Phase 7 slice 2; justified commits land in
/// slice 3). Content-addressed and domain-separated via `ObjectId::new(ObjectKind::Commit,
/// ..)`. `pub` (with `pub` fields) so `forge_store` can build justified commits at `accept`
/// and walk them for `log`/checkout/`doctor` (slice 3).
///
/// **Genesis-hash stability (slice 3, critical):** the two slice-3 fields `actor` and
/// `authored_time` carry `#[serde(skip_serializing_if = "Option::is_none")]`, so a genesis
/// commit (all-`None`) serializes byte-identically to slice 2 — its `ObjectId` is unchanged
/// and existing repos' `base_head` does not desync into spurious `STALE_BASE`. Justified
/// commits (slice 3) populate `actor` + `authored_time` in the HASHED bytes so Phase 9
/// signing attests who/when (a later registry bump cannot retroactively bring earlier
/// justified commits under signed/decider-bound provenance).
///
/// `evidence_digest`, when present, is a [`Hex64`] (an opaque lowercase-hex digest such as
/// the ledger's evidence `content_hash`) — never an excerpt or any free text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitObject {
    pub schema_version: u32,
    pub tree: String,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default)]
    pub intent_id: Option<String>,
    #[serde(default)]
    pub proposal_revision_id: Option<String>,
    #[serde(default)]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub evidence_digest: Option<Hex64>,
    /// The decider (`decisions.actor`). `None` for genesis. Hashed for justified commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Wall-clock authored time (ms). `None` for genesis. Hashed for justified commits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TreeEntry {
    name: String,
    kind: TreeEntryKind,
    mode: u32,
    object: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TreeEntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    executable: bool,
    /// `Some(target)` when this entry is a symlink (NER-138 Phase 7 slice 3): the blob stores
    /// the link target bytes under mode `0o120000`, matching git's symlink representation, so
    /// a symlink round-trips as a link (not a regular file whose content is the target text).
    /// `None` for a regular file. Captured via `read_link` (never followed), so the link's
    /// pointed-at content is never read into a snapshot.
    symlink_target: Option<String>,
}

/// The tree-entry mode for a symlink: git's `120000`. A symlink leaf is a `TreeEntryKind::File`
/// whose blob is the target bytes and whose mode is this; folding mode into the diff key
/// (`FileFingerprint`) keeps a symlink distinct from a regular file with identical bytes.
const SYMLINK_MODE: u32 = 0o120000;

fn object_id_from_content_ref(content_ref: &str) -> Result<ObjectId> {
    ObjectId::parse(
        content_ref
            .strip_prefix(FORGE_TREE_PREFIX)
            .ok_or_else(|| anyhow!("unsupported content ref"))?,
    )
}

fn write_tree(
    store: &NativeObjectStore,
    repo_root: &Path,
    files: &[FileEntry],
    prefix: &str,
) -> Result<ObjectId> {
    let mut grouped: BTreeMap<String, Vec<FileEntry>> = BTreeMap::new();
    let mut direct_files = Vec::new();

    for file in files {
        let rest = if prefix.is_empty() {
            file.path.as_str()
        } else if let Some(rest) = file.path.strip_prefix(&format!("{prefix}/")) {
            rest
        } else {
            continue;
        };

        if let Some((dir, _)) = rest.split_once('/') {
            grouped
                .entry(dir.to_string())
                .or_default()
                .push(file.clone());
        } else {
            direct_files.push(file.clone());
        }
    }

    let mut entries = Vec::new();
    for file in direct_files {
        // A symlink stores its target bytes under mode 120000 (git's representation); a
        // regular file stores its content under 100644/100755.
        let (bytes, mode) = match &file.symlink_target {
            Some(target) => (target.clone().into_bytes(), SYMLINK_MODE),
            None => {
                let blob = store.write_blob_from_path(&repo_root.join(&file.path))?;
                let mode = if file.executable { 0o100755 } else { 0o100644 };
                let name = file
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.path)
                    .to_string();
                entries.push(TreeEntry {
                    name,
                    kind: TreeEntryKind::File,
                    mode,
                    object: blob.to_string(),
                });
                continue;
            }
        };
        let blob = store.write_object(ObjectKind::Blob, &bytes)?;
        let name = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_string();
        entries.push(TreeEntry {
            name,
            kind: TreeEntryKind::File,
            mode,
            object: blob.to_string(),
        });
    }

    for (dir, children) in grouped {
        let child_prefix = if prefix.is_empty() {
            dir.clone()
        } else {
            format!("{prefix}/{dir}")
        };
        let child = write_tree(store, repo_root, &children, &child_prefix)?;
        entries.push(TreeEntry {
            name: dir,
            kind: TreeEntryKind::Dir,
            mode: 0o040000,
            object: child.to_string(),
        });
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let payload = serde_json::to_vec(&TreeObject {
        schema_version: SCHEMA_VERSION,
        entries,
    })?;
    store.write_object(ObjectKind::Tree, &payload)
}

fn materialize_tree(
    store: &NativeObjectStore,
    repo_root: &Path,
    tree_id: &ObjectId,
    prefix: &str,
    target_paths: &mut BTreeSet<String>,
    synced_dirs: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if tree_id.kind()? != ObjectKind::Tree {
        bail!("native content root is not a tree");
    }
    let payload = store.read_object(tree_id)?;
    let tree: TreeObject = serde_json::from_slice(&payload)?;
    if tree.schema_version != SCHEMA_VERSION {
        bail!("unsupported native tree schema version");
    }

    for entry in tree.entries {
        validate_tree_entry(&entry)?;
        let rel = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        if is_ignored_by_policy(&rel) {
            continue;
        }
        let child = ObjectId::parse(&entry.object)?;
        ensure_child_kind(&entry, &child)?;
        match entry.kind {
            TreeEntryKind::File => {
                let full = repo_root.join(&rel);
                if entry.mode == SYMLINK_MODE {
                    let bytes = store.read_object(&child)?;
                    // A symlink entry: the blob is the link target bytes. On Unix, recreate a
                    // symlink (R15: reject an absolute / worktree-escaping target before
                    // creating it). On non-Unix, fall through and write the target bytes as a
                    // regular file (documented platform divergence).
                    #[cfg(unix)]
                    {
                        materialize_symlink(
                            repo_root,
                            &rel,
                            &full,
                            &bytes,
                            target_paths,
                            synced_dirs,
                        )?;
                        continue;
                    }
                }
                if full.is_dir() {
                    fs::remove_dir_all(&full)
                        .map_err(|error| anyhow!("remove directory: {}", error.kind()))?;
                }
                let parent = full
                    .parent()
                    .ok_or_else(|| anyhow!("restore target has no parent"))?;
                // Record which ancestor directories are newly created so their own
                // entries can be made durable below (mirrors write_object) — a freshly
                // created dir's entry is not durable until the dir it lives in is fsynced.
                let newly_created = missing_dirs(parent);
                fs::create_dir_all(parent)?;
                // Crash-atomic restore (NER-132 U4): write to a temp file in the
                // destination's own parent directory — guaranteeing a
                // same-filesystem rename even when `.forge` is a separate mount —
                // set its mode, fsync it, then atomically rename into place. The
                // `.forge-restore-` prefix lets `doctor` reclaim a temp orphaned by
                // a crash mid-restore (tempfile's Drop does not run on a hard kill).
                let mut temp = tempfile::Builder::new()
                    .prefix(RESTORE_TEMP_PREFIX)
                    .tempfile_in(parent)
                    .map_err(|error| anyhow!("create restore temp file: {}", error.kind()))?;
                {
                    let mut writer = BufWriter::new(temp.as_file_mut());
                    store.write_object_payload_to(&child, &mut writer)?;
                    writer.flush()?;
                }
                set_file_mode(temp.path(), entry.mode)?;
                temp.as_file().sync_all()?;
                temp.persist(&full)
                    .map_err(|error| anyhow!("persist restored file: {}", error.error.kind()))?;
                // The renamed file's directory entry must reach disk for the
                // restore to be durable; fsync each parent directory once per
                // restore to bound the fsync cost on large worktrees.
                if synced_dirs.insert(parent.to_path_buf()) {
                    sync_dir(parent)?;
                }
                // Make each newly created ancestor directory durable by fsyncing the
                // directory that gained the new entry (mirrors write_object), deduped
                // across the whole restore so each dir is fsynced at most once.
                for dir in &newly_created {
                    if let Some(grandparent) = dir.parent() {
                        if synced_dirs.insert(grandparent.to_path_buf()) {
                            sync_dir(grandparent)?;
                        }
                    }
                }
                target_paths.insert(rel);
                // Crash boundary (NER-132 U6, debug-only): this file is fully
                // renamed (whole, never torn) and its temp consumed; a crash here
                // leaves a partially-restored worktree with no orphaned temp.
                forge_content::maybe_crash("mid_restore");
            }
            TreeEntryKind::Dir => {
                let full = repo_root.join(&rel);
                if full.is_file() || full.is_symlink() {
                    fs::remove_file(&full)
                        .map_err(|error| anyhow!("remove file: {}", error.kind()))?;
                }
                fs::create_dir_all(full)?;
                materialize_tree(store, repo_root, &child, &rel, target_paths, synced_dirs)?;
            }
        }
    }
    Ok(())
}

/// Lexically normalize a path — resolve `.`/`..` WITHOUT touching the filesystem (a symlink
/// target need not exist). `..` pops the last kept component; `.` is dropped.
#[cfg(unix)]
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// R15 (NER-138 Phase 7 slice 3): a materialized symlink must not escape the worktree. Reject
/// an absolute target, or a relative target whose lexical resolution (from the link's own
/// parent) leaves the worktree root. Errors are PATH-FREE (S1) — neither the link path nor the
/// target is interpolated into the message.
#[cfg(unix)]
fn validate_symlink_target(repo_root: &Path, link_rel: &str, target: &str) -> Result<()> {
    if Path::new(target).is_absolute() {
        bail!("symlink target escapes the worktree (absolute target rejected)");
    }
    let link_parent = Path::new(link_rel)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let combined = lexical_normalize(&repo_root.join(link_parent).join(target));
    if !combined.starts_with(lexical_normalize(repo_root)) {
        bail!("symlink target escapes the worktree");
    }
    Ok(())
}

/// Crash-atomically materialize a symlink (NER-138 Phase 7 slice 3): validate the target
/// (R15), remove any existing entry at the path (without following it), create the symlink
/// under a `.forge-restore-` temp name in the destination's own parent (so a crash
/// mid-materialize leaves a doctor-reclaimable temp, not a torn entry), atomically rename it
/// into place, and fsync the parent directory. `bytes` are the link target.
#[cfg(unix)]
fn materialize_symlink(
    repo_root: &Path,
    rel: &str,
    full: &Path,
    bytes: &[u8],
    target_paths: &mut BTreeSet<String>,
    synced_dirs: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let target =
        std::str::from_utf8(bytes).map_err(|_| anyhow!("symlink target is not valid utf-8"))?;
    validate_symlink_target(repo_root, rel, target)?;
    let parent = full
        .parent()
        .ok_or_else(|| anyhow!("symlink target has no parent"))?;
    let newly_created = missing_dirs(parent);
    fs::create_dir_all(parent)?;
    // Remove any existing entry WITHOUT following a symlink (symlink_metadata): a stale
    // symlink is removed as a file, a real directory via remove_dir_all.
    if let Ok(meta) = fs::symlink_metadata(full) {
        if meta.file_type().is_dir() {
            fs::remove_dir_all(full)?;
        } else {
            fs::remove_file(full)?;
        }
    }
    let name = full
        .file_name()
        .ok_or_else(|| anyhow!("symlink target has no file name"))?
        .to_string_lossy()
        .into_owned();
    let temp = parent.join(format!("{RESTORE_TEMP_PREFIX}{name}"));
    let _ = fs::remove_file(&temp); // clear a crash-orphaned temp from a prior run
    std::os::unix::fs::symlink(target, &temp)
        .map_err(|error| anyhow!("create symlink: {}", error.kind()))?;
    fs::rename(&temp, full).map_err(|error| anyhow!("persist symlink: {}", error.kind()))?;
    // A symlink cannot be fsynced portably; fsyncing the parent dir (which gained the new
    // entry) is the durability boundary, mirroring the regular-file restore.
    if synced_dirs.insert(parent.to_path_buf()) {
        sync_dir(parent)?;
    }
    for dir in &newly_created {
        if let Some(grandparent) = dir.parent() {
            if synced_dirs.insert(grandparent.to_path_buf()) {
                sync_dir(grandparent)?;
            }
        }
    }
    target_paths.insert(rel.to_string());
    Ok(())
}

fn scan_worktree(repo_root: &Path) -> Result<Vec<FileEntry>> {
    let mut files = Vec::new();
    for path in walk_worktree(repo_root)? {
        if is_ignored_by_policy(&path) {
            continue;
        }
        let full = repo_root.join(&path);
        // `symlink_metadata` does NOT follow links: a symlink reports as a symlink (so we can
        // capture it as a `120000` entry via `read_link`, never reading its pointed-at
        // content). A non-symlink file is captured as before.
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            #[cfg(unix)]
            {
                // Capture the link target bytes as a `120000` blob (matching git). The link is
                // never followed, so a symlink pointing at a secret/out-of-tree path leaks no
                // content into the snapshot — only the target string is stored.
                let target = match fs::read_link(&full) {
                    Ok(target) => target.to_string_lossy().into_owned(),
                    Err(_) => continue,
                };
                files.push(FileEntry {
                    path,
                    executable: false,
                    symlink_target: Some(target),
                });
            }
            #[cfg(not(unix))]
            {
                // Non-Unix: preserve the pre-slice-3 behavior (follow the link, capture
                // resolved file content) — documented platform divergence.
                if let Ok(resolved) = fs::metadata(&full) {
                    if resolved.is_file() {
                        files.push(FileEntry {
                            path,
                            executable: is_executable(&resolved),
                            symlink_target: None,
                        });
                    }
                }
            }
            continue;
        }
        if metadata.is_file() {
            files.push(FileEntry {
                path,
                executable: is_executable(&metadata),
                symlink_target: None,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    // Defensive: the serial `ignore` Walk yields each path once, so this dedup is a no-op
    // today — but it guarantees a future change (a second walk root, follow_links, or a
    // parallel walker) can never feed `write_tree` two entries with the same name.
    files.dedup_by(|a, b| a.path == b.path);
    Ok(files)
}

fn hydrate_working_overlay_for_diff(
    repo_root: &Path,
    old_map: &FingerprintMap,
    new_map: &FingerprintMap,
    overlay: &mut BlobOverlay,
) -> Result<()> {
    for (path, new_fp) in new_map {
        if old_map.get(path) == Some(new_fp) || overlay.contains_key(&new_fp.0) {
            continue;
        }
        let bytes = if new_fp.1 == SYMLINK_MODE {
            fs::read_link(repo_root.join(path))?
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else {
            fs::read(repo_root.join(path))?
        };
        overlay.insert(new_fp.0.clone(), bytes);
    }
    Ok(())
}

/// Enumerate snapshot-candidate worktree paths natively, without the `git` binary
/// (NER-138 Phase 7 slice 1). Uses the `ignore` crate to honor repo-local `.gitignore`
/// (nested, with negation) and `.forgeignore`; the authoritative secret/internal
/// exclusion is left to `is_ignored_by_policy` in the caller (`scan_worktree`).
///
/// Exclusion precedence (highest wins):
///   `is_ignored_by_policy` (`.forge/`, `.git/`, `.forge-restore-*`, secret-risk —
///       always wins, not negatable)
///     > `.forgeignore`  (Forge-specific; a `!`-negation can re-include a `.gitignore` drop)
///     > `.gitignore`    (repo-local, nested, with negation)
///     > built-in defaults
///
/// The walker is intentionally **environment-independent**: it does NOT consult
/// `.git/info/exclude` or the user's global `core.excludesfile`, so a repo's native
/// snapshot set is reproducible across machines (the Phase 7 goal). This is a deliberate,
/// documented divergence from `git ls-files --others --exclude-standard`. Resolves the
/// `PRD.md` `.forgeignore` open question for the native backend.
fn walk_worktree(repo_root: &Path) -> Result<Vec<String>> {
    let mut builder = WalkBuilder::new(repo_root);
    builder
        .hidden(false) // git lists dotfiles (.gitignore, .gitattributes, .github/)
        .ignore(false) // git does not honor the `ignore` crate's own `.ignore` files
        .git_ignore(true) // honor repo-local .gitignore (nested, with negation)
        .git_exclude(false) // env-independent: do not read .git/info/exclude
        .git_global(false) // env-independent: do not read global core.excludesfile
        .parents(false) // the repo root is the ignore boundary (matches git)
        .require_git(false) // honor .gitignore even without a .git directory
        .follow_links(false) // do not traverse into symlinked directories
        .add_custom_ignore_filename(".forgeignore") // higher precedence than .gitignore
        .sort_by_file_name(|a, b| a.cmp(b));

    // Prune descent into `is_ignored_by_policy` directories (`.git`, `.forge`) at the walk
    // layer so we never recurse through e.g. thousands of `.git` internals just to discard
    // them. This *reuses* the shared predicate (it is not a fork): the post-walk
    // `is_ignored_by_policy` filter in `scan_worktree` remains the authoritative backstop.
    let prune_root = repo_root.to_path_buf();
    builder.filter_entry(move |entry| match rel_path(&prune_root, entry.path()) {
        Some(rel) => !is_ignored_by_policy(&rel),
        None => true, // the walk root itself has no relative path; always descend it
    });

    let mut paths = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => match map_walk_error(&error) {
                Some(mapped) => return Err(mapped),
                None => continue,
            },
        };
        // Skip directories; yield regular files AND symlinks. `scan_worktree`'s
        // `symlink_metadata` gate then captures a symlink as a `120000` entry (its target,
        // via `read_link` — never followed) and a regular file as content (NER-138 slice 3).
        match entry.file_type() {
            Some(file_type) if file_type.is_dir() => continue,
            None => continue, // no file type (e.g. the stdin sentinel) — never a worktree file
            _ => {}
        }
        if let Some(rel) = rel_path(repo_root, entry.path()) {
            paths.push(rel);
        }
    }
    Ok(paths)
}

/// Map a walk error to either `None` (skip — a benign mid-walk disappearance) or a
/// **path-free** `anyhow` error (security invariant S1: no filesystem path may reach the
/// untyped envelope `message`, which would bypass typed-error secret-path redaction).
/// `ignore::Error`'s own `Display` embeds the offending path, so we never forward it —
/// only the path-free `io::ErrorKind` is surfaced. A `NotFound` (a file that vanished
/// between enumeration and read, realistic under a concurrent agent fleet) is benign and
/// skipped, mirroring the `fs::metadata` skip in `scan_worktree`.
fn map_walk_error(error: &ignore::Error) -> Option<anyhow::Error> {
    match error.io_error() {
        Some(io) if io.kind() == std::io::ErrorKind::NotFound => None,
        Some(io) => Some(anyhow!("failed to walk worktree: {}", io.kind())),
        None => Some(anyhow!("failed to walk worktree")),
    }
}

/// Repo-relative, forward-slash path for a walked entry, or `None` for the walk root
/// itself. Joins only `Normal` components so a filename containing a backslash on Unix is
/// preserved and Windows separators normalize to `/` — matching `is_secret_risk_path`'s
/// `rsplit('/')` and the tree builder's path handling.
///
/// Non-UTF-8 bytes in a filename are best-effort: `to_string_lossy` substitutes U+FFFD, so
/// such a path may not round-trip and the file is dropped at the downstream `fs::metadata`
/// gate. This is no worse than the prior git `ls-files` `.lines()` parsing (which C-quoted
/// such names); faithful non-UTF-8 capture is out of slice-1 scope. The secret backstop is
/// unaffected — `is_secret_risk_path` lowercases the (lossy) filename before matching.
fn rel_path(repo_root: &Path, full: &Path) -> Option<String> {
    let rel = full.strip_prefix(repo_root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn materialized_paths(repo_root: &Path) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for file in scan_worktree(repo_root)? {
        paths.insert(file.path);
    }
    Ok(paths)
}

/// Name-level diff of the base commit's tree against the freshly-built worktree tree
/// (NER-138 Phase 7 slice 2), replacing the prior `git diff --name-only HEAD` +
/// `git ls-files --others` shell-out. A path is reported when its `(blob id, mode)` differs
/// between the two trees: added (worktree-only), removed (base-only), or modified (same
/// path, different blob **or** a changed executable bit). Mode is part of the key so a
/// `chmod +x` with unchanged content still surfaces — matching `git diff --name-only HEAD`,
/// which lists a mode-only change. **Name granularity only — hunk-level diff is Phase 8.**
/// This is reproducibility-over-parity: it does not chase exact `git diff` output.
///
/// The base tree is `ensure_head`'s commit tree (the genesis, established at `start`); by
/// `save` time it is the start-time anchor, so the diff reflects edits since `start`. If
/// the base tree equals the worktree tree (nothing changed, or a just-created genesis),
/// the result is empty. Policy-excluded paths cannot appear in either tree (the walker
/// filters them), but the final `retain` mirrors the git backend's backstop.
fn changed_paths_excluding(
    store: &NativeObjectStore,
    repo_root: &Path,
    worktree_root: &ObjectId,
    excluded_paths: &BTreeSet<&str>,
) -> Result<Vec<String>> {
    let head = ensure_head(repo_root)?;
    let base_tree = ObjectId::parse(&store.read_commit(&head)?.tree)?;
    if &base_tree == worktree_root {
        return Ok(Vec::new());
    }
    let base = flatten_tree(store, &base_tree)?;
    let worktree = flatten_tree(store, worktree_root)?;
    let mut changed = BTreeSet::new();
    for (path, fingerprint) in &worktree {
        if base.get(path) != Some(fingerprint) {
            changed.insert(path.clone()); // added, or content/mode modified
        }
    }
    for path in base.keys() {
        if !worktree.contains_key(path) {
            changed.insert(path.clone()); // removed
        }
    }
    Ok(changed
        .into_iter()
        .filter(|path| !is_ignored_by_policy(path) && !excluded_paths.contains(path.as_str()))
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFileDiff {
    path: String,
    old_path: Option<String>,
    status: String,
    similarity: Option<u8>,
    old: Option<FileFingerprint>,
    new: Option<FileFingerprint>,
}

fn diff_fingerprint_maps(
    store: &NativeObjectStore,
    old_map: BTreeMap<String, FileFingerprint>,
    new_map: BTreeMap<String, FileFingerprint>,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    options: &DiffOptions,
) -> Result<TreeDiff> {
    let mut dropped_secret_paths = BTreeSet::new();
    let mut removed = BTreeMap::new();
    let mut added = BTreeMap::new();
    let mut pending = Vec::new();

    for (path, old_fp) in &old_map {
        if is_ignored_by_policy(path) {
            if new_map.get(path) != Some(old_fp) {
                dropped_secret_paths.insert(path.clone());
            }
            continue;
        }
        match new_map.get(path) {
            None => {
                removed.insert(path.clone(), old_fp.clone());
            }
            Some(new_fp) if new_fp != old_fp => {
                if is_ignored_by_policy(path) {
                    dropped_secret_paths.insert(path.clone());
                } else {
                    pending.push(PendingFileDiff {
                        path: path.clone(),
                        old_path: None,
                        status: "M".to_string(),
                        similarity: None,
                        old: Some(old_fp.clone()),
                        new: Some(new_fp.clone()),
                    });
                }
            }
            Some(_) => {}
        }
    }

    for (path, new_fp) in &new_map {
        if is_ignored_by_policy(path) {
            if old_map.get(path) != Some(new_fp) {
                dropped_secret_paths.insert(path.clone());
            }
            continue;
        }
        if !old_map.contains_key(path) {
            added.insert(path.clone(), new_fp.clone());
        }
    }

    let mut warnings = Vec::new();
    if options.detect_renames {
        detect_renames(
            store,
            blob_overlay,
            &mut removed,
            &mut added,
            &mut pending,
            &mut warnings,
            options,
        )?;
    }

    for (path, old) in removed {
        pending.push(PendingFileDiff {
            path,
            old_path: None,
            status: "D".to_string(),
            similarity: None,
            old: Some(old),
            new: None,
        });
    }
    for (path, new) in added {
        pending.push(PendingFileDiff {
            path,
            old_path: None,
            status: "A".to_string(),
            similarity: None,
            old: None,
            new: Some(new),
        });
    }

    let mut files = Vec::new();
    for change in pending {
        if is_ignored_by_policy(&change.path)
            || change.old_path.as_deref().is_some_and(is_ignored_by_policy)
        {
            dropped_secret_paths.insert(change.path);
            if let Some(old_path) = change.old_path {
                dropped_secret_paths.insert(old_path);
            }
            continue;
        }
        files.push(build_file_diff(
            store,
            blob_overlay,
            change,
            options.include_hunks,
        )?);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path).then(a.old_path.cmp(&b.old_path)));

    Ok(TreeDiff {
        files,
        dropped_secret_paths: dropped_secret_paths.into_iter().collect(),
        warnings,
    })
}

fn detect_renames(
    store: &NativeObjectStore,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    removed: &mut BTreeMap<String, FileFingerprint>,
    added: &mut BTreeMap<String, FileFingerprint>,
    pending: &mut Vec<PendingFileDiff>,
    warnings: &mut Vec<DiffWarning>,
    options: &DiffOptions,
) -> Result<()> {
    let mut exact_pairs = Vec::new();
    for (old_path, old_fp) in removed.iter() {
        if is_ignored_by_policy(old_path) {
            continue;
        }
        if let Some((new_path, new_fp)) = added.iter().find(|(new_path, new_fp)| {
            !is_ignored_by_policy(new_path) && new_fp.0 == old_fp.0 && new_fp.1 == old_fp.1
        }) {
            exact_pairs.push((
                old_path.clone(),
                new_path.clone(),
                old_fp.clone(),
                new_fp.clone(),
            ));
        }
    }
    for (old_path, new_path, old_fp, new_fp) in exact_pairs {
        if removed.remove(&old_path).is_some() && added.remove(&new_path).is_some() {
            pending.push(PendingFileDiff {
                path: new_path,
                old_path: Some(old_path),
                status: "R100".to_string(),
                similarity: Some(100),
                old: Some(old_fp),
                new: Some(new_fp),
            });
        }
    }

    let candidate_count = removed.len().saturating_mul(added.len());
    if candidate_count > options.rename_limit {
        warnings.push(DiffWarning {
            code: "rename_detection_skipped".to_string(),
            message: "inexact rename detection skipped because the candidate set exceeded the configured limit".to_string(),
        });
        return Ok(());
    }

    loop {
        let mut best: Option<(u8, String, String)> = None;
        for (old_path, old_fp) in removed.iter() {
            if is_ignored_by_policy(old_path) {
                continue;
            }
            for (new_path, new_fp) in added.iter() {
                if is_ignored_by_policy(new_path)
                    || old_fp.1 == SYMLINK_MODE
                    || new_fp.1 == SYMLINK_MODE
                    || old_fp.1 != new_fp.1
                {
                    continue;
                }
                let score = similarity_score(store, blob_overlay, old_fp, new_fp)?;
                if score >= options.rename_threshold
                    && best
                        .as_ref()
                        .is_none_or(|(best_score, _, _)| score > *best_score)
                {
                    best = Some((score, old_path.clone(), new_path.clone()));
                }
            }
        }
        let Some((score, old_path, new_path)) = best else {
            break;
        };
        let old_fp = removed
            .remove(&old_path)
            .expect("best old path still present");
        let new_fp = added
            .remove(&new_path)
            .expect("best new path still present");
        pending.push(PendingFileDiff {
            path: new_path,
            old_path: Some(old_path),
            status: format!("R{score}"),
            similarity: Some(score),
            old: Some(old_fp),
            new: Some(new_fp),
        });
    }

    Ok(())
}

fn similarity_score(
    store: &NativeObjectStore,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    old_fp: &FileFingerprint,
    new_fp: &FileFingerprint,
) -> Result<u8> {
    let old = read_blob(store, blob_overlay, &old_fp.0)?;
    let new = read_blob(store, blob_overlay, &new_fp.0)?;
    if is_binary(&old) || is_binary(&new) {
        return Ok(if old == new { 100 } else { 0 });
    }
    let old_lines = split_lines(&old);
    let new_lines = split_lines(&new);
    let larger = old_lines.len().max(new_lines.len());
    if larger == 0 {
        return Ok(100);
    }
    let mut old_counts = BTreeMap::<Vec<u8>, usize>::new();
    for line in old_lines {
        *old_counts.entry(line).or_default() += 1;
    }
    let mut common = 0usize;
    for line in new_lines {
        if let Some(count) = old_counts.get_mut(&line) {
            if *count > 0 {
                *count -= 1;
                common += 1;
            }
        }
    }
    Ok(((common * 100) / larger).min(100) as u8)
}

fn build_file_diff(
    store: &NativeObjectStore,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    change: PendingFileDiff,
    include_hunks: bool,
) -> Result<FileDiff> {
    let old_blob = change.old.as_ref().map(|fp| fp.0.as_str());
    let new_blob = change.new.as_ref().map(|fp| fp.0.as_str());
    let old_mode = change.old.as_ref().map(|fp| fp.1);
    let new_mode = change.new.as_ref().map(|fp| fp.1);
    let old_bytes = read_optional_blob(store, blob_overlay, old_blob)?;
    let new_bytes = read_optional_blob(store, blob_overlay, new_blob)?;
    let binary =
        old_bytes.as_deref().is_some_and(is_binary) || new_bytes.as_deref().is_some_and(is_binary);
    let symlink = old_mode == Some(SYMLINK_MODE) || new_mode == Some(SYMLINK_MODE);

    let (insertions, deletions, hunk, hunks, truncated) = if binary {
        (None, None, None, Vec::new(), false)
    } else if symlink {
        let insertions = new_bytes
            .as_ref()
            .map(|bytes| split_lines(bytes).len() as u64);
        let deletions = old_bytes
            .as_ref()
            .map(|bytes| split_lines(bytes).len() as u64);
        (insertions, deletions, None, Vec::new(), false)
    } else {
        let old_bytes = old_bytes.as_deref().unwrap_or(&[]);
        let new_bytes = new_bytes.as_deref().unwrap_or(&[]);
        let body = diff_text_bytes(old_bytes, new_bytes, include_hunks)?;
        (
            Some(body.insertions),
            Some(body.deletions),
            body.hunk,
            body.hunks,
            body.truncated,
        )
    };

    Ok(FileDiff {
        path: change.path,
        status: change.status,
        insertions,
        deletions,
        binary,
        hunk,
        truncated,
        hunks,
        old_path: change.old_path,
        similarity: change.similarity,
    })
}

fn read_optional_blob(
    store: &NativeObjectStore,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    id: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    id.map(|id| read_blob(store, blob_overlay, id)).transpose()
}

fn read_blob(
    store: &NativeObjectStore,
    blob_overlay: &BTreeMap<String, Vec<u8>>,
    id: &str,
) -> Result<Vec<u8>> {
    if let Some(bytes) = blob_overlay.get(id) {
        return Ok(bytes.clone());
    }
    store.read_object(&ObjectId::parse(id)?)
}

#[derive(Debug)]
struct TextDiffBody {
    insertions: u64,
    deletions: u64,
    hunk: Option<String>,
    hunks: Vec<HunkDiff>,
    truncated: bool,
}

fn diff_text_bytes(old: &[u8], new: &[u8], include_hunks: bool) -> Result<TextDiffBody> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);
    let ops = similar::capture_diff_slices(similar::Algorithm::Patience, &old_lines, &new_lines);
    let groups = similar::group_diff_ops(ops, DIFF_CONTEXT_LINES);

    let mut text = String::new();
    let mut hunks = Vec::new();
    let mut insertions = 0u64;
    let mut deletions = 0u64;

    for group in groups {
        let Some(first) = group.first() else { continue };
        let Some(last) = group.last() else { continue };
        let mut hunk_header = None;
        if include_hunks {
            let (_, first_old, first_new) = first.as_tag_tuple();
            let (_, last_old, last_new) = last.as_tag_tuple();
            let old_start = first_old.start as u64 + 1;
            let new_start = first_new.start as u64 + 1;
            let old_lines_count = last_old.end.saturating_sub(first_old.start) as u64;
            let new_lines_count = last_new.end.saturating_sub(first_new.start) as u64;
            text.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                old_start, old_lines_count, new_start, new_lines_count
            ));
            hunk_header = Some((old_start, old_lines_count, new_start, new_lines_count));
        }
        let mut lines = Vec::new();
        for op in &group {
            for change in op.iter_changes(&old_lines, &new_lines) {
                let tag = match change.tag() {
                    similar::ChangeTag::Equal => DiffLineTag::Context,
                    similar::ChangeTag::Delete => {
                        deletions += 1;
                        DiffLineTag::Delete
                    }
                    similar::ChangeTag::Insert => {
                        insertions += 1;
                        DiffLineTag::Insert
                    }
                };
                if include_hunks {
                    let prefix = match tag {
                        DiffLineTag::Context => ' ',
                        DiffLineTag::Delete => '-',
                        DiffLineTag::Insert => '+',
                    };
                    let line = line_to_redacted_string(change.value().as_slice());
                    text.push(prefix);
                    text.push_str(&line);
                    text.push('\n');
                    lines.push(DiffLine { tag, content: line });
                }
            }
        }
        if let Some((old_start, old_lines_count, new_start, new_lines_count)) = hunk_header {
            hunks.push(HunkDiff {
                old_start,
                old_lines: old_lines_count,
                new_start,
                new_lines: new_lines_count,
                lines,
            });
        }
    }

    let mut truncated = false;
    let hunk = if text.is_empty() {
        None
    } else {
        let (bounded, was_truncated) = bound_string(text, HUNK_LIMIT);
        truncated |= was_truncated;
        Some(bounded)
    };
    while serde_json::to_vec(&hunks)?.len() > HUNK_LIMIT {
        truncated = true;
        let Some(last) = hunks.last_mut() else { break };
        if last.lines.pop().is_none() {
            hunks.pop();
        }
    }

    Ok(TextDiffBody {
        insertions,
        deletions,
        hunk,
        hunks,
        truncated,
    })
}

fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(bytes[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < bytes.len() {
        lines.push(bytes[start..].to_vec());
    }
    lines
}

fn line_to_redacted_string(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    let (redacted, _kinds) = forge_content::redact_evidence_excerpt(&text);
    redacted
}

fn bound_string(value: String, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value, false);
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SCAN_LIMIT).any(|byte| *byte == 0)
}

/// A file leaf's change-detection fingerprint: its blob object id AND its mode, so a
/// mode-only change (e.g. `chmod +x`) is detected even though the blob content (hence the
/// blob id) is unchanged.
type FileFingerprint = (String, u32);
type FingerprintMap = BTreeMap<String, FileFingerprint>;
type BlobOverlay = BTreeMap<String, Vec<u8>>;

fn merge_fingerprint_maps(
    store: &NativeObjectStore,
    base: &FingerprintMap,
    ours: &FingerprintMap,
    theirs: &FingerprintMap,
) -> Result<NativeMergeResult> {
    let mut paths = BTreeSet::new();
    paths.extend(base.keys().cloned());
    paths.extend(ours.keys().cloned());
    paths.extend(theirs.keys().cloned());

    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut dropped_secret_paths = Vec::new();

    for path in paths {
        if is_ignored_by_policy(&path) {
            dropped_secret_paths.push(path);
            continue;
        }
        let base_fp = base.get(&path);
        let ours_fp = ours.get(&path);
        let theirs_fp = theirs.get(&path);

        if has_dir_file_collision(&path, ours, theirs) {
            conflicts.push(native_merge_conflict(
                path,
                NativeMergeConflictKind::DirFile,
                base_fp,
                ours_fp,
                theirs_fp,
            ));
            continue;
        }

        if ours_fp == theirs_fp {
            if let Some(fp) = ours_fp {
                merged.insert(path, fp.clone());
            }
            continue;
        }
        if ours_fp == base_fp {
            if let Some(fp) = theirs_fp {
                merged.insert(path, fp.clone());
            }
            continue;
        }
        if theirs_fp == base_fp {
            if let Some(fp) = ours_fp {
                merged.insert(path, fp.clone());
            }
            continue;
        }

        match (base_fp, ours_fp, theirs_fp) {
            (Some(base_fp), Some(ours_fp), Some(theirs_fp)) => {
                if let Some(fp) = merge_regular_text_file(store, base_fp, ours_fp, theirs_fp)? {
                    merged.insert(path, fp);
                } else {
                    conflicts.push(native_merge_conflict(
                        path,
                        classify_three_way_conflict(store, base_fp, ours_fp, theirs_fp)?,
                        Some(base_fp),
                        Some(ours_fp),
                        Some(theirs_fp),
                    ));
                }
            }
            (Some(_), None, Some(_)) | (Some(_), Some(_), None) => {
                conflicts.push(native_merge_conflict(
                    path,
                    NativeMergeConflictKind::DeleteModify,
                    base_fp,
                    ours_fp,
                    theirs_fp,
                ));
            }
            (None, Some(ours_fp), Some(theirs_fp)) => {
                let kind = if merge_entry_is_binary_or_symlink(store, ours_fp, theirs_fp)? {
                    classify_added_added_conflict(store, ours_fp, theirs_fp)?
                } else {
                    NativeMergeConflictKind::Content
                };
                conflicts.push(native_merge_conflict(
                    path,
                    kind,
                    base_fp,
                    Some(ours_fp),
                    Some(theirs_fp),
                ));
            }
            _ => {}
        }
    }

    let merged_content_ref = if conflicts.is_empty() {
        let root = write_tree_from_fingerprints(store, &merged, "")?;
        Some(format!("{FORGE_TREE_PREFIX}{root}"))
    } else {
        None
    };
    Ok(NativeMergeResult {
        merged_content_ref,
        conflicts,
        dropped_secret_paths,
    })
}

fn has_dir_file_collision(path: &str, ours: &FingerprintMap, theirs: &FingerprintMap) -> bool {
    (ours.contains_key(path) && has_descendant(theirs, path))
        || (theirs.contains_key(path) && has_descendant(ours, path))
}

fn has_descendant(map: &FingerprintMap, path: &str) -> bool {
    let prefix = format!("{path}/");
    map.keys().any(|candidate| candidate.starts_with(&prefix))
}

fn native_merge_conflict(
    path: String,
    kind: NativeMergeConflictKind,
    base: Option<&FileFingerprint>,
    ours: Option<&FileFingerprint>,
    theirs: Option<&FileFingerprint>,
) -> NativeMergeConflict {
    NativeMergeConflict {
        path,
        kind,
        base_ref: base.map(|fp| fp.0.clone()),
        ours_ref: ours.map(|fp| fp.0.clone()),
        theirs_ref: theirs.map(|fp| fp.0.clone()),
        base_status: side_status(base),
        ours_status: side_status(ours),
        theirs_status: side_status(theirs),
        base_mode: base.map(|fp| fp.1),
        ours_mode: ours.map(|fp| fp.1),
        theirs_mode: theirs.map(|fp| fp.1),
    }
}

fn side_status(side: Option<&FileFingerprint>) -> Option<String> {
    Some(if side.is_some() { "present" } else { "absent" }.to_string())
}

fn classify_three_way_conflict(
    store: &NativeObjectStore,
    base: &FileFingerprint,
    ours: &FileFingerprint,
    theirs: &FileFingerprint,
) -> Result<NativeMergeConflictKind> {
    if [base.1, ours.1, theirs.1].contains(&SYMLINK_MODE) {
        return Ok(NativeMergeConflictKind::Symlink);
    }
    if ours.1 != theirs.1 || ours.1 != base.1 {
        return Ok(NativeMergeConflictKind::Mode);
    }
    let base_bytes = read_blob(store, &BTreeMap::new(), &base.0)?;
    let ours_bytes = read_blob(store, &BTreeMap::new(), &ours.0)?;
    let theirs_bytes = read_blob(store, &BTreeMap::new(), &theirs.0)?;
    if is_binary(&base_bytes) || is_binary(&ours_bytes) || is_binary(&theirs_bytes) {
        return Ok(NativeMergeConflictKind::Binary);
    }
    Ok(NativeMergeConflictKind::Content)
}

fn classify_added_added_conflict(
    store: &NativeObjectStore,
    ours: &FileFingerprint,
    theirs: &FileFingerprint,
) -> Result<NativeMergeConflictKind> {
    if ours.1 == SYMLINK_MODE || theirs.1 == SYMLINK_MODE {
        return Ok(NativeMergeConflictKind::Symlink);
    }
    if ours.1 != theirs.1 {
        return Ok(NativeMergeConflictKind::Mode);
    }
    let ours_bytes = read_blob(store, &BTreeMap::new(), &ours.0)?;
    let theirs_bytes = read_blob(store, &BTreeMap::new(), &theirs.0)?;
    if is_binary(&ours_bytes) || is_binary(&theirs_bytes) {
        return Ok(NativeMergeConflictKind::Binary);
    }
    Ok(NativeMergeConflictKind::Content)
}

fn merge_entry_is_binary_or_symlink(
    store: &NativeObjectStore,
    ours: &FileFingerprint,
    theirs: &FileFingerprint,
) -> Result<bool> {
    if ours.1 == SYMLINK_MODE || theirs.1 == SYMLINK_MODE {
        return Ok(true);
    }
    let ours_bytes = read_blob(store, &BTreeMap::new(), &ours.0)?;
    let theirs_bytes = read_blob(store, &BTreeMap::new(), &theirs.0)?;
    Ok(is_binary(&ours_bytes) || is_binary(&theirs_bytes))
}

fn merge_regular_text_file(
    store: &NativeObjectStore,
    base: &FileFingerprint,
    ours: &FileFingerprint,
    theirs: &FileFingerprint,
) -> Result<Option<FileFingerprint>> {
    if [base.1, ours.1, theirs.1].contains(&SYMLINK_MODE) || base.1 != ours.1 || base.1 != theirs.1
    {
        return Ok(None);
    }
    let base_bytes = read_blob(store, &BTreeMap::new(), &base.0)?;
    let ours_bytes = read_blob(store, &BTreeMap::new(), &ours.0)?;
    let theirs_bytes = read_blob(store, &BTreeMap::new(), &theirs.0)?;
    if is_binary(&base_bytes) || is_binary(&ours_bytes) || is_binary(&theirs_bytes) {
        return Ok(None);
    }
    let Some(merged) = merge_text_non_overlapping(&base_bytes, &ours_bytes, &theirs_bytes)? else {
        return Ok(None);
    };
    let blob = store.write_object(ObjectKind::Blob, &merged)?;
    Ok(Some((blob.to_string(), base.1)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextReplacement {
    start: usize,
    end: usize,
    lines: Vec<Vec<u8>>,
}

fn merge_text_non_overlapping(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Option<Vec<u8>>> {
    let base_lines = split_lines(base);
    let ours_replacements = text_replacements(&base_lines, &split_lines(ours));
    let theirs_replacements = text_replacements(&base_lines, &split_lines(theirs));
    for ours_replacement in &ours_replacements {
        for theirs_replacement in &theirs_replacements {
            if replacements_conflict(ours_replacement, theirs_replacement) {
                return Ok(None);
            }
        }
    }
    let mut replacements = ours_replacements;
    replacements.extend(theirs_replacements);
    coalesce_shared_boundary_replacements(&mut replacements);
    replacements.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then(b.end.cmp(&a.end))
            .then(b.lines.cmp(&a.lines))
    });
    replacements.dedup();
    let mut merged = base_lines;
    for replacement in replacements {
        merged.splice(replacement.start..replacement.end, replacement.lines);
    }
    Ok(Some(merged.into_iter().flatten().collect()))
}

fn coalesce_shared_boundary_replacements(replacements: &mut Vec<TextReplacement>) {
    let mut index = 0;
    while index < replacements.len() {
        let mut merged = false;
        let mut other_index = index + 1;
        while other_index < replacements.len() {
            if let Some(replacement) =
                merge_shared_boundary_replacements(&replacements[index], &replacements[other_index])
            {
                replacements[index] = replacement;
                replacements.remove(other_index);
                merged = true;
                break;
            }
            other_index += 1;
        }
        if !merged {
            index += 1;
        }
    }
}

fn merge_shared_boundary_replacements(
    a: &TextReplacement,
    b: &TextReplacement,
) -> Option<TextReplacement> {
    if a.end == b.start && a.lines.last()? == b.lines.first()? {
        let mut lines = a.lines.clone();
        lines.extend_from_slice(&b.lines[1..]);
        return Some(TextReplacement {
            start: a.start,
            end: b.end,
            lines,
        });
    }
    if b.end == a.start && b.lines.last()? == a.lines.first()? {
        let mut lines = b.lines.clone();
        lines.extend_from_slice(&a.lines[1..]);
        return Some(TextReplacement {
            start: b.start,
            end: a.end,
            lines,
        });
    }
    None
}

fn text_replacements(base: &[Vec<u8>], side: &[Vec<u8>]) -> Vec<TextReplacement> {
    similar::capture_diff_slices(similar::Algorithm::Patience, base, side)
        .into_iter()
        .filter_map(|op| {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            if matches!(tag, similar::DiffTag::Equal) {
                return None;
            }
            Some(TextReplacement {
                start: old_range.start,
                end: old_range.end,
                lines: side[new_range].to_vec(),
            })
        })
        .collect()
}

fn replacements_conflict(a: &TextReplacement, b: &TextReplacement) -> bool {
    let overlapping = a.start < b.end && b.start < a.end;
    let same_insertion = a.start == a.end && b.start == b.end && a.start == b.start;
    overlapping || (same_insertion && a.lines != b.lines)
}

fn write_tree_from_fingerprints(
    store: &NativeObjectStore,
    files: &FingerprintMap,
    prefix: &str,
) -> Result<ObjectId> {
    let mut entries = Vec::new();
    let mut child_dirs = BTreeSet::new();
    for (path, (object, mode)) in files {
        let rest = if prefix.is_empty() {
            path.as_str()
        } else if let Some(rest) = path.strip_prefix(&format!("{prefix}/")) {
            rest
        } else {
            continue;
        };
        if let Some((dir, _)) = rest.split_once('/') {
            child_dirs.insert(dir.to_string());
            continue;
        }
        entries.push(TreeEntry {
            name: rest.to_string(),
            kind: TreeEntryKind::File,
            mode: *mode,
            object: object.clone(),
        });
    }
    for dir in child_dirs {
        let child_prefix = if prefix.is_empty() {
            dir.clone()
        } else {
            format!("{prefix}/{dir}")
        };
        let child = write_tree_from_fingerprints(store, files, &child_prefix)?;
        entries.push(TreeEntry {
            name: dir,
            kind: TreeEntryKind::Dir,
            mode: 0o040000,
            object: child.to_string(),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let payload = serde_json::to_vec(&TreeObject {
        schema_version: SCHEMA_VERSION,
        entries,
    })?;
    store.write_object(ObjectKind::Tree, &payload)
}

/// Flatten a native tree into a map of repo-relative file path → `(blob id, mode)`,
/// recursing into directory entries. Used by `changed_paths` for the name-level
/// base-vs-worktree diff.
fn flatten_tree(
    store: &NativeObjectStore,
    tree_id: &ObjectId,
) -> Result<BTreeMap<String, FileFingerprint>> {
    let mut out = BTreeMap::new();
    flatten_tree_into(store, tree_id, "", &mut out)?;
    Ok(out)
}

fn flatten_tree_into(
    store: &NativeObjectStore,
    tree_id: &ObjectId,
    prefix: &str,
    out: &mut BTreeMap<String, FileFingerprint>,
) -> Result<()> {
    let payload = store.read_object(tree_id)?;
    let tree: TreeObject = serde_json::from_slice(&payload)
        .with_context(|| format!("malformed native tree object {}", tree_id))?;
    if tree.schema_version != SCHEMA_VERSION {
        bail!("unsupported native tree schema version");
    }
    for entry in tree.entries {
        validate_tree_entry(&entry)?;
        let path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        match entry.kind {
            TreeEntryKind::File => {
                out.insert(path, (entry.object, entry.mode));
            }
            TreeEntryKind::Dir => {
                let child = ObjectId::parse(&entry.object)?;
                // Mirror verify_reachable/materialize_tree: a Dir entry must point at a
                // Tree, so a corrupt entry surfaces a typed "wrong object type" error
                // rather than a downstream serde failure.
                ensure_child_kind(&entry, &child)?;
                flatten_tree_into(store, &child, &path, out)?;
            }
        }
    }
    Ok(())
}

fn validate_tree_entry(entry: &TreeEntry) -> Result<()> {
    if entry.name.is_empty()
        || entry.name == "."
        || entry.name == ".."
        || entry.name.contains('/')
        || entry.name.contains('\\')
        || Path::new(&entry.name).is_absolute()
    {
        bail!("unsafe native tree entry name");
    }
    Ok(())
}

fn ensure_child_kind(entry: &TreeEntry, child: &ObjectId) -> Result<()> {
    match (entry.kind, child.kind()?) {
        (TreeEntryKind::File, ObjectKind::Blob) | (TreeEntryKind::Dir, ObjectKind::Tree) => Ok(()),
        _ => bail!("native tree entry points at wrong object type"),
    }
}

/// Fsync a directory so a newly created or renamed entry within it is durable.
/// Errors propagate — a swallowed directory sync silently breaks crash-durability,
/// which is the hole this replaces. This is a deliberate fail-hard: a write whose
/// directory entry may not survive a crash must not report success.
/// Known limitation: a few filesystems (some network/overlay mounts) reject fsync on a
/// directory fd (EINVAL/ENOTSUP), where directory durability is meaningless anyway; on
/// those `.forge` locations write_object will now error. `.forge` is expected to be on a
/// local filesystem (Phase 1b's WAL work makes that constraint explicit), so tolerating
/// those errnos as a degraded no-op is deferred rather than guessed at here.
fn sync_dir(path: &Path) -> Result<()> {
    let dir =
        File::open(path).map_err(|error| anyhow!("open directory for fsync: {}", error.kind()))?;
    dir.sync_all()
        .map_err(|error| anyhow!("fsync directory: {}", error.kind()))?;
    Ok(())
}

/// Ancestor directories of `dir` (inclusive) that do not yet exist, ordered
/// shallowest-first. Lets the caller fsync only the directories whose creation is
/// new, so already-durable directories are not re-synced on every write.
fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = Some(dir);
    while let Some(path) = current {
        if path.exists() {
            break;
        }
        missing.push(path.to_path_buf());
        current = path.parent();
    }
    missing.reverse();
    missing
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_ids_are_domain_separated() {
        let blob = ObjectId::new(ObjectKind::Blob, b"same");
        let tree = ObjectId::new(ObjectKind::Tree, b"same");
        let commit = ObjectId::new(ObjectKind::Commit, b"same");
        assert_eq!(blob, ObjectId::new(ObjectKind::Blob, b"same"));
        // The same payload hashes to three distinct ids — the domain set is `commit`-aware
        // (NER-138 Phase 7 slice 2).
        assert_ne!(blob, tree);
        assert_ne!(blob, commit);
        assert_ne!(tree, commit);
        assert!(ObjectId::parse(&blob.to_string()).is_ok());
        assert!(ObjectId::parse(&commit.to_string()).is_ok());
        assert_eq!(commit.kind().unwrap(), ObjectKind::Commit);
        assert!(ObjectId::parse("f1:blob:sha256:not-hex").is_err());
    }

    #[test]
    fn merge_commit_object_id_is_a_golden_vector() {
        let commit = CommitObject {
            schema_version: COMMIT_SCHEMA_VERSION,
            tree: format!("f1:tree:sha256:{}", "1".repeat(64)),
            parents: vec![
                format!("f1:commit:sha256:{}", "2".repeat(64)),
                format!("f1:commit:sha256:{}", "3".repeat(64)),
            ],
            intent_id: Some("intent_merge".to_string()),
            proposal_revision_id: Some("revision_merge".to_string()),
            decision_id: Some("decision_merge".to_string()),
            evidence_digest: Some(Hex64::new("4".repeat(64)).unwrap()),
            actor: Some("agent".to_string()),
            authored_time: Some(1_234_567_890),
        };
        let payload = serde_json::to_vec(&commit).unwrap();
        let id = ObjectId::new(ObjectKind::Commit, &payload);

        assert_eq!(
            id.to_string(),
            "f1:commit:sha256:1009c818d414f87683ecd37232868515820a6b99b6b0bac55cb0e15f1801fca7"
        );
    }

    #[test]
    fn loose_object_write_is_idempotent_and_verified() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let id = store.write_object(ObjectKind::Blob, b"payload").unwrap();
        let again = store.write_object(ObjectKind::Blob, b"payload").unwrap();
        assert_eq!(id, again);
        assert_eq!(store.read_object(&id).unwrap(), b"payload");

        fs::write(store.object_path(&id), b"corrupt").unwrap();
        assert!(store
            .read_object(&id)
            .unwrap_err()
            .to_string()
            .contains("hash mismatch"));
    }

    #[test]
    #[cfg(unix)]
    fn tree_fingerprints_flatten_nested_executable_and_symlink_entries() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/app.sh"), b"#!/bin/sh\n").unwrap();
        fs::write(repo.join("README.md"), b"readme\n").unwrap();
        fs::set_permissions(repo.join("src/app.sh"), fs::Permissions::from_mode(0o755)).unwrap();

        let store = NativeObjectStore::new(repo);
        let root = write_tree(
            &store,
            repo,
            &[
                FileEntry {
                    path: "README.md".to_string(),
                    executable: false,
                    symlink_target: None,
                },
                FileEntry {
                    path: "src/app.sh".to_string(),
                    executable: true,
                    symlink_target: None,
                },
                FileEntry {
                    path: "src/link".to_string(),
                    executable: false,
                    symlink_target: Some("../README.md".to_string()),
                },
            ],
            "",
        )
        .unwrap();

        let fingerprints = store.tree_fingerprints(&root).unwrap();
        assert_eq!(fingerprints["README.md"].1, 0o100644);
        assert_eq!(fingerprints["src/app.sh"].1, 0o100755);
        assert_eq!(fingerprints["src/link"].1, 0o120000);
    }

    #[test]
    fn tree_fingerprints_empty_tree_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let root = write_tree(&store, temp.path(), &[], "").unwrap();
        assert!(store.tree_fingerprints(&root).unwrap().is_empty());
    }

    fn write_test_tree(repo: &Path, files: &[(&str, &[u8], bool)]) -> ObjectId {
        for (path, bytes, executable) in files {
            let full = repo.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, bytes).unwrap();
            #[cfg(unix)]
            if *executable {
                fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let entries: Vec<FileEntry> = files
            .iter()
            .map(|(path, _bytes, executable)| FileEntry {
                path: (*path).to_string(),
                executable: *executable,
                symlink_target: None,
            })
            .collect();
        write_tree(&NativeObjectStore::new(repo), repo, &entries, "").unwrap()
    }

    fn merged_file_bytes(repo: &Path, content_ref: &str, path: &str) -> Vec<u8> {
        let store = NativeObjectStore::new(repo);
        let root = object_id_from_content_ref(content_ref).unwrap();
        let fingerprints = store.tree_fingerprints(&root).unwrap();
        let (blob, _mode) = fingerprints.get(path).expect("merged path exists");
        store.read_object(&ObjectId::parse(blob).unwrap()).unwrap()
    }

    #[test]
    fn merge_native_trees_takes_one_sided_text_change() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("app.txt", b"one\n", false)]);
        let ours = write_test_tree(repo, &[("app.txt", b"ours\n", false)]);
        let theirs = write_test_tree(repo, &[("app.txt", b"one\n", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert!(result.is_clean(), "{result:?}");
        let content_ref = result.merged_content_ref.as_deref().unwrap();
        assert_eq!(merged_file_bytes(repo, content_ref, "app.txt"), b"ours\n");
    }

    #[test]
    fn merge_native_trees_combines_non_overlapping_text_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("app.txt", b"one\ntwo\nthree\nfour\n", false)]);
        let ours = write_test_tree(repo, &[("app.txt", b"ONE\ntwo\nthree\nfour\n", false)]);
        let theirs = write_test_tree(repo, &[("app.txt", b"one\ntwo\nthree\nFOUR\n", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert!(result.is_clean(), "{result:?}");
        let content_ref = result.merged_content_ref.as_deref().unwrap();
        assert_eq!(
            merged_file_bytes(repo, content_ref, "app.txt"),
            b"ONE\ntwo\nthree\nFOUR\n"
        );
    }

    #[test]
    fn merge_native_trees_deduplicates_identical_concurrent_insertions() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("app.txt", b"one\nthree\n", false)]);
        let ours = write_test_tree(repo, &[("app.txt", b"ONE\ntwo\nthree\n", false)]);
        let theirs = write_test_tree(repo, &[("app.txt", b"one\ntwo\nTHREE\n", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert!(result.is_clean(), "{result:?}");
        let content_ref = result.merged_content_ref.as_deref().unwrap();
        assert_eq!(
            merged_file_bytes(repo, content_ref, "app.txt"),
            b"ONE\ntwo\nTHREE\n"
        );
    }

    #[test]
    fn merge_native_trees_reports_overlapping_content_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("app.txt", b"one\ntwo\nthree\n", false)]);
        let ours = write_test_tree(repo, &[("app.txt", b"one\nours\nthree\n", false)]);
        let theirs = write_test_tree(repo, &[("app.txt", b"one\ntheirs\nthree\n", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert_eq!(result.merged_content_ref, None);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].path, "app.txt");
        assert_eq!(result.conflicts[0].kind, NativeMergeConflictKind::Content);
    }

    #[test]
    fn merge_native_trees_reports_binary_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("bin.dat", b"a\0base", false)]);
        let ours = write_test_tree(repo, &[("bin.dat", b"a\0ours", false)]);
        let theirs = write_test_tree(repo, &[("bin.dat", b"a\0theirs", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].kind, NativeMergeConflictKind::Binary);
    }

    #[test]
    fn merge_native_trees_reports_delete_modify_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let base = write_test_tree(repo, &[("app.txt", b"base\n", false)]);
        let ours = write_test_tree(repo, &[]);
        let theirs = write_test_tree(repo, &[("app.txt", b"changed\n", false)]);

        let result = merge_native_trees(&store, &base, &ours, &theirs).unwrap();

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(
            result.conflicts[0].kind,
            NativeMergeConflictKind::DeleteModify
        );
    }

    #[test]
    fn merge_native_trees_reports_dir_file_conflict() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let store = NativeObjectStore::new(repo);
        let ours_blob = store.write_object(ObjectKind::Blob, b"file\n").unwrap();
        let theirs_blob = store.write_object(ObjectKind::Blob, b"nested\n").unwrap();
        let base = BTreeMap::new();
        let ours = BTreeMap::from([("node".to_string(), (ours_blob.to_string(), 0o100644))]);
        let theirs = BTreeMap::from([(
            "node/file.txt".to_string(),
            (theirs_blob.to_string(), 0o100644),
        )]);

        let result = merge_fingerprint_maps(&store, &base, &ours, &theirs).unwrap();

        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].path, "node");
        assert_eq!(result.conflicts[0].kind, NativeMergeConflictKind::DirFile);
    }

    #[test]
    fn diff_native_trees_reports_structured_redacted_hunks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("app.txt", b"one\nAPI_TOKEN=oldsecret\n", false)]);
        let new = write_test_tree(
            repo,
            &[("app.txt", b"one\nAPI_TOKEN=newsecret\nthree\n", false)],
        );
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let file = &diff.files[0];
        assert_eq!(file.status, "M");
        assert_eq!(file.insertions, Some(2));
        assert_eq!(file.deletions, Some(1));
        assert!(file.hunk.as_deref().unwrap().contains("[REDACTED]"));
        assert!(!file.hunk.as_deref().unwrap().contains("newsecret"));
        let rendered = serde_json::to_string(&file.hunks).unwrap();
        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("oldsecret"));
        assert!(!rendered.contains("newsecret"));
    }

    #[test]
    fn diff_native_trees_drops_policy_paths_before_emit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("safe.txt", b"old\n", false)]);
        let new = write_test_tree(
            repo,
            &[
                ("safe.txt", b"new\n", false),
                (".forge/forge.db", b"internal\n", false),
                (".env", b"TOKEN=secret\n", false),
            ],
        );
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        assert_eq!(
            diff.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            vec!["safe.txt"]
        );
        assert!(diff
            .dropped_secret_paths
            .contains(&".forge/forge.db".to_string()));
        assert!(diff.dropped_secret_paths.contains(&".env".to_string()));
    }

    #[test]
    fn diff_native_trees_handles_binary_and_truncates_structured_output() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(
            repo,
            &[("bin.dat", b"a\0b", false), ("big.txt", b"start\n", false)],
        );
        let big = format!("start\n{}", "line\n".repeat(2000));
        let new = write_test_tree(
            repo,
            &[
                ("bin.dat", b"a\0c", false),
                ("big.txt", big.as_bytes(), false),
            ],
        );
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert!(by_path["bin.dat"].binary);
        assert_eq!(by_path["bin.dat"].insertions, None);
        assert!(by_path["bin.dat"].hunks.is_empty());
        assert!(by_path["big.txt"].truncated);
        assert!(by_path["big.txt"].hunk.as_deref().unwrap().len() <= HUNK_LIMIT);
        assert!(serde_json::to_vec(&by_path["big.txt"].hunks).unwrap().len() <= HUNK_LIMIT);
    }

    #[test]
    fn diff_native_trees_counts_text_changes_when_hunks_are_suppressed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("app.txt", b"one\ntwo\nthree\n", false)]);
        let new = write_test_tree(repo, &[("app.txt", b"one\nTWO\nthree\nfour\n", false)]);
        let store = NativeObjectStore::new(repo);
        let options = DiffOptions {
            include_hunks: false,
            ..DiffOptions::default()
        };

        let diff = diff_native_trees(&store, &old, &new, &options).unwrap();
        let file = &diff.files[0];
        assert_eq!(file.status, "M");
        assert_eq!(file.insertions, Some(2));
        assert_eq!(file.deletions, Some(1));
        assert!(file.hunk.is_none());
        assert!(file.hunks.is_empty());
        assert!(!file.truncated);
    }

    #[test]
    #[cfg(unix)]
    fn diff_native_trees_reports_chmod_and_symlink_without_line_hunks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        fs::write(repo.join("script.sh"), b"echo hi\n").unwrap();
        let store = NativeObjectStore::new(repo);
        let old = write_tree(
            &store,
            repo,
            &[FileEntry {
                path: "script.sh".to_string(),
                executable: false,
                symlink_target: None,
            }],
            "",
        )
        .unwrap();
        fs::set_permissions(repo.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let new = write_tree(
            &store,
            repo,
            &[
                FileEntry {
                    path: "script.sh".to_string(),
                    executable: true,
                    symlink_target: None,
                },
                FileEntry {
                    path: "link".to_string(),
                    executable: false,
                    symlink_target: Some("script.sh".to_string()),
                },
            ],
            "",
        )
        .unwrap();

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["script.sh"].status, "M");
        assert!(by_path["script.sh"].hunks.is_empty());
        assert_eq!(by_path["link"].status, "A");
        assert!(by_path["link"].hunks.is_empty());
    }

    #[test]
    fn diff_native_trees_detects_exact_and_inexact_renames() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(
            repo,
            &[
                ("old.txt", b"a\nb\nc\n", false),
                ("move.txt", b"one\ntwo\nthree\nfour\n", false),
            ],
        );
        let new = write_test_tree(
            repo,
            &[
                ("new.txt", b"a\nb\nc\n", false),
                ("moved.txt", b"one\ntwo\nthree\nchanged\n", false),
            ],
        );
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["new.txt"].status, "R100");
        assert_eq!(by_path["new.txt"].old_path.as_deref(), Some("old.txt"));
        assert!(by_path["moved.txt"].status.starts_with('R'));
        assert_eq!(by_path["moved.txt"].old_path.as_deref(), Some("move.txt"));
        assert!(by_path["moved.txt"].similarity.unwrap() >= 50);
    }

    #[test]
    #[cfg(unix)]
    fn diff_native_trees_does_not_pair_renames_across_mode_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("old.txt", b"same\n", false)]);
        let new = write_test_tree(repo, &[("new.txt", b"same\n", true)]);
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["old.txt"].status, "D");
        assert_eq!(by_path["new.txt"].status, "A");
        assert!(diff.files.iter().all(|file| !file.status.starts_with('R')));
    }

    #[test]
    #[cfg(unix)]
    fn diff_native_trees_does_not_pair_regular_file_to_symlink_rename() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        fs::write(repo.join("old.txt"), b"target").unwrap();
        let store = NativeObjectStore::new(repo);
        let old = write_tree(
            &store,
            repo,
            &[FileEntry {
                path: "old.txt".to_string(),
                executable: false,
                symlink_target: None,
            }],
            "",
        )
        .unwrap();

        std::os::unix::fs::symlink("target", repo.join("link")).unwrap();
        let new = write_tree(
            &store,
            repo,
            &[FileEntry {
                path: "link".to_string(),
                executable: false,
                symlink_target: Some("target".to_string()),
            }],
            "",
        )
        .unwrap();

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["old.txt"].status, "D");
        assert_eq!(by_path["link"].status, "A");
        assert!(diff.files.iter().all(|file| !file.status.starts_with('R')));
    }

    #[test]
    fn diff_native_trees_never_leaks_secret_old_path_through_rename() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[(".env", b"API_TOKEN=oldsecret\nshared\n", false)]);
        let new = write_test_tree(
            repo,
            &[("public.txt", b"API_TOKEN=oldsecret\nshared\n", false)],
        );
        let store = NativeObjectStore::new(repo);

        let diff = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap();
        assert!(diff
            .files
            .iter()
            .all(|file| file.old_path.as_deref() != Some(".env")));
        assert!(diff.dropped_secret_paths.contains(&".env".to_string()));
    }

    #[test]
    fn diff_native_trees_skips_inexact_rename_when_limit_exceeded() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("a.txt", b"a\n", false), ("b.txt", b"b\n", false)]);
        let new = write_test_tree(repo, &[("c.txt", b"c\n", false), ("d.txt", b"d\n", false)]);
        let store = NativeObjectStore::new(repo);
        let options = DiffOptions {
            rename_limit: 1,
            ..DiffOptions::default()
        };

        let diff = diff_native_trees(&store, &old, &new, &options).unwrap();
        assert!(diff
            .warnings
            .iter()
            .any(|warning| warning.code == "rename_detection_skipped"));
        assert!(diff.files.iter().all(|file| !file.status.starts_with('R')));
    }

    #[test]
    fn diff_native_tree_corrupt_blob_error_is_path_free() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = write_test_tree(repo, &[("app.txt", b"old\n", false)]);
        let new = write_test_tree(repo, &[("app.txt", b"new\n", false)]);
        let store = NativeObjectStore::new(repo);
        let blob = store.tree_fingerprints(&new).unwrap()["app.txt"].0.clone();
        let blob_id = ObjectId::parse(&blob).unwrap();
        fs::write(store.object_path(&blob_id), b"corrupt").unwrap();

        let error = diff_native_trees(&store, &old, &new, &DiffOptions::default()).unwrap_err();
        let object_path = store.object_path(&blob_id).to_string_lossy().into_owned();
        assert!(!error.to_string().contains(&object_path));
        assert!(!format!("{error:#}").contains(&object_path));
    }

    #[test]
    #[cfg(unix)]
    fn diff_working_vs_tree_uses_policy_filtered_symlink_aware_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let root = write_test_tree(
            repo,
            &[("app.txt", b"old\n", false), ("gone.txt", b"bye\n", false)],
        );
        fs::remove_file(repo.join("gone.txt")).unwrap();
        fs::write(repo.join("app.txt"), b"new\n").unwrap();
        fs::write(repo.join("added.txt"), b"added\n").unwrap();
        fs::write(repo.join(".env"), b"API_TOKEN=secret\n").unwrap();
        std::os::unix::fs::symlink("app.txt", repo.join("link")).unwrap();
        let store = NativeObjectStore::new(repo);
        let root_ref = format!("{FORGE_TREE_PREFIX}{root}");

        let diff = diff_working_vs_tree(&store, repo, &root_ref, &DiffOptions::default()).unwrap();
        let by_path: BTreeMap<_, _> = diff.files.iter().map(|f| (f.path.as_str(), f)).collect();
        assert_eq!(by_path["app.txt"].status, "M");
        assert_eq!(by_path["added.txt"].status, "A");
        assert_eq!(by_path["gone.txt"].status, "D");
        assert_eq!(by_path["link"].status, "A");
        assert!(by_path["link"].hunks.is_empty());
        assert!(!by_path.contains_key(".env"));
    }

    #[test]
    fn wal_sidecars_are_excluded_by_policy() {
        // WAL (enabled in forge-store::open_connection) makes `forge.db` travel
        // with `-wal`/`-shm` sidecars holding committed-but-uncheckpointed data,
        // including evidence excerpts. They must never leak into a snapshot/export.
        assert!(is_ignored_by_policy(".forge/forge.db"));
        assert!(is_ignored_by_policy(".forge/forge.db-wal"));
        assert!(is_ignored_by_policy(".forge/forge.db-shm"));
        // The NER-132 advisory lock file is covered by the same blanket `.forge/`
        // prefix; pin it so a future refactor of the exclusion rule cannot drop it.
        assert!(is_ignored_by_policy(".forge/forge.lock"));
        assert!(is_ignored_by_policy(".forge"));
        // Restore temps live in worktree dirs (NER-132 U4), not under .forge, so they
        // need their own exclusion — an orphan from a crash-interrupted restore must
        // never land in a snapshot/export.
        assert!(is_ignored_by_policy(".forge-restore-abc123"));
        assert!(is_ignored_by_policy("src/nested/.forge-restore-xyz"));
        // Symmetric secret/internal-path assertions: both backends route to the shared
        // `forge_content::is_ignored_by_policy`, so the exclusion set cannot drift
        // (NER-133 U6).
        assert!(is_ignored_by_policy(".env"));
        assert!(is_ignored_by_policy("certs/server.pem"));
        assert!(is_ignored_by_policy(".git"));
        assert!(is_ignored_by_policy(".git/config"));
        // A normal worktree file is still snapshot-eligible.
        assert!(!is_ignored_by_policy("README.md"));
    }

    #[test]
    fn sync_dir_propagates_error_on_missing_path() {
        // Directory fsync rarely fails on a healthy FS (and on macOS a dir fsync is a
        // near-noop), so exercise the error path through a mockable seam: a path that
        // cannot be opened must surface an Err rather than being swallowed.
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");
        let error = sync_dir(&missing).unwrap_err();
        // S1 (NER-143 R8): the error must surface the io::ErrorKind, never the filesystem
        // path, in either the Display or the alternate context-chained `{:#}` form.
        let needle = missing.to_string_lossy();
        assert!(
            !error.to_string().contains(&*needle) && !format!("{error:#}").contains(&*needle),
            "sync_dir leaked a path: {error:#}"
        );
    }

    #[test]
    fn missing_dirs_lists_uncreated_ancestors_shallowest_first() {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = a.join("b");
        let c = b.join("c");
        // temp.path() already exists; a, b, c do not.
        assert_eq!(missing_dirs(&c), vec![a.clone(), b.clone(), c.clone()]);
        fs::create_dir_all(&c).unwrap();
        assert!(missing_dirs(&c).is_empty());
    }

    #[test]
    fn write_object_into_fresh_shard_is_durable_and_roundtrips() {
        // A fresh store means the sha256/<prefix>/ shard dir is newly created, exercising
        // the ancestor-fsync path; the object must round-trip after write_object returns Ok.
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let id = store
            .write_object(ObjectKind::Blob, b"durable-payload")
            .unwrap();
        assert_eq!(store.read_object(&id).unwrap(), b"durable-payload");
    }

    #[test]
    fn restore_roundtrips_atomically_and_leaves_no_temp() {
        // Snapshot a source worktree, then materialize it into a fresh destination
        // and over an existing (stale) file. The crash-atomic file arm uses
        // temp+rename, so on a clean restore: content round-trips, the stale file
        // is fully replaced, and no `.forge-restore-*` temp survives.
        let src = tempfile::tempdir().unwrap();
        // The native walker (NER-138 Phase 7 slice 1) enumerates worktree paths via the
        // `ignore` crate, so a `.git` directory is no longer required to snapshot; the
        // untracked files below are picked up directly from the filesystem.
        fs::create_dir_all(src.path().join("dir")).unwrap();
        fs::write(src.path().join("top.txt"), b"top-new").unwrap();
        fs::write(src.path().join("dir/nested.txt"), b"nested-new").unwrap();
        let backend = NativeContentBackend;
        let content = backend.snapshot_worktree(src.path()).unwrap();

        let dest = tempfile::tempdir().unwrap();
        // A stale file at a target path must be replaced wholesale (never torn).
        fs::write(dest.path().join("top.txt"), b"stale-old-and-longer").unwrap();
        materialize_content_ref(src.path(), dest.path(), &content.content_ref).unwrap();

        assert_eq!(fs::read(dest.path().join("top.txt")).unwrap(), b"top-new");
        assert_eq!(
            fs::read(dest.path().join("dir/nested.txt")).unwrap(),
            b"nested-new"
        );

        // No restore temp may linger in the destination tree after a clean restore.
        let mut leftover = Vec::new();
        for dir in [dest.path().to_path_buf(), dest.path().join("dir")] {
            for entry in fs::read_dir(&dir).unwrap() {
                let name = entry.unwrap().file_name().to_string_lossy().into_owned();
                if name.starts_with(RESTORE_TEMP_PREFIX) {
                    leftover.push(name);
                }
            }
        }
        assert!(
            leftover.is_empty(),
            "restore left orphaned temp files: {leftover:?}"
        );
    }

    // --- NER-138 Phase 7 slice 1: native walker differential harness ---
    //
    // These tests prove the native `ignore`-crate walker's snapshot set equals the prior
    // git-based set across a parity corpus (incl. secret-risk exclusion), and assert each
    // index-vs-filesystem divergence class explicitly rather than masking it. The harness
    // is the safety net that justifies removing the `git ls-files` shell-out from the
    // native snapshot path (R5). Most are git-backed; the `.forgeignore` and special-byte
    // cases are native-only (git knows nothing of `.forgeignore`, and the special-byte
    // case is exactly the C-quote leak the native walker structurally cures).

    /// Run a git command in `repo`, asserting success. Test setup only.
    #[cfg(test)]
    fn run_git(repo: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap()
                .status
                .success(),
            "git {args:?} failed"
        );
    }

    /// Run a git command and capture its stdout. Lives ONLY in the test module (already
    /// `#[cfg(test)]`): the `git_based_scan` differential harness shells git to reproduce
    /// the prior git-based set, but NER-138 Phase 7 slice 2 removed the last production
    /// caller — native snapshot/base/changed_paths are git-binary-free (pinned by
    /// `native_production_paths_shell_no_git`).
    fn git(repo_root: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_root)
            .output()
            .with_context(|| format!("failed to run git {args:?}"))?;
        if !output.status.success() {
            bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8(output.stdout)?)
    }

    #[cfg(test)]
    fn init_git_repo(repo: &Path) {
        run_git(repo, &["init"]);
        run_git(repo, &["config", "user.email", "t@example.com"]);
        run_git(repo, &["config", "user.name", "forge-test"]);
    }

    /// The pre-slice-1 git-based candidate enumeration, retained ONLY as the differential
    /// harness's reference set. Mirrors the removed `snapshot_candidate_paths` (the union of
    /// `git ls-files` and `git ls-files --others --exclude-standard`) followed by the same
    /// downstream filters `scan_worktree` applies (`is_ignored_by_policy` + `is_file`).
    /// Production no longer shells git for snapshotting (R1); this lives in the test module
    /// so the harness can prove native-walk set == prior git-based set.
    #[cfg(test)]
    fn git_based_scan(repo: &Path) -> Vec<String> {
        let mut candidates = BTreeSet::new();
        for args in [
            ["ls-files"].as_slice(),
            ["ls-files", "--others", "--exclude-standard"].as_slice(),
        ] {
            candidates.extend(git(repo, args).unwrap().lines().map(str::to_string));
        }
        let mut files: Vec<String> = candidates
            .into_iter()
            .filter(|path| !is_ignored_by_policy(path))
            .filter(|path| matches!(fs::metadata(repo.join(path)), Ok(meta) if meta.is_file()))
            .collect();
        files.sort();
        files
    }

    /// The native walker's final scan set (post policy backstop + `is_file` gate), sorted.
    #[cfg(test)]
    fn native_scan(repo: &Path) -> Vec<String> {
        let mut files: Vec<String> = scan_worktree(repo)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect();
        files.sort();
        files
    }

    #[test]
    fn native_walk_matches_git_based_set_on_parity_corpus() {
        // Parity corpus: only paths whose membership is identical between git's index-based
        // enumeration and a filesystem walk (no index-only paths — those are asserted as
        // divergences below). Includes tracked, untracked, gitignored (+ negation),
        // secret-risk, internal, and restore-temp-at-depth cases.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);

        fs::create_dir_all(repo.join("src/nested")).unwrap();
        fs::write(repo.join("README.md"), b"readme").unwrap();
        fs::write(repo.join("src/main.rs"), b"fn main() {}").unwrap();
        fs::write(repo.join("src/nested/deep.txt"), b"deep").unwrap();
        fs::write(repo.join(".gitattributes"), b"* text=auto\n").unwrap();
        // *.log ignored, but keep.log re-included by a negation (parent not excluded).
        fs::write(repo.join(".gitignore"), b"*.log\n!keep.log\n").unwrap();
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "-m", "init"]);

        // Untracked, non-ignored.
        fs::write(repo.join("untracked.txt"), b"u").unwrap();
        // Untracked + gitignored (excluded) and the negated re-include (kept).
        fs::write(repo.join("debug.log"), b"d").unwrap();
        fs::write(repo.join("keep.log"), b"k").unwrap();
        // Secret-risk + internal: excluded by is_ignored_by_policy in BOTH pipelines.
        fs::write(repo.join(".env"), b"SECRET=x").unwrap();
        fs::create_dir_all(repo.join("certs")).unwrap();
        fs::write(repo.join("certs/server.pem"), b"key").unwrap();
        fs::create_dir_all(repo.join(".forge")).unwrap();
        fs::write(repo.join(".forge/forge.db"), b"db").unwrap();
        // Orphaned restore temp at depth (policy-excluded at any depth, not via .gitignore).
        fs::write(repo.join("src/nested/.forge-restore-abc"), b"tmp").unwrap();

        assert_eq!(
            native_scan(repo),
            git_based_scan(repo),
            "native walk set must equal the prior git-based set on the parity corpus"
        );

        // Pin the content the equality locks in.
        let native = native_scan(repo);
        for kept in [
            "README.md",
            "src/main.rs",
            "src/nested/deep.txt",
            ".gitattributes",
            ".gitignore",
            "untracked.txt",
            "keep.log",
        ] {
            assert!(
                native.contains(&kept.to_string()),
                "expected {kept} in {native:?}"
            );
        }
        for dropped in [".env", "certs/server.pem", "debug.log", ".forge/forge.db"] {
            assert!(
                !native.contains(&dropped.to_string()),
                "unexpected {dropped} in {native:?}"
            );
        }
        assert!(
            !native.iter().any(|p| p.contains(".forge-restore-")),
            "restore temp leaked into snapshot set: {native:?}"
        );
    }

    #[test]
    fn walk_does_not_recurse_into_git_or_forge() {
        // filter_entry must prune `.git`/`.forge` descent so the walk never yields their
        // internals (a real `.git` holds thousands of files; statting them every save is
        // pure waste, and they must never reach the snapshot anyway).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join("README.md"), b"x").unwrap();
        fs::create_dir_all(repo.join(".forge/objects/sha256/ab")).unwrap();
        fs::write(repo.join(".forge/forge.db"), b"db").unwrap();
        fs::write(repo.join(".forge/objects/sha256/ab/deadbeef"), b"obj").unwrap();

        let walked = walk_worktree(repo).unwrap();
        assert!(
            walked.iter().all(|p| {
                p != ".git" && !p.starts_with(".git/") && p != ".forge" && !p.starts_with(".forge/")
            }),
            "walk recursed into .git/ or .forge/: {walked:?}"
        );
        assert!(walked.contains(&"README.md".to_string()));
    }

    #[test]
    fn force_added_gitignored_file_is_index_only_divergence() {
        // Divergence class 1: git's index lists a force-added (`add -f`) file even though
        // .gitignore matches it; the native filesystem walk drops it (no index concept).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join(".gitignore"), b"forced.bin\n").unwrap();
        fs::write(repo.join("forced.bin"), b"x").unwrap();
        run_git(repo, &["add", "-f", "forced.bin"]);
        run_git(repo, &["add", ".gitignore"]);
        run_git(repo, &["commit", "-m", "force"]);

        assert!(
            git_based_scan(repo).contains(&"forced.bin".to_string()),
            "git index lists the force-added file"
        );
        assert!(
            !native_scan(repo).contains(&"forced.bin".to_string()),
            "native filesystem walk drops the gitignored file (no index concept)"
        );
        // The force-added path is the ONLY difference: the two scans agree on everything else.
        let strip = |set: Vec<String>| -> Vec<String> {
            set.into_iter().filter(|p| p != "forced.bin").collect()
        };
        assert_eq!(strip(native_scan(repo)), strip(git_based_scan(repo)));
    }

    #[test]
    fn tracked_then_later_ignored_file_is_index_only_divergence() {
        // Divergence class 2: a normally-committed file later matched by an added
        // .gitignore rule — git's ls-files still lists it (tracked); native walk drops it.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join("data.gen"), b"x").unwrap();
        run_git(repo, &["add", "data.gen"]);
        run_git(repo, &["commit", "-m", "track"]);
        fs::write(repo.join(".gitignore"), b"*.gen\n").unwrap();

        assert!(git_based_scan(repo).contains(&"data.gen".to_string()));
        assert!(!native_scan(repo).contains(&"data.gen".to_string()));
        // The now-ignored tracked path is the ONLY difference between the two scans.
        let strip = |set: Vec<String>| -> Vec<String> {
            set.into_iter().filter(|p| p != "data.gen").collect()
        };
        assert_eq!(strip(native_scan(repo)), strip(git_based_scan(repo)));
    }

    #[test]
    fn tracked_then_deleted_from_disk_converges_after_metadata_gate() {
        // Divergence class 3: git's raw `ls-files` lists a tracked path even after it is
        // deleted from the worktree (still in the index); the native walk cannot see a
        // nonexistent file. Both *scan* pipelines converge because the `fs::metadata`
        // `is_file` gate drops the now-missing path from the git reference too.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join("gone.txt"), b"x").unwrap();
        fs::write(repo.join("stay.txt"), b"y").unwrap();
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "-m", "add"]);
        fs::remove_file(repo.join("gone.txt")).unwrap();

        assert!(
            git(repo, &["ls-files"])
                .unwrap()
                .lines()
                .any(|l| l == "gone.txt"),
            "git index still lists the deleted path"
        );
        assert!(!native_scan(repo).contains(&"gone.txt".to_string()));
        // After the metadata gate the two scans agree (both keep stay.txt, drop gone.txt).
        assert_eq!(native_scan(repo), git_based_scan(repo));
    }

    #[test]
    fn walk_is_environment_independent_of_info_exclude() {
        // Divergence class 4 (load-bearing for Phase 7): the native walker is intentionally
        // MORE inclusive than `git ls-files --others --exclude-standard` — it ignores
        // `.git/info/exclude` and the user's global core.excludesfile so a repo's snapshot
        // set is reproducible across machines. This pins that git_exclude(false)/git_global(false)
        // are real: a path excluded ONLY via `.git/info/exclude` is dropped by git but KEPT by
        // the native walk. If a crate upgrade or edit flipped those toggles, this catches it.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join("keep-me.txt"), b"x").unwrap();
        fs::write(repo.join(".git/info/exclude"), b"keep-me.txt\n").unwrap();

        let git_others = git(repo, &["ls-files", "--others", "--exclude-standard"]).unwrap();
        assert!(
            !git_others.lines().any(|l| l == "keep-me.txt"),
            "git --exclude-standard drops a .git/info/exclude path"
        );
        assert!(
            native_scan(repo).contains(&"keep-me.txt".to_string()),
            "native walk must ignore .git/info/exclude (reproducibility): {:?}",
            native_scan(repo)
        );
    }

    // Divergence classes intentionally NOT asserted, with rationale (plan R5/U3):
    //   • Case-folded `.gitignore` on a case-insensitive filesystem (macOS): a rule like
    //     `SECRET.txt` drops `secret.txt` under git's core.ignorecase, but the native walk
    //     matches case-sensitively, so membership can differ. Asserting it would be
    //     platform-dependent (green on Linux CI, divergent on macOS), so it is documented as
    //     an accepted divergence rather than pinned by a flaky test. Secret hygiene is
    //     unaffected: `is_secret_risk_path` lowercases the filename, so a case-variant secret
    //     name is still excluded by the policy backstop.
    //   • Submodule gitlinks: `git ls-files` lists a gitlink path; a filesystem walk
    //     descends/skips it differently. A real submodule fixture in a unit test is
    //     impractical (a second repo + offline `submodule add`), so this class is scoped out
    //     by comment per the plan's allowance.

    #[test]
    fn nested_subdir_gitignore_is_honored() {
        // R2's headline claim is "nested, with negation". A .gitignore in a SUBDIRECTORY
        // (not just the root) must scope to that directory — the deceptively-hard part the
        // `ignore` crate exists to handle. Pin it so a crate-version bump can't regress it.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/.gitignore"), b"local.tmp\n!keep.tmp\n").unwrap();
        fs::write(repo.join("src/local.tmp"), b"x").unwrap();
        fs::write(repo.join("src/keep.tmp"), b"x").unwrap();
        fs::write(repo.join("src/main.rs"), b"x").unwrap();
        fs::write(repo.join("local.tmp"), b"x").unwrap(); // root: NOT covered by src/.gitignore

        let native = native_scan(repo);
        assert!(
            !native.contains(&"src/local.tmp".to_string()),
            "nested .gitignore must exclude src/local.tmp: {native:?}"
        );
        assert!(
            native.contains(&"src/keep.tmp".to_string()),
            "nested negation must re-include src/keep.tmp: {native:?}"
        );
        assert!(native.contains(&"src/main.rs".to_string()));
        assert!(
            native.contains(&"local.tmp".to_string()),
            "root local.tmp is outside src/.gitignore's scope: {native:?}"
        );
        // The nested rule is repo-local, so native and git agree.
        assert_eq!(native, git_based_scan(repo));
    }

    #[test]
    fn empty_worktree_snapshots_and_roundtrips() {
        // A fresh dir with no files: walk_worktree returns empty, snapshot_worktree builds an
        // empty root tree, and the snapshot materializes. No git binary needed for the walk.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert!(walk_worktree(repo).unwrap().is_empty());
        let content = NativeContentBackend
            .snapshot_worktree(repo)
            .expect("empty worktree snapshots");
        assert!(content.content_ref.starts_with(FORGE_TREE_PREFIX));
        let dest = tempfile::tempdir().unwrap();
        materialize_content_ref(repo, dest.path(), &content.content_ref)
            .expect("empty snapshot materializes");
    }

    #[cfg(unix)]
    #[test]
    fn tracked_symlink_to_file_is_captured() {
        // The walker yields symlinks (a symlink's own file_type is is_symlink, so a walk-layer
        // is_file() filter would drop it); scan_worktree's symlink_metadata gate then captures
        // it as a 120000 entry. Path-set parity with git holds: git ls-files lists the link and
        // git_based_scan's is_file() (which follows) keeps a symlink-to-file.
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        init_git_repo(repo);
        fs::write(repo.join("target.txt"), b"content").unwrap();
        symlink("target.txt", repo.join("link.txt")).unwrap();
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "-m", "link"]);

        let native = native_scan(repo);
        assert!(
            native.contains(&"link.txt".to_string()),
            "tracked symlink-to-file must be captured: {native:?}"
        );
        assert!(native.contains(&"target.txt".to_string()));
        assert_eq!(native, git_based_scan(repo));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_round_trips_as_a_link_not_a_regular_file() {
        // NER-138 slice 3: a symlink snapshots as a 120000 object (its target) and restores as
        // a SYMLINK with the identical target — not a regular file whose content is the target
        // text. A regular file containing the same bytes is a DISTINCT object + diff entry.
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("target.txt"), b"hello").unwrap();
        symlink("target.txt", repo.join("link.txt")).unwrap();
        // A regular file whose CONTENT equals the symlink's target string.
        fs::write(repo.join("decoy.txt"), b"target.txt").unwrap();

        let content = NativeContentBackend
            .snapshot_worktree(repo)
            .expect("snapshot with a symlink");

        let dest = tempfile::tempdir().unwrap();
        // Objects live under `repo`; files materialize into `dest` (which is also the worktree
        // root the R15 target validation resolves against).
        materialize_content_ref(repo, dest.path(), &content.content_ref)
            .expect("materialize symlink");
        let restored = dest.path().join("link.txt");
        let meta = fs::symlink_metadata(&restored).expect("restored link exists");
        assert!(
            meta.file_type().is_symlink(),
            "must restore as a symlink, not a regular file"
        );
        assert_eq!(fs::read_link(&restored).unwrap(), Path::new("target.txt"));

        // The symlink and the same-bytes regular file are distinct (mode in the diff key):
        // their tree entries carry different modes (120000 vs 100644) even though one's blob
        // content equals the other's target string.
        let root = object_id_from_content_ref(&content.content_ref).unwrap();
        let store = NativeObjectStore::new(repo);
        let flat = flatten_tree(&store, &root).unwrap();
        let (_, link_mode) = flat.get("link.txt").expect("link.txt in tree");
        let (_, decoy_mode) = flat.get("decoy.txt").expect("decoy.txt in tree");
        assert_eq!(*link_mode, 0o120000, "symlink entry is mode 120000");
        assert_eq!(*decoy_mode, 0o100644, "regular file entry is mode 100644");
    }

    #[cfg(unix)]
    #[test]
    fn materializing_an_escaping_symlink_is_rejected() {
        // R15 (security P0): a materialized symlink must not escape the worktree. A captured
        // target of `../../etc/passwd` or an absolute `/etc/passwd` is rejected at materialize
        // (path-free error); a safe relative target within the worktree materializes.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        // Escaping (relative) and absolute targets are rejected.
        for bad in ["../../etc/passwd", "/etc/passwd"] {
            let error = validate_symlink_target(repo, "sub/link", bad).unwrap_err();
            let rendered = format!("{error:#}");
            assert!(
                !rendered.contains("etc/passwd") && !rendered.contains(repo.to_str().unwrap()),
                "S1: rejection must be path-free: {rendered}"
            );
        }
        // A safe relative target within the worktree is accepted.
        validate_symlink_target(repo, "sub/link", "../README.md").expect("safe relative target");
        validate_symlink_target(repo, "link", "target.txt").expect("sibling target");
    }

    #[test]
    fn native_walk_excludes_secret_with_special_byte_in_name() {
        // The structural cure for the C-quote class (Phase 6 §5): the native walker passes
        // the REAL filename to is_secret_risk_path, never a git-C-quoted string. `.env.café`
        // matches starts_with(".env.") on its real name and is excluded; git ls-tree/ls-files
        // would C-quote it (`".env.caf\303\251"`) and the leading quote would defeat the
        // prefix match — the leak this design avoids. No git repo needed (require_git(false)).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join(".env.café"), b"SECRET=1").unwrap();
        fs::write(repo.join("plain.txt"), b"x").unwrap();
        let native = native_scan(repo);
        assert!(
            !native.iter().any(|p| p.contains(".env.caf")),
            "special-byte secret name must be excluded: {native:?}"
        );
        assert!(native.contains(&"plain.txt".to_string()));
    }

    #[test]
    fn walk_error_is_path_free_and_skips_not_found() {
        // S1: a walk error must never leak a filesystem path (which would bypass typed-error
        // redaction). `ignore::Error`'s own Display embeds the path, so map_walk_error emits
        // only the path-free io::ErrorKind. A NotFound is benign (mid-walk delete) → skipped.
        use std::io::{Error as IoError, ErrorKind};
        let secret = PathBuf::from("/tmp/.env.supersecret-leak");

        let perm = ignore::Error::WithPath {
            path: secret.clone(),
            err: Box::new(ignore::Error::Io(IoError::new(
                ErrorKind::PermissionDenied,
                secret.display().to_string(),
            ))),
        };
        let mapped = map_walk_error(&perm).expect("non-NotFound walk errors surface");
        let top = mapped.to_string();
        let chain = format!("{mapped:#}");
        assert!(
            !top.contains(".env.supersecret") && !chain.contains(".env.supersecret"),
            "S1: walk error leaked a path: top={top:?} chain={chain:?}"
        );
        assert!(top.contains("permission denied"));

        let gone = ignore::Error::WithPath {
            path: secret,
            err: Box::new(ignore::Error::Io(IoError::from(ErrorKind::NotFound))),
        };
        assert!(
            map_walk_error(&gone).is_none(),
            "NotFound is benign and skipped, not surfaced as an error"
        );
    }

    // --- NER-138 Phase 7 slice 1: .forgeignore precedence (native-only) ---

    #[test]
    fn forgeignore_excludes_paths_gitignore_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join(".forgeignore"), b"*.tmp\n").unwrap();
        fs::write(repo.join("scratch.tmp"), b"x").unwrap();
        fs::write(repo.join("keep.txt"), b"x").unwrap();
        let walked = walk_worktree(repo).unwrap();
        assert!(
            !walked.contains(&"scratch.tmp".to_string()),
            ".forgeignore *.tmp must exclude scratch.tmp: {walked:?}"
        );
        assert!(walked.contains(&"keep.txt".to_string()));
    }

    #[test]
    fn forgeignore_negation_reincludes_gitignored_path() {
        // .forgeignore has higher precedence than .gitignore (add_custom_ignore_filename),
        // so a ! negation there re-includes a path .gitignore excluded.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join(".gitignore"), b"*.log\n").unwrap();
        fs::write(repo.join(".forgeignore"), b"!keep.log\n").unwrap();
        fs::write(repo.join("debug.log"), b"x").unwrap();
        fs::write(repo.join("keep.log"), b"x").unwrap();
        let walked = walk_worktree(repo).unwrap();
        assert!(
            !walked.contains(&"debug.log".to_string()),
            ".gitignore still excludes debug.log: {walked:?}"
        );
        assert!(
            walked.contains(&"keep.log".to_string()),
            ".forgeignore ! must re-include keep.log (higher precedence): {walked:?}"
        );
    }

    #[test]
    fn forgeignore_cannot_reinclude_policy_excluded_secret() {
        // The is_ignored_by_policy backstop runs AFTER the ignore engine and is not
        // negatable: even an explicit !.env in .forgeignore cannot re-include a secret.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join(".forgeignore"), b"!.env\n").unwrap();
        fs::write(repo.join(".env"), b"SECRET=1").unwrap();
        fs::write(repo.join("ok.txt"), b"x").unwrap();
        let native = native_scan(repo);
        assert!(
            !native.iter().any(|p| p == ".env"),
            ".env must stay excluded by the always-wins policy backstop: {native:?}"
        );
        assert!(native.contains(&"ok.txt".to_string()));
    }

    // --- NER-138 Phase 7 slice 2: native commit objects, ref store, base anchoring ---

    #[test]
    fn commit_object_roundtrips_and_is_recovered_by_all_object_ids() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        // Genesis shape: empty parents, all-None justification (what slice 2 writes).
        let genesis = CommitObject {
            schema_version: SCHEMA_VERSION,
            tree: format!("f1:tree:sha256:{}", "a".repeat(64)),
            parents: Vec::new(),
            intent_id: None,
            proposal_revision_id: None,
            decision_id: None,
            evidence_digest: None,
            actor: None,
            authored_time: None,
        };
        let gid = store.write_commit(&genesis).unwrap();
        assert!(gid.to_string().starts_with("f1:commit:sha256:"));
        assert_eq!(store.read_commit(&gid).unwrap(), genesis);
        // Fully-populated (justified) shape — exercises every field including the slice-3
        // actor + authored_time in the hashed bytes. evidence_digest is a Hex64, so excerpt
        // text is structurally unrepresentable.
        let justified = CommitObject {
            schema_version: SCHEMA_VERSION,
            tree: format!("f1:tree:sha256:{}", "b".repeat(64)),
            parents: vec![gid.to_string()],
            intent_id: Some("intent_x".to_string()),
            proposal_revision_id: Some("revision_x".to_string()),
            decision_id: Some("decision_x".to_string()),
            evidence_digest: Some(Hex64::new("c".repeat(64)).unwrap()),
            actor: Some("agent:tester".to_string()),
            authored_time: Some(1_700_000_000_000),
        };
        let jid = store.write_commit(&justified).unwrap();
        assert_eq!(store.read_commit(&jid).unwrap(), justified);
        assert!(
            justified
                .evidence_digest
                .as_ref()
                .unwrap()
                .as_str()
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "evidence_digest is an opaque hex digest, never excerpt text"
        );
        // The triple-kind all_object_ids scan recovers both commit ids.
        let ids = store.all_object_ids().unwrap();
        assert!(ids.contains(&gid) && ids.contains(&jid));
    }

    /// GENESIS-HASH STABILITY (slice 3, critical): adding `actor`/`authored_time`
    /// (skip_serializing_if) and retyping `evidence_digest` to `Hex64` must NOT change the
    /// bytes a genesis commit serializes to — otherwise every existing native repo's
    /// `base_head` desyncs into spurious `STALE_BASE`. The expected JSON is a hard-coded
    /// literal of what slice 2 wrote (NOT recomputed from the struct), per the adversarial
    /// doc-review finding. Because the `ObjectId` is `hash(preimage(these bytes))`, equal
    /// bytes ⇒ equal id — so byte-equality is the genesis-stability proof.
    #[test]
    fn genesis_commit_serialization_is_byte_identical_to_slice_2() {
        let tree = format!("f1:tree:sha256:{}", "0".repeat(64));
        // EXACTLY what a slice-2 genesis serialized to: 7 fields, the 4 justification
        // Options as null, no actor/authored_time keys.
        let expected = format!(
            r#"{{"schema_version":1,"tree":"{tree}","parents":[],"intent_id":null,"proposal_revision_id":null,"decision_id":null,"evidence_digest":null}}"#
        );
        let genesis = CommitObject {
            schema_version: SCHEMA_VERSION,
            tree: tree.clone(),
            parents: Vec::new(),
            intent_id: None,
            proposal_revision_id: None,
            decision_id: None,
            evidence_digest: None,
            actor: None,
            authored_time: None,
        };
        let serialized = serde_json::to_string(&genesis).unwrap();
        assert_eq!(
            serialized, expected,
            "genesis serialization drifted — existing repos' base_head would desync"
        );
        // Pin the FULL content-addressed id against a hard-coded literal (not just a prefix),
        // so a change to the preimage framing (object_preimage / ObjectId::new) — not only the
        // CommitObject shape — also fails loudly. This literal is the deterministic genesis id
        // for the all-zeros tree above; it must never change, or existing repos' base_head
        // desyncs into spurious STALE_BASE.
        let id = ObjectId::new(ObjectKind::Commit, serialized.as_bytes()).to_string();
        assert_eq!(
            id, "f1:commit:sha256:cf31029e040659af09e1dd6f323b3dcd76db2bf9a2d5c639b2a588d8a9fa809e",
            "genesis commit id changed — preimage framing or commit shape drifted"
        );
    }

    /// The slice-3 `actor`/`authored_time` are in the HASHED bytes (Phase 9 signs who/when):
    /// two justified commits identical except `actor` MUST have distinct ids, and a
    /// justified commit MUST differ from the genesis-shaped commit over the same tree.
    #[test]
    fn justified_commit_fields_are_hashed() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let base = CommitObject {
            schema_version: SCHEMA_VERSION,
            tree: format!("f1:tree:sha256:{}", "a".repeat(64)),
            parents: vec![format!("f1:commit:sha256:{}", "b".repeat(64))],
            intent_id: Some("intent_x".to_string()),
            proposal_revision_id: Some("revision_x".to_string()),
            decision_id: Some("decision_x".to_string()),
            evidence_digest: Some(Hex64::new("c".repeat(64)).unwrap()),
            actor: Some("agent:alice".to_string()),
            authored_time: Some(1_700_000_000_000),
        };
        let mut other_actor = base.clone();
        other_actor.actor = Some("agent:bob".to_string());
        let mut other_time = base.clone();
        other_time.authored_time = Some(1_700_000_000_001);
        let id_base = store.write_commit(&base).unwrap();
        let id_actor = store.write_commit(&other_actor).unwrap();
        let id_time = store.write_commit(&other_time).unwrap();
        assert_ne!(id_base, id_actor, "actor must be in the hashed bytes");
        assert_ne!(
            id_base, id_time,
            "authored_time must be in the hashed bytes"
        );
    }

    /// `Hex64` rejects anything that is not exactly 64 lowercase-hex chars, so excerpt text
    /// can never reach `CommitObject::evidence_digest` (the secret-hygiene guard).
    #[test]
    fn hex64_rejects_non_digest() {
        assert!(Hex64::new("c".repeat(64)).is_ok());
        assert!(Hex64::new("not a real digest, this is excerpt text").is_err());
        assert!(Hex64::new("C".repeat(64)).is_err(), "uppercase rejected");
        assert!(Hex64::new("c".repeat(63)).is_err(), "wrong length rejected");
        // Error is path-free (S1): no separators leak from the validation message.
        let err = Hex64::new("zz").unwrap_err().to_string();
        assert!(!err.contains('/') && !err.contains('\\'));
    }

    /// NER-138 slice 3 U4: object-kind headers coexist with legacy raw-payload objects, and
    /// the headered-vs-legacy decision is hash-resolved (prove-before-delete: the header path
    /// enumerates/reads the same id set the triple-hash scan would, across a mixed store).
    #[test]
    fn object_kind_headers_coexist_with_legacy_objects() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());

        // (1) A slice-3 headered object via write_object: the file IS the preimage.
        let headered_id = store
            .write_object(ObjectKind::Blob, b"headered payload")
            .unwrap();
        let on_disk = fs::read(store.object_path(&headered_id)).unwrap();
        assert!(
            on_disk.starts_with(OBJECT_MAGIC),
            "new writes store the self-describing preimage"
        );
        assert_eq!(
            store.read_object(&headered_id).unwrap(),
            b"headered payload"
        );

        // (2) A legacy slice-1/2 object: same id (over the framed preimage), but the FILE is
        // the RAW payload — planted directly to simulate what slice 1/2 wrote.
        let legacy_payload = b"legacy raw payload".to_vec();
        let legacy_id = ObjectId::new(ObjectKind::Blob, &legacy_payload);
        let legacy_path = store.object_path(&legacy_id);
        fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        fs::write(&legacy_path, &legacy_payload).unwrap();
        assert_eq!(
            store.read_object(&legacy_id).unwrap(),
            legacy_payload,
            "legacy raw-payload object reads via the fallback, never a hash mismatch"
        );

        // (3) Differential: the header-aware all_object_ids recovers BOTH.
        let ids = store.all_object_ids().unwrap();
        assert!(ids.contains(&headered_id), "headered object enumerated");
        assert!(ids.contains(&legacy_id), "legacy object enumerated");

        // (4) Hash-resolved disambiguation (adversarial finding): a legacy blob whose RAW
        // content starts with the magic and even parses as a preimage still reads correctly,
        // because hash(file) != a headered id, so it falls back to the legacy branch.
        let tricky_payload = b"forge-object\nblob\n1\n0\n".to_vec();
        let tricky_id = ObjectId::new(ObjectKind::Blob, &tricky_payload);
        let tricky_path = store.object_path(&tricky_id);
        fs::create_dir_all(tricky_path.parent().unwrap()).unwrap();
        fs::write(&tricky_path, &tricky_payload).unwrap();
        assert_eq!(store.read_object(&tricky_id).unwrap(), tricky_payload);
        assert!(store.all_object_ids().unwrap().contains(&tricky_id));
    }

    #[test]
    fn packed_blob_tree_and_commit_objects_are_readable() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let blob_payload = b"packed blob".to_vec();
        let tree_payload = serde_json::to_vec(&TreeObject {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        })
        .unwrap();
        let commit_payload = serde_json::to_vec(&CommitObject {
            schema_version: COMMIT_SCHEMA_VERSION,
            tree: ObjectId::new(ObjectKind::Tree, &tree_payload).to_string(),
            parents: Vec::new(),
            intent_id: None,
            proposal_revision_id: None,
            decision_id: None,
            evidence_digest: None,
            actor: None,
            authored_time: None,
        })
        .unwrap();
        let blob = ObjectId::new(ObjectKind::Blob, &blob_payload);
        let tree = ObjectId::new(ObjectKind::Tree, &tree_payload);
        let commit = ObjectId::new(ObjectKind::Commit, &commit_payload);

        pack::write_test_pack(
            temp.path(),
            "pack-read",
            &[
                (
                    blob.clone(),
                    object_preimage(ObjectKind::Blob, &blob_payload),
                ),
                (
                    tree.clone(),
                    object_preimage(ObjectKind::Tree, &tree_payload),
                ),
                (
                    commit.clone(),
                    object_preimage(ObjectKind::Commit, &commit_payload),
                ),
            ],
        )
        .unwrap();

        assert_eq!(store.read_object(&blob).unwrap(), blob_payload);
        assert_eq!(store.read_object(&tree).unwrap(), tree_payload);
        assert_eq!(store.read_object(&commit).unwrap(), commit_payload);
        let ids = store.all_object_ids().unwrap();
        assert!(ids.contains(&blob));
        assert!(ids.contains(&tree));
        assert!(ids.contains(&commit));
    }

    #[test]
    fn corrupt_packed_bytes_fail_closed_without_paths() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"packed corrupt".to_vec();
        let id = ObjectId::new(ObjectKind::Blob, &payload);
        pack::write_test_pack(
            temp.path(),
            "pack-corrupt",
            &[(id.clone(), object_preimage(ObjectKind::Blob, &payload))],
        )
        .unwrap();
        let pack_path = pack::test_pack_data_path(temp.path(), "pack-corrupt");
        let mut bytes = fs::read(&pack_path).unwrap();
        bytes[0] ^= 0xff;
        fs::write(&pack_path, bytes).unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"));
        let rendered = format!("{error:#}");
        assert!(!rendered.contains(pack_path.to_string_lossy().as_ref()));
    }

    #[test]
    fn packed_wrong_kind_frame_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"wrong kind".to_vec();
        let tree_frame = object_preimage(ObjectKind::Tree, &payload);
        let id = ObjectId {
            kind: "blob".to_string(),
            digest: hex_lower(&Sha256::digest(&tree_frame)),
        };
        pack::write_test_pack(temp.path(), "pack-kind", &[(id.clone(), tree_frame)]).unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error
            .to_string()
            .contains("packed native object kind header mismatch"));
    }

    #[test]
    fn all_object_ids_enumerates_loose_and_packed_deduped() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let loose = store
            .write_object(ObjectKind::Blob, b"loose and packed")
            .unwrap();
        let packed_only_payload = b"packed only".to_vec();
        let packed_only = ObjectId::new(ObjectKind::Blob, &packed_only_payload);
        pack::write_test_pack(
            temp.path(),
            "pack-ids",
            &[
                (
                    loose.clone(),
                    object_preimage(ObjectKind::Blob, b"loose and packed"),
                ),
                (
                    packed_only.clone(),
                    object_preimage(ObjectKind::Blob, &packed_only_payload),
                ),
            ],
        )
        .unwrap();

        let ids = store.all_object_ids().unwrap();
        assert!(ids.contains(&loose));
        assert!(ids.contains(&packed_only));
        assert_eq!(
            ids.iter().filter(|candidate| *candidate == &loose).count(),
            1
        );
    }

    #[test]
    fn corrupt_loose_duplicate_does_not_fallback_to_valid_pack() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"duplicate".to_vec();
        let id = store.write_object(ObjectKind::Blob, &payload).unwrap();
        pack::write_test_pack(
            temp.path(),
            "pack-duplicate",
            &[(id.clone(), object_preimage(ObjectKind::Blob, &payload))],
        )
        .unwrap();
        fs::write(store.object_path(&id), b"corrupt loose").unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error.to_string().contains("hash mismatch"));
    }

    #[test]
    fn pack_index_rejects_pack_id_that_does_not_match_filename() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"pack id mismatch".to_vec();
        let id = ObjectId::new(ObjectKind::Blob, &payload);
        let packs_dir = temp.path().join(".forge/packs");
        fs::create_dir_all(&packs_dir).unwrap();
        fs::write(packs_dir.join("safe.fpack"), b"not used").unwrap();
        fs::write(
            packs_dir.join("safe.fidx"),
            serde_json::to_vec(&pack::PackIndex {
                schema_version: 1,
                pack_id: "../escape".to_string(),
                entries: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error.to_string().contains("malformed native pack id"));
    }

    #[test]
    fn pack_index_range_must_fit_pack_file_before_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"range".to_vec();
        let id = ObjectId::new(ObjectKind::Blob, &payload);
        let packs_dir = temp.path().join(".forge/packs");
        fs::create_dir_all(&packs_dir).unwrap();
        fs::write(packs_dir.join("range.fpack"), b"x").unwrap();
        fs::write(
            packs_dir.join("range.fidx"),
            serde_json::to_vec(&pack::PackIndex {
                schema_version: 1,
                pack_id: "range".to_string(),
                entries: vec![pack::PackEntry {
                    object_id: id.to_string(),
                    offset: 0,
                    framed_len: 1,
                    compressed_len: 1024,
                    checksum: "0".repeat(64),
                    packed_at_ms: None,
                    loose_mtime_ms: None,
                }],
            })
            .unwrap(),
        )
        .unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error
            .to_string()
            .contains("native pack index range exceeds pack length"));
    }

    #[test]
    fn malformed_pack_indexes_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let id = ObjectId::new(ObjectKind::Blob, b"missing");
        let packs_dir = temp.path().join(".forge/packs");
        fs::create_dir_all(&packs_dir).unwrap();
        fs::write(packs_dir.join("bad-json.fidx"), b"{ not json").unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(!format!("{error:#}").contains(temp.path().to_string_lossy().as_ref()));

        fs::remove_file(packs_dir.join("bad-json.fidx")).unwrap();
        fs::write(
            packs_dir.join("future.fidx"),
            serde_json::json!({
                "schema_version": 99,
                "pack_id": "future",
                "entries": []
            })
            .to_string(),
        )
        .unwrap();

        let error = store.read_object(&id).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported native pack index schema version"));

        fs::remove_file(packs_dir.join("future.fidx")).unwrap();
        fs::write(
            packs_dir.join("invalid-id.fidx"),
            serde_json::json!({
                "schema_version": 1,
                "pack_id": "invalid-id",
                "entries": [{
                    "object_id": "not-an-object-id",
                    "offset": 0,
                    "framed_len": 0,
                    "compressed_len": 0,
                    "checksum": "0".repeat(64)
                }]
            })
            .to_string(),
        )
        .unwrap();

        let error = store.all_object_ids().unwrap_err();
        assert!(error.to_string().contains("malformed native object id"));
    }

    #[test]
    fn packed_data_validation_covers_decompression_length_and_frame_errors() {
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        let payload = b"packed branches".to_vec();
        let id = ObjectId::new(ObjectKind::Blob, &payload);
        let packs_dir = temp.path().join(".forge/packs");
        fs::create_dir_all(&packs_dir).unwrap();

        let invalid_compressed = b"not a zstd frame".to_vec();
        write_raw_test_pack(
            &packs_dir,
            "decode",
            &id,
            invalid_compressed.clone(),
            payload.len() as u64,
        );
        let error = store.read_object(&id).unwrap_err();
        assert!(error
            .to_string()
            .contains("decompress packed native object"));

        fs::remove_file(packs_dir.join("decode.fpack")).unwrap();
        fs::remove_file(packs_dir.join("decode.fidx")).unwrap();
        let frame = object_preimage(ObjectKind::Blob, &payload);
        let compressed = zstd::stream::encode_all(std::io::Cursor::new(&frame), 0).unwrap();
        write_raw_test_pack(
            &packs_dir,
            "length",
            &id,
            compressed.clone(),
            frame.len() as u64 + 1,
        );
        let error = store.read_object(&id).unwrap_err();
        assert!(error
            .to_string()
            .contains("packed native object length mismatch"));

        fs::remove_file(packs_dir.join("length.fpack")).unwrap();
        fs::remove_file(packs_dir.join("length.fidx")).unwrap();
        let malformed_frame =
            zstd::stream::encode_all(std::io::Cursor::new(b"not framed"), 0).unwrap();
        write_raw_test_pack(
            &packs_dir,
            "malformed",
            &id,
            malformed_frame,
            b"not framed".len() as u64,
        );
        let error = store.read_object(&id).unwrap_err();
        assert!(error.to_string().contains("malformed packed native object"));
    }

    fn write_raw_test_pack(
        packs_dir: &Path,
        pack_id: &str,
        id: &ObjectId,
        compressed: Vec<u8>,
        framed_len: u64,
    ) {
        fs::write(packs_dir.join(format!("{pack_id}.fpack")), &compressed).unwrap();
        fs::write(
            packs_dir.join(format!("{pack_id}.fidx")),
            serde_json::json!({
                "schema_version": 1,
                "pack_id": pack_id,
                "entries": [{
                    "object_id": id.to_string(),
                    "offset": 0,
                    "framed_len": framed_len,
                    "compressed_len": compressed.len(),
                    "checksum": hex_lower(&Sha256::digest(&compressed))
                }]
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn read_commit_rejects_wrong_kind_and_bad_schema() {
        let id = ObjectId::parse(&format!("f1:commit:sha256:{}", "d".repeat(64))).unwrap();
        assert_eq!(id.kind().unwrap(), ObjectKind::Commit);
        let temp = tempfile::tempdir().unwrap();
        let store = NativeObjectStore::new(temp.path());
        // Wrong kind: a blob id is not a commit.
        let blob = store
            .write_object(ObjectKind::Blob, b"not a commit")
            .unwrap();
        assert!(store
            .read_commit(&blob)
            .unwrap_err()
            .to_string()
            .contains("not a commit"));
        // Bad schema: a commit whose schema_version is newer than this binary supports must
        // be refused, not silently ingested (forward-compat guard).
        let future = CommitObject {
            schema_version: SCHEMA_VERSION + 1,
            tree: format!("f1:tree:sha256:{}", "e".repeat(64)),
            parents: Vec::new(),
            intent_id: None,
            proposal_revision_id: None,
            decision_id: None,
            evidence_digest: None,
            actor: None,
            authored_time: None,
        };
        let payload = serde_json::to_vec(&future).unwrap();
        let future_id = store.write_object(ObjectKind::Commit, &payload).unwrap();
        assert!(store
            .read_commit(&future_id)
            .unwrap_err()
            .to_string()
            .contains("unsupported native commit schema version"));
    }

    #[test]
    fn read_head_rejects_non_commit_kind() {
        // HEAD must name a commit. A valid-but-wrong-kind id (a blob/tree id written
        // directly) is rejected — distinct from the unparseable-garbage case.
        let temp = tempfile::tempdir().unwrap();
        let refs = NativeRefStore::new(temp.path());
        fs::create_dir_all(temp.path().join(".forge/refs")).unwrap();
        let tree_id = ObjectId::new(ObjectKind::Tree, b"a tree, not a commit");
        fs::write(
            temp.path().join(".forge/refs/HEAD"),
            tree_id.to_string().as_bytes(),
        )
        .unwrap();
        assert!(refs
            .read_head()
            .unwrap_err()
            .to_string()
            .contains("does not name a commit"));
    }

    #[test]
    fn reachable_from_head_covers_genesis_commit_and_its_tree() {
        // gc reachability must include the live history tip: the genesis commit AND the
        // objects its tree reaches (the gc-flags-base-as-garbage fix).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("a.txt"), b"hello").unwrap();
        let base = NativeContentBackend.current_base(repo).unwrap(); // creates genesis + HEAD
        let store = NativeObjectStore::new(repo);
        let reachable = store.reachable_from_head().unwrap();
        let genesis = ObjectId::parse(&base).unwrap();
        assert!(
            reachable.contains(&genesis),
            "genesis commit must be reachable from HEAD"
        );
        let tree = ObjectId::parse(&store.read_commit(&genesis).unwrap().tree).unwrap();
        assert!(reachable.contains(&tree), "genesis tree must be reachable");
        // A repo with no HEAD (git backend / pre-anchoring) yields an empty set.
        let empty = tempfile::tempdir().unwrap();
        assert!(NativeObjectStore::new(empty.path())
            .reachable_from_head()
            .unwrap()
            .is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn native_changed_paths_reports_executable_bit_change() {
        // A chmod with unchanged content must surface in changed_paths (the blob id is
        // unchanged but the mode is part of the diff key) — parity with git diff.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("script.sh"), b"#!/bin/sh\n").unwrap();
        let backend = NativeContentBackend;
        let first = backend.snapshot_worktree(repo).unwrap(); // genesis @ mode 644
        assert!(first.changed_paths.is_empty());
        fs::set_permissions(repo.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let snap = backend.snapshot_worktree(repo).unwrap();
        assert!(
            snap.changed_paths.contains(&"script.sh".to_string()),
            "executable-bit-only change must be reported: {:?}",
            snap.changed_paths
        );
    }

    #[test]
    fn native_changed_paths_handles_nested_subdirectories() {
        // Exercises flatten_tree's recursive Dir branch in the changed_paths path: a
        // modified file and an added file in a subdirectory both surface with /-joined paths.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/main.rs"), b"v1").unwrap();
        let backend = NativeContentBackend;
        backend.snapshot_worktree(repo).unwrap(); // genesis
        fs::write(repo.join("src/main.rs"), b"v2").unwrap();
        fs::write(repo.join("src/util.rs"), b"new").unwrap();
        let mut changed = backend.snapshot_worktree(repo).unwrap().changed_paths;
        changed.sort();
        assert_eq!(
            changed,
            vec!["src/main.rs".to_string(), "src/util.rs".to_string()]
        );
    }

    #[test]
    fn ref_store_head_roundtrips_and_replaces_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let refs = NativeRefStore::new(temp.path());
        assert!(
            refs.read_head().unwrap().is_none(),
            "absent HEAD reads as None"
        );
        let first = ObjectId::new(ObjectKind::Commit, b"first");
        refs.set_head(&first).unwrap();
        assert_eq!(refs.read_head().unwrap(), Some(first));
        // Atomic replace: a second set is read back whole (never torn), into a freshly
        // created `.forge/refs/` (ancestor-fsync path).
        let second = ObjectId::new(ObjectKind::Commit, b"second");
        refs.set_head(&second).unwrap();
        assert_eq!(refs.read_head().unwrap(), Some(second));
    }

    #[test]
    fn ref_store_corrupt_head_error_is_path_free() {
        // S1: a corrupt/garbage HEAD must surface a path-free error.
        let temp = tempfile::tempdir().unwrap();
        let refs = NativeRefStore::new(temp.path());
        fs::create_dir_all(temp.path().join(".forge/refs")).unwrap();
        fs::write(
            temp.path().join(".forge/refs/HEAD"),
            b"garbage-not-an-object-id",
        )
        .unwrap();
        let error = refs.read_head().unwrap_err();
        let repo_str = temp.path().to_string_lossy();
        assert!(
            !error.to_string().contains(&*repo_str) && !format!("{error:#}").contains(&*repo_str),
            "S1: corrupt-HEAD error leaked a path: {error:#}"
        );
    }

    #[test]
    fn current_base_creates_stable_genesis_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("a.txt"), b"hello").unwrap();
        let backend = NativeContentBackend;
        let base = backend.current_base(repo).unwrap();
        assert!(
            base.starts_with("f1:commit:sha256:"),
            "native base is a commit id, not a git SHA: {base}"
        );
        // Idempotent: HEAD now exists, so a second call returns the same genesis id.
        assert_eq!(backend.current_base(repo).unwrap(), base);
        // Stability (the stale-base correctness property): editing/adding worktree files
        // must NOT move the base anchor.
        fs::write(repo.join("a.txt"), b"changed").unwrap();
        fs::write(repo.join("b.txt"), b"new").unwrap();
        assert_eq!(
            backend.current_base(repo).unwrap(),
            base,
            "base anchor must not move on worktree edits"
        );
    }

    #[test]
    fn base_content_ref_materializes_policy_excluded_tree() {
        // S2: base_content_ref names a forge-tree: that excludes is_ignored_by_policy paths.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("keep.txt"), b"data").unwrap();
        fs::write(repo.join(".env"), b"SECRET=1").unwrap();
        fs::write(repo.join("server.pem"), b"key").unwrap();
        let backend = NativeContentBackend;
        let base = backend.current_base(repo).unwrap();
        let content_ref = backend.base_content_ref(repo, &base).unwrap();
        assert!(
            content_ref.starts_with(FORGE_TREE_PREFIX),
            "base content ref is a native forge-tree: ref: {content_ref}"
        );
        let dest = tempfile::tempdir().unwrap();
        materialize_content_ref(repo, dest.path(), &content_ref).unwrap();
        assert_eq!(fs::read(dest.path().join("keep.txt")).unwrap(), b"data");
        assert!(
            !dest.path().join(".env").exists(),
            "S2: secret must not materialize from the base tree"
        );
        assert!(!dest.path().join("server.pem").exists());
    }

    #[test]
    fn base_content_ref_missing_commit_is_path_free() {
        // S1: resolving a base that points at a missing commit object surfaces a path-free
        // error (no repo/temp-dir path in the envelope or its chain).
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let backend = NativeContentBackend;
        let missing = format!("f1:commit:sha256:{}", "0".repeat(64));
        let error = backend.base_content_ref(repo, &missing).unwrap_err();
        let repo_str = repo.to_string_lossy();
        assert!(
            !error.to_string().contains(&*repo_str) && !format!("{error:#}").contains(&*repo_str),
            "S1: base_content_ref leaked a path: {error:#}"
        );
    }

    #[test]
    fn native_changed_paths_reports_added_modified_removed() {
        // No git binary involved — pure native tree diff.
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("keep.txt"), b"v1").unwrap();
        fs::write(repo.join("gone.txt"), b"bye").unwrap();
        let backend = NativeContentBackend;
        // First snapshot establishes the genesis base over the current worktree.
        let first = backend.snapshot_worktree(repo).unwrap();
        assert!(
            first.changed_paths.is_empty(),
            "genesis-equals-worktree yields no changes: {:?}",
            first.changed_paths
        );
        // Modify, add, remove.
        fs::write(repo.join("keep.txt"), b"v2").unwrap();
        fs::write(repo.join("new.txt"), b"hi").unwrap();
        fs::remove_file(repo.join("gone.txt")).unwrap();
        let mut changed = backend.snapshot_worktree(repo).unwrap().changed_paths;
        changed.sort();
        assert_eq!(
            changed,
            vec![
                "gone.txt".to_string(),
                "keep.txt".to_string(),
                "new.txt".to_string()
            ]
        );
    }

    #[test]
    fn native_changed_paths_excludes_secret() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::write(repo.join("app.txt"), b"v1").unwrap();
        let backend = NativeContentBackend;
        backend.snapshot_worktree(repo).unwrap(); // establish genesis
        fs::write(repo.join(".env"), b"SECRET=1").unwrap();
        fs::write(repo.join("app.txt"), b"v2").unwrap();
        let snap = backend.snapshot_worktree(repo).unwrap();
        assert!(snap.changed_paths.contains(&"app.txt".to_string()));
        assert!(
            !snap.changed_paths.iter().any(|p| p.contains(".env")),
            "secret must never surface in changed_paths: {:?}",
            snap.changed_paths
        );
    }

    #[test]
    fn native_production_paths_shell_no_git() {
        // NER-138 exit criterion: no git binary in the native snapshot/base/changed-paths
        // production paths. The differential harness in THIS test module still shells git
        // for the slice-1 parity proofs, so scope the scan to the production prefix —
        // everything before the test module. The invariant is strong and false-positive-
        // free: production native code spawns NO subprocess at all (`Command::new`). The
        // `ignore`-crate walker, the object/ref stores, and the tree diff are all
        // in-process, so any `Command::new` outside #[cfg(test)] is a git-dependency
        // regression. (Substring-matching `ls-files`/`rev-parse` would false-positive on
        // doc comments that reference the removed git calls, so we gate on the spawn.)
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lib.rs"))
            .expect("read forge-content-native/src/lib.rs");
        let production = src
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("source has a production prefix before the test module");
        assert!(
            !production.contains("Command::new"),
            "native production code must spawn no subprocess (found `Command::new` outside \
             #[cfg(test)]); slice 2 made base_head + changed_paths git-binary-free"
        );
    }
}
