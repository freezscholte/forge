use serde::Serialize;

use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub attempt_id: String,
    pub parent_snapshot_id: Option<String>,
    pub content_ref: String,
    pub changed_paths: Vec<String>,
    pub operation_id: String,
    pub current_view_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub snapshot_id: String,
    pub content_ref: String,
    pub changed_paths: Vec<String>,
}

pub fn save_snapshot(
    cwd: &Path,
    request_id: Option<String>,
    attempt_id: Option<&str>,
    content_ref: String,
    changed_paths: Vec<String>,
) -> Result<SnapshotRecord> {
    save_snapshot_with_private_overlays(
        cwd,
        request_id,
        attempt_id,
        content_ref,
        changed_paths,
        Vec::new(),
    )
}

pub fn save_snapshot_with_private_overlays(
    cwd: &Path,
    request_id: Option<String>,
    attempt_id: Option<&str>,
    content_ref: String,
    changed_paths: Vec<String>,
    private_overlays: Vec<SaveSnapshotPrivateOverlayInput>,
) -> Result<SnapshotRecord> {
    let context = open_repository(cwd)?;
    let attempt = resolve_attempt_in_context(&context, attempt_id)?.attempt;
    // Write-binding verification (NER-134): the authoritative, non-bypassable guard
    // on the production write path. Refuse to record the worktree's content under an
    // attempt other than the one the worktree is materialized for.
    verify_worktree_binding(&context, &attempt.attempt_id)?;
    let mut connection = open_connection(&context.database_path)?;
    let (snapshot_id, parent_snapshot_id, op) = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        for overlay in &private_overlays {
            validate_work_package_kind(&overlay.work_package_kind)?;
            validate_private_hash(&overlay.path_hash)?;
            validate_private_hash(&overlay.ciphertext_digest)?;
            if overlay.envelope_format.trim().is_empty()
                || overlay.recipient_fingerprint.trim().is_empty()
                || overlay.private_object_path.trim().is_empty()
                || overlay.encrypted_metadata_json.trim().is_empty()
            {
                return Err(ForgeError::PrivateContentInvalid {
                    reason: "empty_private_payload_metadata".to_string(),
                }
                .into());
            }
        }
        let parent_snapshot_id: Option<String> = tx
            .query_row(
                "SELECT id FROM snapshots WHERE attempt_id = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![attempt.attempt_id],
                |row| row.get(0),
            )
            .optional()?;
        let snapshot_id = new_id("snapshot");
        tx.execute(
            "INSERT INTO snapshots (
                id, repo_id, attempt_id, parent_snapshot_id, content_ref, changed_paths_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                snapshot_id,
                context.repo_id,
                attempt.attempt_id,
                parent_snapshot_id,
                content_ref,
                serde_json::to_string(&changed_paths)?,
                now_ms()
            ],
        )?;
        let now = now_ms();
        for overlay in private_overlays.clone() {
            insert_encrypted_private_payload_on(
                tx,
                &context.repo_id,
                EncryptedPrivatePayloadInput {
                    work_package_kind: overlay.work_package_kind,
                    work_package_id: overlay.work_package_id,
                    snapshot_id: Some(snapshot_id.clone()),
                    path_label_id: overlay.path_label_id,
                    path_hash: overlay.path_hash,
                    envelope_format: overlay.envelope_format,
                    recipient_fingerprint: overlay.recipient_fingerprint,
                    ciphertext_digest: overlay.ciphertext_digest,
                    private_object_path: overlay.private_object_path,
                    encrypted_metadata_json: overlay.encrypted_metadata_json,
                },
                now,
            )?;
        }
        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "save".to_string(),
                kind: "snapshot_saved".to_string(),
                view_kind: ViewKind::Initialized,
                // NER-255: persist the success `data` payload (minus operation_id /
                // current_view_id, which are minted by this very insert) into the op
                // view state so an idempotent replay can return the ORIGINAL ids instead
                // of just {idempotent_replay, request_id}. The existing lifecycle/id keys
                // are kept as siblings (other code json_extracts $.lifecycle,
                // $.snapshot_id, $.attempt_id) — `replay_data` is added alongside, never
                // nesting them. `operation_id` is overlaid on replay from the op row;
                // `current_view_id` is intentionally omitted (not known until after this
                // insert, and not crash-recovery-critical).
                state: json!({
                    "lifecycle": "snapshot_saved",
                    "attempt_id": attempt.attempt_id,
                    "snapshot_id": snapshot_id,
                    "replay_data": {
                        "snapshot_id": snapshot_id,
                        "attempt_id": attempt.attempt_id,
                        "parent_snapshot_id": parent_snapshot_id,
                        "content_ref": content_ref,
                        "changed_paths": changed_paths,
                    }
                }),
            },
        )?;
        // NER-143 R1: the worktree now holds exactly this snapshot's tree (save captured it),
        // so it becomes the expected dirty-check baseline for the worktree that issued `save`.
        // Native attempt workspaces have independent materialized baselines; do not let a
        // workspace save poison the owner repo's root-level dirty check.
        set_context_expected_content_ref(tx, &context, &content_ref)?;
        if context.workspace_attempt_id.is_some() {
            initialize_root_expected_content_ref_if_missing(tx, &context, &attempt.base_head)?;
        }
        Ok((snapshot_id, parent_snapshot_id, op))
    })?;
    Ok(SnapshotRecord {
        snapshot_id,
        attempt_id: attempt.attempt_id,
        parent_snapshot_id,
        content_ref,
        changed_paths,
        operation_id: op.operation_id,
        current_view_id: op.view_id,
    })
}

/// The attempt that owns `snapshot_id` (NER-134 Piece 1b), so `restore` can refuse
/// to materialize a snapshot belonging to an attempt other than the bound one.
pub fn snapshot_owner_attempt_id(cwd: &Path, snapshot_id: &str) -> Result<String> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    connection
        .query_row(
            "SELECT attempt_id FROM snapshots WHERE repo_id = ?1 AND id = ?2",
            params![context.repo_id, snapshot_id],
            |row| row.get(0),
        )
        .optional()?
        // Defensive: `restore` resolves `snapshot_content_ref` first, so a missing
        // snapshot already errors before this is reached.
        .ok_or_else(|| ForgeError::NoSnapshot.into())
}

pub fn snapshot_content_ref(cwd: &Path, snapshot_id: &str) -> Result<String> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    connection
        .query_row(
            "SELECT content_ref FROM snapshots WHERE id = ?1",
            params![snapshot_id],
            |row| row.get(0),
        )
        .with_context(|| format!("unknown snapshot {snapshot_id}"))
}

pub fn latest_snapshot_content_ref(cwd: &Path, attempt_id: Option<&str>) -> Result<Option<String>> {
    let context = open_repository(cwd)?;
    let attempt = resolve_attempt_in_context(&context, attempt_id)?.attempt;
    Ok(latest_snapshot_for_attempt(&context, &attempt.attempt_id)?
        .map(|snapshot| snapshot.content_ref))
}

/// The content ref the effective worktree is EXPECTED to hold — the tree the last materializing
/// op put there. Owner-root worktrees use `current_state.expected_content_ref` (migration 007,
/// NER-143 R1). Native attempt workspaces use their own `attempt_workspaces.materialized_content_ref`
/// so a save/run/propose loop in one isolated workspace does not poison root-level dirty checks
/// or another attempt workspace's baseline.
///
/// `None` for a pre-007 repo or a fresh worktree before its first materialize; the dirty-check
/// then falls back to the latest-snapshot baseline. This is the crash-safe baseline: a non-save
/// op materializes a different tree than the latest *saved* snapshot, so comparing the worktree
/// against "latest saved" spuriously fails chained navigation (undo twice) — comparing against
/// "expected" does not.
pub fn expected_content_ref(cwd: &Path) -> Result<Option<String>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    if let Some(attempt_id) = context.workspace_attempt_id.as_deref() {
        return Ok(connection
            .query_row(
                "SELECT materialized_content_ref FROM attempt_workspaces
                 WHERE repo_id = ?1 AND attempt_id = ?2",
                params![context.repo_id, attempt_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten());
    }
    Ok(connection
        .query_row(
            "SELECT expected_content_ref FROM current_state WHERE singleton = 1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// Set `current_state.expected_content_ref` to the tree a root worktree materializing op just put
/// in the owner repo (NER-143 R1). Called inside the recorder's `IMMEDIATE` txn — atomic with the
/// op-log advance, so a `CurrentStateChanged` CAS-loss rolls BOTH back together. DR-F2: this is a
/// DEDICATED UPDATE in each materializing recorder, never folded into the shared
/// `insert_operation_view` CAS (which every op hits — folding it there would clobber the expected
/// ref on a non-materializing `accept`/`run`/`propose`/`check`).
fn set_expected_content_ref(tx: &Connection, content_ref: &str) -> Result<()> {
    tx.execute(
        "UPDATE current_state SET expected_content_ref = ?1 WHERE singleton = 1",
        params![content_ref],
    )?;
    Ok(())
}

fn set_workspace_expected_content_ref(
    tx: &Connection,
    context: &RepositoryContext,
    attempt_id: &str,
    content_ref: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE attempt_workspaces
         SET materialized_content_ref = ?1, updated_at_ms = ?2
         WHERE repo_id = ?3 AND attempt_id = ?4",
        params![content_ref, now_ms(), context.repo_id, attempt_id],
    )?;
    Ok(())
}

pub(crate) fn set_context_expected_content_ref(
    tx: &Connection,
    context: &RepositoryContext,
    content_ref: &str,
) -> Result<()> {
    if let Some(attempt_id) = context.workspace_attempt_id.as_deref() {
        set_workspace_expected_content_ref(tx, context, attempt_id, content_ref)
    } else {
        set_expected_content_ref(tx, content_ref)
    }
}

fn initialize_root_expected_content_ref_if_missing(
    tx: &Connection,
    context: &RepositoryContext,
    base_head: &str,
) -> Result<()> {
    let existing = tx.query_row(
        "SELECT expected_content_ref FROM current_state WHERE singleton = 1",
        [],
        |row| row.get::<_, Option<String>>(0),
    )?;
    if existing.is_some() || context.content_backend != "native" {
        return Ok(());
    }
    let id = match forge_content_native::ObjectId::parse(base_head) {
        Ok(id) if matches!(id.kind(), Ok(forge_content_native::ObjectKind::Commit)) => id,
        _ => return Ok(()),
    };
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let commit = store.read_commit(&id)?;
    set_expected_content_ref(
        tx,
        &format!("{}{}", forge_content::FORGE_TREE_PREFIX, commit.tree),
    )
}

pub fn record_restore(
    cwd: &Path,
    request_id: Option<String>,
    snapshot_id: &str,
    content_ref: &str,
) -> Result<OperationViewResult> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    let op = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "restore".to_string(),
                kind: "snapshot_restored".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({ "lifecycle": "snapshot_restored", "snapshot_id": snapshot_id }),
            },
        )?;
        // NER-143 R1: restore just materialized this snapshot's tree into the worktree.
        set_context_expected_content_ref(tx, &context, content_ref)?;
        Ok(op)
    })?;
    Ok(op)
}

/// Resolve a historical commit id to the `forge-tree:` content ref of its tree, for
/// `forge checkout` (NER-138 Phase 7 slice 3). Fail-closed BEFORE any materialization:
/// - a non-parseable / non-commit id, or a never-written id the ledger does not reference,
///   is a USER error → a path-free `anyhow` ("unknown commit", mapped to COMMAND_FAILED),
///   NOT corruption (so a typo never inflates the perceived corruption rate);
/// - a commit/tree the ledger references but whose object is missing is genuine corruption
///   → typed `NativeHistoryCorrupt` (DanglingCommitId / DanglingTree).
pub fn checkout_target_content_ref(cwd: &Path, commit_id: &str) -> Result<String> {
    let context = open_repository(cwd)?;
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let id = match forge_content_native::ObjectId::parse(commit_id) {
        Ok(id) if matches!(id.kind(), Ok(forge_content_native::ObjectKind::Commit)) => id,
        _ => bail!("unknown commit: not a native commit id in this repository"),
    };
    let commit = match store.read_commit(&id) {
        Ok(commit) => commit,
        Err(_) => return Err(classify_missing_commit(cwd, commit_id)?),
    };
    let content_ref = format!("{}{}", forge_content::FORGE_TREE_PREFIX, commit.tree);
    // The tree (and everything it reaches) must exist before we clobber the worktree.
    store
        .verify_content_ref(&content_ref)
        .map_err(|_| ForgeError::NativeHistoryCorrupt {
            kind: NativeHistoryCorruptKind::DanglingTree,
            commit_id: commit_id.to_string(),
            related_id: Some(commit.tree.clone()),
        })?;
    Ok(content_ref)
}

/// Classify a native commit id whose object is absent from the store (its `read_commit`
/// failed) as either genuine corruption or a user error. When the ledger still references
/// the id — a `decisions.commit_id` row or a recorded sync-merge tip — the object should
/// exist, so its absence is genuine corruption → `NativeHistoryCorrupt::DanglingCommitId`.
/// Otherwise the id is a typo / never-written commit → a path-free "unknown commit" user
/// error (so a typo never inflates the perceived corruption rate). Returns the error to
/// raise; the `Err` arm is a genuine DB read failure to propagate. Shared by
/// [`checkout_target_content_ref`] and `blame`'s `--at` selector so a store-missing commit
/// classifies identically on both paths.
pub fn classify_missing_commit(cwd: &Path, commit_id: &str) -> Result<anyhow::Error> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let query = format!(
        "SELECT 1 FROM decisions WHERE repo_id = ?1 AND commit_id = ?2
         UNION ALL
         SELECT 1
           FROM operations o
           JOIN views v ON v.id = o.resulting_view_id
          WHERE o.repo_id = ?1
            AND o.kind IN ({})
            AND json_extract(v.state_json, '$.commit_id') = ?2
         LIMIT 1",
        sync::SYNC_MERGED_OP_KIND_SQL_IN
    );
    let referenced: bool = connection
        .query_row(&query, params![context.repo_id, commit_id], |_| Ok(true))
        .optional()?
        .unwrap_or(false);
    if referenced {
        Ok(ForgeError::NativeHistoryCorrupt {
            kind: NativeHistoryCorruptKind::DanglingCommitId,
            commit_id: commit_id.to_string(),
            related_id: None,
        }
        .into())
    } else {
        Ok(anyhow::anyhow!(
            "unknown commit: not in this repository's native history"
        ))
    }
}

/// Record a `forge checkout` in the op-log (NER-138 Phase 7 slice 3) so `undo` can reverse
/// it and gc treats the materialized commit as a reachability root. The target `commit_id`
/// is in the view `state_json`. Does NOT advance the base anchor (checkout is materialize-only
/// — a `save` afterward still diffs against the unchanged base HEAD; see the slice-3 plan).
pub fn record_checkout(
    cwd: &Path,
    request_id: Option<String>,
    commit_id: &str,
    content_ref: &str,
) -> Result<OperationViewResult> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    let op = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "checkout".to_string(),
                kind: "commit_checked_out".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({ "lifecycle": "commit_checked_out", "commit_id": commit_id }),
            },
        )?;
        // NER-143 R1: checkout just materialized this commit's tree into the worktree.
        set_context_expected_content_ref(tx, &context, content_ref)?;
        Ok(op)
    })?;
    Ok(op)
}

pub fn set_materialized_expected_content_ref(cwd: &Path, content_ref: &str) -> Result<()> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        set_context_expected_content_ref(tx, &context, content_ref)
    })
}

/// What a `forge undo` will restore (NER-138 Phase 7 slice 3 / NER-143 R3+R4).
///
/// **Semantics (deliberate v0 cut):** undo reverses the **last save of the attached attempt** by
/// restoring that save's parent snapshot (the `snapshots.parent_snapshot_id` chain — robust, no
/// cross-table timestamp comparison). It is NOT the op-log `current_state` rewind: after a
/// non-save head op (accept/checkout/run) undo still reverses the last *save*, not that head op.
/// The full op-log-rewind model is future work.
///
/// `undone_operation_id` (NER-143 R4) is the `save` operation that produced the snapshot being
/// reversed (the attempt's latest), resolved from that save's view — NOT the op-log head, which
/// after a non-save head op would mislabel the audit field.
#[derive(Debug, Clone, Serialize)]
pub struct UndoTarget {
    pub undone_operation_id: String,
    pub content_ref: String,
    pub restored_snapshot_id: String,
}

/// Resolve what `forge undo` will restore: the parent of the **attached attempt's** latest
/// snapshot. Read-only and fail-closed with a clear, path-free "nothing to undo" when there is no
/// snapshot, or the latest snapshot is the first (no earlier state to restore). Restoring past the
/// first snapshot (to the base) is future work.
pub fn undo_target(cwd: &Path) -> Result<UndoTarget> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    // NER-143 R3: bind to the attached attempt. The repo-wide latest snapshot could belong to a
    // DIFFERENT attempt than the one the worktree is bound to, so undo could otherwise restore
    // attempt X's content into attempt Y's worktree (the dirty-check resolves the attached
    // attempt's latest, so the two would also disagree). The `parent_snapshot_id` chain stays
    // within an attempt (`save_snapshot` chains per-attempt), so binding the latest-snapshot
    // selection to the attached attempt makes "undo the last save" mean "this attempt's last
    // save" and never crosses attempts.
    let attempt = resolve_attempt_in_context(&context, None)?.attempt;
    let latest: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT id, parent_snapshot_id FROM snapshots \
             WHERE attempt_id = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
            params![attempt.attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((latest_id, parent_snapshot_id)) = latest else {
        bail!("nothing to undo: this repository has no snapshots");
    };
    let Some(parent_id) = parent_snapshot_id else {
        bail!("nothing to undo: already at the first saved snapshot");
    };
    let content_ref: String = connection.query_row(
        "SELECT content_ref FROM snapshots WHERE id = ?1",
        params![parent_id],
        |row| row.get(0),
    )?;
    // NER-143 R4: the undone operation is the SAVE that produced the latest snapshot (the one
    // being reversed) — found via that save view's `snapshot_id` — not the op-log head. Scoped
    // to `snapshot_saved` views: a later `restore`/checkout of the same snapshot ALSO carries
    // `$.snapshot_id`, so without the lifecycle filter the ORDER BY would pick that restore op
    // and mislabel the audit field (code-review finding). Falls back to the op-log head only if
    // no save view is found (defensive; a save always records one). `json_extract` is exact (no
    // LIKE false-matches); SQLite's JSON1 is bundled.
    let undone_operation_id: String = connection
        .query_row(
            "SELECT operation_id FROM views \
             WHERE repo_id = ?1 AND json_extract(state_json, '$.snapshot_id') = ?2 \
             AND json_extract(state_json, '$.lifecycle') = 'snapshot_saved' \
             ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
            params![context.repo_id, latest_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| context.current_operation_id.clone());
    Ok(UndoTarget {
        undone_operation_id,
        content_ref,
        restored_snapshot_id: parent_id,
    })
}

/// Record a `forge undo` in the op-log (NER-138 Phase 7 slice 3). Append-only: undo is a
/// FORWARD operation that restores prior content — it NEVER deletes a `decisions` row (so an
/// undone accept's `commit_id` stays a permanent gc reachability root) or any op-log row. The
/// undone operation + restored snapshot are in the view `state_json` for auditability.
pub fn record_undo(
    cwd: &Path,
    request_id: Option<String>,
    undone_operation_id: &str,
    restored_snapshot_id: &str,
    content_ref: &str,
) -> Result<OperationViewResult> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    let op = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "undo".to_string(),
                kind: "operation_undone".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({
                    "lifecycle": "undone",
                    "undone_operation_id": undone_operation_id,
                    "restored_snapshot_id": restored_snapshot_id,
                }),
            },
        )?;
        // NER-143 R1: undo just materialized the restored snapshot's tree into the worktree.
        set_context_expected_content_ref(tx, &context, content_ref)?;
        Ok(op)
    })?;
    Ok(op)
}

pub fn reconcile_native_head(cwd: &Path) -> Result<()> {
    let context = match open_repository(cwd) {
        Ok(context) => context,
        // Not a forge repo / not initialized: nothing to reconcile. The command's own
        // `open_repository` surfaces the real NOT_INITIALIZED — reconcile is best-effort.
        Err(_) => return Ok(()),
    };
    let refs = forge_content_native::NativeRefStore::new(&context.root_path);
    let current_head = refs.read_head()?;
    let connection = open_connection(&context.database_path)?;
    let tip = native_tip(&context, &connection)?;
    let Some(tip) = tip else {
        return Ok(()); // genesis-only / git repo — nothing to reconcile
    };
    if current_head.as_ref() == Some(&tip) {
        return Ok(()); // HEAD already current
    }
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    // Walk the tip's ancestry: every object must exist (a miss is the store-before-DB
    // violation), there is no cycle, and the current HEAD must be an ancestor of the tip
    // (HEAD lags, never forks). Merge history may reach the same shared ancestor more
    // than once; repeated visited commits are normal diamond ancestry, not corruption.
    let commits = walk_native_commits(&store, &tip)?;
    for (cid, commit) in &commits {
        let tree_ref = format!("{}{}", forge_content::FORGE_TREE_PREFIX, commit.tree);
        if store.verify_content_ref(&tree_ref).is_err() {
            return Err(ForgeError::NativeHistoryCorrupt {
                kind: NativeHistoryCorruptKind::DanglingTree,
                commit_id: cid.to_string(),
                related_id: Some(commit.tree.clone()),
            }
            .into());
        }
    }
    let head_reached = current_head
        .as_ref()
        .map(|head| commits.iter().any(|(cid, _)| cid == head))
        .unwrap_or(true);
    if !head_reached {
        // The ledger tip does not descend from the current HEAD — a fork, which lock-
        // serialized accepts cannot produce: the store is corrupt.
        return Err(ForgeError::NativeHistoryCorrupt {
            kind: NativeHistoryCorruptKind::DanglingParent,
            commit_id: tip.to_string(),
            related_id: current_head.map(|head| head.to_string()),
        }
        .into());
    }
    refs.set_head(&tip)?;
    Ok(())
}

/// The authoritative native history tip: the latest native-history-producing ledger entry
/// (accepted decisions or clean sync merge operations) that descends from the cached HEAD if any
/// exists, else the ref-store HEAD (the genesis), else `None`. Imported peer decisions can carry
/// divergent native commit IDs and peer wall-clock timestamps, so timestamp ordering alone is not
/// allowed to advance the local tip across a fork.
pub(crate) fn native_tip(
    context: &RepositoryContext,
    connection: &Connection,
) -> Result<Option<forge_content_native::ObjectId>> {
    let refs = forge_content_native::NativeRefStore::new(&context.root_path);
    let current_head = refs.read_head()?;
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let query = format!(
        "SELECT commit_id, source_rank FROM (
                 SELECT d.commit_id AS commit_id,
                        d.created_at_ms AS created_at_ms,
                        0 AS source_rank,
                        d.rowid AS tie_rowid
                   FROM decisions d
                  WHERE d.repo_id = ?1
                    AND d.commit_id IS NOT NULL
                 UNION ALL
                 SELECT json_extract(v.state_json, '$.commit_id') AS commit_id,
                        o.created_at_ms AS created_at_ms,
                        1 AS source_rank,
                        o.rowid AS tie_rowid
                  FROM operations o
                   JOIN views v ON v.id = o.resulting_view_id
                  WHERE o.repo_id = ?1
                    AND o.kind IN ({})
                    AND json_extract(v.state_json, '$.commit_id') IS NOT NULL
             )
             ORDER BY created_at_ms DESC, source_rank DESC, tie_rowid DESC, commit_id DESC",
        sync::SYNC_MERGED_OP_KIND_SQL_IN
    );
    let mut statement = connection.prepare(&query)?;
    let candidates = statement
        .query_map(params![context.repo_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if let (Some(head), Some(candidate)) = (current_head.as_ref(), candidates.first()) {
        if candidate == &head.to_string() {
            return Ok(current_head);
        }
    }
    for candidate in candidates {
        let candidate = forge_content_native::ObjectId::parse(&candidate)?;
        let Some(head) = current_head.as_ref() else {
            return Ok(Some(candidate));
        };
        if &candidate == head {
            return Ok(Some(candidate));
        }
        match walk_native_commits(&store, &candidate) {
            Ok(commits) if commits.iter().any(|(cid, _)| cid == head) => {
                return Ok(Some(candidate));
            }
            Ok(_) => {}
            // Tip selection only trusts candidates whose objects can be walked; doctor still
            // reports dangling decision/op commit ids independently via verify_native_history.
            Err(_) => {}
        }
    }
    Ok(current_head)
}

/// One commit in the native history, as surfaced by `forge log` through the JSON contract
/// ("show every change under this intent and the evidence that justified it"). Optional
/// justification fields are omitted when absent (genesis), matching the on-object shape.
#[derive(Debug, Clone, Serialize)]
pub struct CommitView {
    pub commit_id: String,
    pub tree: String,
    pub parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal_revision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authored_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_digest: Option<String>,
}

/// The authoritative ledger-derived native history tip, exactly as `native_log`
/// resolves it (NER-362). Read-only (no lock, no HEAD write): the ref-store HEAD
/// lags the ledger by design (HEAD-lags-never-leads) and `reconcile_native_head`
/// runs only for mutating commands, so read-only callers (e.g. `forge blame`)
/// resolve their tip here instead of reading HEAD. `None` when there is no native
/// history yet.
pub fn native_history_tip(cwd: &Path) -> Result<Option<forge_content_native::ObjectId>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    native_tip(&context, &connection)
}

/// Walk the native commit DAG from the authoritative tip (NER-138 Phase 7 slice 3),
/// tip→genesis, returning each commit's justification. Read-only (no lock, no HEAD write).
/// When `intent` is `Some`, only commits whose `intent_id` matches are returned — the literal
/// "show every change under this intent" query. A missing tip/parent object surfaces typed
/// `NativeHistoryCorrupt` (DanglingCommitId for the tip, DanglingParent deeper); a parent
/// cycle is `Cycle`.
pub fn native_log(cwd: &Path, intent: Option<&str>) -> Result<Vec<CommitView>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let tip = native_tip(&context, &connection)?;
    let mut out = Vec::new();
    let Some(tip) = tip else {
        return Ok(out);
    };
    for (cid, commit) in walk_native_commits(&store, &tip)? {
        let matches = intent
            .map(|want| commit.intent_id.as_deref() == Some(want))
            .unwrap_or(true);
        if matches {
            out.push(CommitView {
                commit_id: cid.to_string(),
                tree: commit.tree.clone(),
                parents: commit.parents.clone(),
                intent_id: commit.intent_id.clone(),
                proposal_revision_id: commit.proposal_revision_id.clone(),
                decision_id: commit.decision_id.clone(),
                actor: commit.actor.clone(),
                authored_time: commit.authored_time,
                evidence_digest: commit
                    .evidence_digest
                    .as_ref()
                    .map(|h| h.as_str().to_string()),
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeVisitState {
    Visiting,
    Visited,
}

pub(crate) fn walk_native_commits(
    store: &forge_content_native::NativeObjectStore,
    tip: &forge_content_native::ObjectId,
) -> Result<
    Vec<(
        forge_content_native::ObjectId,
        forge_content_native::CommitObject,
    )>,
> {
    let mut out = Vec::new();
    let mut states = std::collections::BTreeMap::new();
    let mut stack = vec![(tip.clone(), false)];
    while let Some((cid, expanded)) = stack.pop() {
        if expanded {
            states.insert(cid, NativeVisitState::Visited);
            continue;
        }
        match states.get(&cid).copied() {
            Some(NativeVisitState::Visited) => continue,
            Some(NativeVisitState::Visiting) => {
                return Err(ForgeError::NativeHistoryCorrupt {
                    kind: NativeHistoryCorruptKind::Cycle,
                    commit_id: cid.to_string(),
                    related_id: None,
                }
                .into());
            }
            None => {}
        }
        states.insert(cid.clone(), NativeVisitState::Visiting);
        let commit = store.read_commit(&cid).map_err(|_| {
            let kind = if &cid == tip {
                NativeHistoryCorruptKind::DanglingCommitId
            } else {
                NativeHistoryCorruptKind::DanglingParent
            };
            anyhow::Error::from(ForgeError::NativeHistoryCorrupt {
                kind,
                commit_id: cid.to_string(),
                related_id: None,
            })
        })?;
        out.push((cid.clone(), commit.clone()));
        stack.push((cid, true));
        for parent in commit.parents.iter().rev() {
            stack.push((forge_content_native::ObjectId::parse(parent)?, false));
        }
    }
    Ok(out)
}

pub(crate) fn latest_snapshot_for_attempt(
    context: &RepositoryContext,
    attempt_id: &str,
) -> Result<Option<SnapshotSummary>> {
    let connection = open_connection(&context.database_path)?;
    latest_snapshot_on(&connection, attempt_id)
}

/// Determining "latest snapshot" read against a caller-supplied connection. A
/// writer passes its own `IMMEDIATE` transaction (`&tx` deref-coerces to
/// `&Connection`) so the read-then-write is atomic on one connection (U4).
pub(crate) fn latest_snapshot_on(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<SnapshotSummary>> {
    connection
        .query_row(
            "SELECT id, content_ref, changed_paths_json FROM snapshots
             WHERE attempt_id = ?1 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
            params![attempt_id],
            |row| {
                let changed_paths_json: String = row.get(2)?;
                Ok(SnapshotSummary {
                    snapshot_id: row.get(0)?,
                    content_ref: row.get(1)?,
                    changed_paths: serde_json::from_str(&changed_paths_json).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(Into::into)
}
