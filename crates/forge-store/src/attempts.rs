use serde::{Deserialize, Serialize};

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkspaceMarker {
    pub(crate) repo_root: String,
    pub(crate) attempt_id: String,
}

/// Role qualifier emitted next to `workspace_path` (NER-382): the workspace dir
/// is materialized state, not an editing surface. Enum-ready, but this is the
/// only value today.
pub const WORKSPACE_ROLE_MATERIALIZATION_TARGET: &str = "materialization_target";

#[derive(Debug, Clone, Serialize)]
pub struct StartAttempt {
    pub intent_id: String,
    pub attempt_id: String,
    pub base_head: String,
    pub attached: bool,
    pub workspace_path: String,
    pub workspace_role: &'static str,
    pub operation_id: String,
    pub current_view_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    pub attempt_id: String,
    pub intent_id: String,
    pub intent: String,
    pub base_head: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptSummary {
    pub attempt_id: String,
    pub intent_id: String,
    pub intent: String,
    pub base_head: String,
    pub status: String,
    pub attached: bool,
    pub workspace_path: String,
    /// Additive NER-382 qualifier mirroring [`StartAttempt::workspace_role`]:
    /// `attempt list`/`attempt show` carry the same role next to `workspace_path`.
    pub workspace_role: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptShowRecord {
    pub attempt: AttemptSummary,
    pub latest_snapshot: Option<SnapshotSummary>,
    pub latest_evidence: Option<EvidenceSummary>,
    pub proposals: Vec<ProposalMetadata>,
}

#[derive(Debug, Clone)]
pub struct ResolvedAttempt {
    pub attempt: AttemptRecord,
}

pub fn start_attempt(
    cwd: &Path,
    request_id: Option<String>,
    intent: String,
    base_head: String,
    check_spec_json: Option<String>,
) -> Result<StartAttempt> {
    let context = open_repository(cwd)?;
    create_attempt(
        &context,
        request_id,
        None,
        Some(intent),
        base_head,
        true,
        "start",
        check_spec_json,
    )
}

pub fn start_attempt_for_intent(
    cwd: &Path,
    request_id: Option<String>,
    intent_id: String,
    base_head: String,
) -> Result<StartAttempt> {
    let context = open_repository(cwd)?;
    create_attempt(
        &context,
        request_id,
        Some(intent_id),
        None,
        base_head,
        false,
        "attempt start",
        // `attempt start` references an existing intent and inherits its gates; it
        // never mints an intent, so it carries no check spec of its own (NER-135).
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn create_attempt(
    context: &RepositoryContext,
    request_id: Option<String>,
    intent_id: Option<String>,
    intent: Option<String>,
    base_head: String,
    attach: bool,
    command: &str,
    check_spec_json: Option<String>,
) -> Result<StartAttempt> {
    let mut connection = open_connection(&context.database_path)?;
    let (intent_id, attempt_id, op) = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let (intent_id, intent_visibility) = match intent_id.clone() {
            Some(id) => {
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM intents WHERE repo_id = ?1 AND id = ?2)",
                    params![context.repo_id, id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Err(ForgeError::UnknownIntent {
                        selector: id.to_string(),
                    }
                    .into());
                }
                let visibility =
                    effective_work_package_visibility_on(tx, &context.repo_id, "intent", &id)?;
                insert_work_package_visibility(
                    tx,
                    &context.repo_id,
                    "intent",
                    &id,
                    &visibility,
                    now,
                )?;
                (id, visibility)
            }
            None => {
                let id = new_id("intent");
                tx.execute(
                    "INSERT INTO intents (id, repo_id, text, check_spec_json, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id,
                        context.repo_id,
                        intent
                            .clone()
                            .unwrap_or_else(|| "local agent attempt".to_string()),
                        check_spec_json,
                        now
                    ],
                )?;
                let visibility = visibility_policy_on(tx)?.default_work_package_visibility;
                insert_work_package_visibility(
                    tx,
                    &context.repo_id,
                    "intent",
                    &id,
                    &visibility,
                    now,
                )?;
                (id, visibility)
            }
        };
        let attempt_id = new_id("attempt");
        tx.execute(
            "INSERT INTO attempts (id, repo_id, intent_id, base_head, status, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'active', ?5)",
            params![attempt_id, context.repo_id, intent_id, base_head, now],
        )?;
        insert_work_package_visibility(
            tx,
            &context.repo_id,
            "attempt",
            &attempt_id,
            &intent_visibility,
            now,
        )?;
        tx.execute(
            "INSERT INTO attempt_workspaces (
                attempt_id, repo_id, workspace_rel_path, status,
                materialized_content_ref, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, 'active', NULL, ?4, ?4)",
            params![
                attempt_id,
                context.repo_id,
                workspace_rel_path_for_attempt(&attempt_id),
                now
            ],
        )?;

        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: command.to_string(),
                kind: "attempt_started".to_string(),
                view_kind: ViewKind::Initialized,
                // NER-255: mirror the success `data` into the op view state for
                // idempotent replay (see save_snapshot for the rationale). `operation_id`
                // is overlaid on replay; `current_view_id` is omitted (minted by this
                // insert). The lifecycle/id keys stay siblings for existing json_extracts.
                state: json!({
                    "lifecycle": "attempt_active",
                    "attempt_id": attempt_id,
                    "intent_id": intent_id,
                    "replay_data": {
                        "intent_id": intent_id,
                        "attempt_id": attempt_id,
                        "base_head": base_head,
                        "attached": attach,
                        "workspace_path": workspace_rel_path_for_attempt(&attempt_id),
                        "workspace_role": WORKSPACE_ROLE_MATERIALIZATION_TARGET,
                    }
                }),
            },
        )?;
        if attach {
            tx.execute(
                "UPDATE current_state SET attached_attempt_id = ?1 WHERE singleton = 1",
                params![attempt_id],
            )?;
        }
        Ok((intent_id, attempt_id, op))
    })?;

    Ok(StartAttempt {
        intent_id,
        workspace_path: workspace_rel_path_for_attempt(&attempt_id),
        workspace_role: WORKSPACE_ROLE_MATERIALIZATION_TARGET,
        attempt_id,
        base_head,
        attached: attach,
        operation_id: op.operation_id,
        current_view_id: op.view_id,
    })
}

fn workspace_rel_path_for_attempt(attempt_id: &str) -> String {
    format!(".forge/worktrees/{attempt_id}")
}

pub(crate) fn attempt_workspace_rel_path(
    context: &RepositoryContext,
    attempt_id: &str,
) -> Result<String> {
    let connection = open_connection(&context.database_path)?;
    connection
        .query_row(
            "SELECT workspace_rel_path FROM attempt_workspaces
             WHERE repo_id = ?1 AND attempt_id = ?2",
            params![context.repo_id, attempt_id],
            |row| row.get(0),
        )
        .optional()
        .map(|value| value.unwrap_or_else(|| workspace_rel_path_for_attempt(attempt_id)))
        .map_err(Into::into)
}

pub fn attempt_workspace_path(cwd: &Path, attempt_id: &str) -> Result<PathBuf> {
    let context = open_repository(cwd)?;
    attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
        selector: attempt_id.to_string(),
    })?;
    let rel = attempt_workspace_rel_path(&context, attempt_id)?;
    Ok(context.root_path.join(rel))
}

pub fn ensure_attempt_workspace_marker(cwd: &Path, attempt_id: &str) -> Result<PathBuf> {
    let context = open_repository(cwd)?;
    attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
        selector: attempt_id.to_string(),
    })?;
    let path = attempt_workspace_path(cwd, attempt_id)?;
    fs::create_dir_all(&path).map_err(|error| anyhow!("create workspace: {}", error.kind()))?;
    let marker = WorkspaceMarker {
        repo_root: context.root_path.to_string_lossy().into_owned(),
        attempt_id: attempt_id.to_string(),
    };
    let marker_path = path.join(forge_content::WORKSPACE_MARKER_FILE);
    fs::write(&marker_path, serde_json::to_vec(&marker)?)
        .map_err(|error| anyhow!("write workspace marker: {}", error.kind()))?;
    Ok(path)
}

pub fn record_attempt_workspace_materialized(
    cwd: &Path,
    attempt_id: &str,
    content_ref: &str,
) -> Result<()> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        tx.execute(
            "UPDATE attempt_workspaces
             SET materialized_content_ref = ?1, updated_at_ms = ?2
             WHERE repo_id = ?3 AND attempt_id = ?4",
            params![content_ref, now_ms(), context.repo_id, attempt_id],
        )?;
        Ok(())
    })
}

/// Write-binding verification (NER-134): refuse to act on the worktree under
/// `target_attempt_id` when the worktree is currently materialized for a
/// *different* attempt (`current_state.attached_attempt_id`).
///
/// The raw `attached_attempt_id` is consulted regardless of the attached attempt's
/// active/abandoned status: if the worktree holds attempt `W`'s content, recording
/// it under `X != W` is cross-attempt contamination even if `W` was later abandoned.
/// `None` (nothing materialized — the single-attempt v0 flow, or after a forced
/// detach) is always allowed: contamination requires a *different* attempt to have
/// been materialized, and materialization always sets `attached_attempt_id`. The
/// check runs only after attempt resolution succeeds, so an unqualified ambiguous
/// selector still surfaces `AmbiguousAttempt` first.
pub(crate) fn verify_worktree_binding(
    context: &RepositoryContext,
    target_attempt_id: &str,
) -> Result<()> {
    if let Some(workspace_attempt_id) = context.workspace_attempt_id.as_deref() {
        if workspace_attempt_id != target_attempt_id {
            return Err(ForgeError::AttemptWorktreeMismatch {
                requested_attempt: target_attempt_id.to_string(),
                attached_attempt: workspace_attempt_id.to_string(),
            }
            .into());
        }
        return Ok(());
    }
    if let Some(attached) = context.attached_attempt_id.as_deref() {
        if attached != target_attempt_id {
            return Err(ForgeError::AttemptWorktreeMismatch {
                requested_attempt: target_attempt_id.to_string(),
                attached_attempt: attached.to_string(),
            }
            .into());
        }
    }
    Ok(())
}

/// Resolve the `save` target attempt and verify the worktree binding *before* the
/// CLI snapshots the worktree (NER-134), so a mismatch fails fast without writing
/// orphan content objects. Returns the resolved attempt id; the CLI passes it back
/// as an explicit selector to [`save_snapshot`], whose own [`verify_worktree_binding`]
/// call is the authoritative guard. Pure read — takes no advisory lock.
pub fn verify_save_target(cwd: &Path, attempt_id: Option<&str>) -> Result<String> {
    let context = open_repository(cwd)?;
    let attempt = resolve_attempt_in_context(&context, attempt_id)?.attempt;
    verify_worktree_binding(&context, &attempt.attempt_id)?;
    Ok(attempt.attempt_id)
}

pub fn resolve_attempt(cwd: &Path, attempt_id: Option<&str>) -> Result<ResolvedAttempt> {
    let context = open_repository(cwd)?;
    resolve_attempt_in_context(&context, attempt_id)
}

pub(crate) fn resolve_attempt_in_context(
    context: &RepositoryContext,
    attempt_id: Option<&str>,
) -> Result<ResolvedAttempt> {
    if let Some(attempt_id) = attempt_id {
        let attempt =
            attempt_by_id(context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
                selector: attempt_id.to_string(),
            })?;
        return Ok(ResolvedAttempt { attempt });
    }

    if let Some(workspace_attempt_id) = context.workspace_attempt_id.as_deref() {
        if let Some(attempt) = attempt_by_id(context, workspace_attempt_id)? {
            if attempt.status == "active" {
                return Ok(ResolvedAttempt { attempt });
            }
        }
    }

    if let Some(attached_attempt_id) = context.attached_attempt_id.as_deref() {
        if let Some(attempt) = attempt_by_id(context, attached_attempt_id)? {
            if attempt.status == "active" {
                return Ok(ResolvedAttempt { attempt });
            }
        }
    }

    let attempts = active_attempts(context)?;
    match attempts.as_slice() {
        [] => Err(ForgeError::NoActiveAttempt.into()),
        [attempt] => Ok(ResolvedAttempt {
            attempt: attempt.clone(),
        }),
        _ => Err(ForgeError::AmbiguousAttempt {
            candidate_ids: attempts
                .iter()
                .map(|attempt| attempt.attempt_id.clone())
                .collect(),
        }
        .into()),
    }
}

pub(crate) fn attempt_by_id(
    context: &RepositoryContext,
    attempt_id: &str,
) -> Result<Option<AttemptRecord>> {
    let connection = open_connection(&context.database_path)?;
    connection
        .query_row(
            "SELECT a.id, a.intent_id, i.text, a.base_head, a.status
             FROM attempts a
             JOIN intents i ON i.id = a.intent_id
             WHERE a.repo_id = ?1 AND a.id = ?2",
            params![context.repo_id, attempt_id],
            |row| {
                Ok(AttemptRecord {
                    attempt_id: row.get(0)?,
                    intent_id: row.get(1)?,
                    intent: row.get(2)?,
                    base_head: row.get(3)?,
                    status: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn active_attempts(context: &RepositoryContext) -> Result<Vec<AttemptRecord>> {
    let connection = open_connection(&context.database_path)?;
    let mut statement = connection.prepare(
        "SELECT a.id, a.intent_id, i.text, a.base_head, a.status
         FROM attempts a
         JOIN intents i ON i.id = a.intent_id
         WHERE a.repo_id = ?1 AND a.status = 'active'
         ORDER BY a.created_at_ms ASC",
    )?;
    let rows = statement.query_map(params![context.repo_id], |row| {
        Ok(AttemptRecord {
            attempt_id: row.get(0)?,
            intent_id: row.get(1)?,
            intent: row.get(2)?,
            base_head: row.get(3)?,
            status: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn list_attempts(cwd: &Path) -> Result<Vec<AttemptSummary>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let mut statement = connection.prepare(
        "SELECT a.id, a.intent_id, i.text, a.base_head, a.status,
                COALESCE(aw.workspace_rel_path, '.forge/worktrees/' || a.id)
         FROM attempts a
         JOIN intents i ON i.id = a.intent_id
         LEFT JOIN attempt_workspaces aw ON aw.attempt_id = a.id AND aw.repo_id = a.repo_id
         WHERE a.repo_id = ?1
         ORDER BY a.created_at_ms ASC",
    )?;
    let attached = context.attached_attempt_id.clone();
    let rows = statement.query_map(params![context.repo_id], |row| {
        let attempt_id: String = row.get(0)?;
        Ok(AttemptSummary {
            attached: attached.as_deref() == Some(attempt_id.as_str()),
            attempt_id,
            intent_id: row.get(1)?,
            intent: row.get(2)?,
            base_head: row.get(3)?,
            status: row.get(4)?,
            workspace_path: row.get(5)?,
            workspace_role: WORKSPACE_ROLE_MATERIALIZATION_TARGET,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

/// The latest proposal's `content_ref` for an attempt — the diffable tree the pairwise
/// `compare --diff` path feeds to the CLI diff router. Errors `UnknownAttempt`
/// when the attempt does not exist, `NoProposal` when it has no proposal yet.
pub fn show_attempt(cwd: &Path, attempt_id: &str) -> Result<AttemptShowRecord> {
    let context = open_repository(cwd)?;
    let attempt =
        attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
            selector: attempt_id.to_string(),
        })?;
    Ok(AttemptShowRecord {
        attempt: AttemptSummary {
            attached: context.attached_attempt_id.as_deref() == Some(attempt.attempt_id.as_str()),
            attempt_id: attempt.attempt_id.clone(),
            intent_id: attempt.intent_id.clone(),
            intent: attempt.intent.clone(),
            base_head: attempt.base_head.clone(),
            status: attempt.status.clone(),
            workspace_path: attempt_workspace_rel_path(&context, &attempt.attempt_id)?,
            workspace_role: WORKSPACE_ROLE_MATERIALIZATION_TARGET,
        },
        latest_snapshot: latest_snapshot_for_attempt(&context, &attempt.attempt_id)?,
        latest_evidence: latest_evidence_for_attempt(&context, &attempt.attempt_id)?,
        proposals: proposal_metadata_for_attempt(&context, &attempt.attempt_id)?,
    })
}

pub fn attach_attempt(
    cwd: &Path,
    request_id: Option<String>,
    attempt_id: &str,
    content_ref: &str,
) -> Result<OperationViewResult> {
    let context = open_repository(cwd)?;
    let attempt =
        attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
            selector: attempt_id.to_string(),
        })?;
    let mut connection = open_connection(&context.database_path)?;
    let op = with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let op = insert_operation_view(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "attempt attach".to_string(),
                kind: "attempt_attached".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({
                    "lifecycle": "attempt_attached",
                    "attempt_id": attempt.attempt_id
                }),
            },
        )?;
        tx.execute(
            "UPDATE current_state SET attached_attempt_id = ?1 WHERE singleton = 1",
            params![attempt.attempt_id],
        )?;
        // NER-143 R1: `attempt attach` is the FIFTH materializing op — the CLI restores the
        // attached attempt's tree into the worktree before calling this. Set the expected
        // baseline atomically with the attach (code-review P1: without this, an attach-then-nav
        // spuriously fails DIRTY_WORKTREE because `expected` still points at the prior attempt's
        // tree). Same dedicated-UPDATE discipline as the other recorders (DR-F2).
        set_context_expected_content_ref(tx, &context, content_ref)?;
        Ok(op)
    })?;
    Ok(op)
}

pub fn attempt_materialization_ref(cwd: &Path, attempt_id: &str) -> Result<Option<String>> {
    let context = open_repository(cwd)?;
    let attempt =
        attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
            selector: attempt_id.to_string(),
        })?;
    Ok(latest_snapshot_for_attempt(&context, &attempt.attempt_id)?
        .map(|snapshot| snapshot.content_ref))
}

pub fn attempt_base_head(cwd: &Path, attempt_id: &str) -> Result<String> {
    let context = open_repository(cwd)?;
    Ok(attempt_by_id(&context, attempt_id)?
        .ok_or_else(|| ForgeError::UnknownAttempt {
            selector: attempt_id.to_string(),
        })?
        .base_head)
}

/// NER-382 workspace drift guard: refuse `attempt attach` when the target attempt's
/// workspace dir no longer holds the content the store recorded materializing into it
/// (`attempt_workspaces.materialized_content_ref`) — re-materializing would silently
/// discard those workspace edits. This is an EQUALITY check, not a diff: it delegates
/// to [`forge_content_native::tree_equality_drift`], the read-only, cache-free
/// primitive that enumerates the workspace dir under the SAME exclusion contract as
/// the native snapshot scanner (policy filter + `.forgeignore` + `.gitignore` rooted
/// at the workspace dir) and hash/byte-compares each file against the recorded tree.
/// A refusal leaves the workspace untouched.
///
/// The attempt's local private path labels are excluded from BOTH sides: an
/// in-workspace `save` records a private-EXCLUDED public tree while the private
/// files stay on disk, so counting them would be permanent false drift naming
/// private paths (and `--discard-workspace-changes` would then delete them).
///
/// Skipped (Ok) when nothing was ever materialized: a NULL/absent
/// `materialized_content_ref` (including every git-backend workspace, which never
/// records one) has no baseline to drift from.
pub fn verify_attempt_workspace_undrifted(cwd: &Path, attempt_id: &str) -> Result<()> {
    let context = open_repository(cwd)?;
    attempt_by_id(&context, attempt_id)?.ok_or_else(|| ForgeError::UnknownAttempt {
        selector: attempt_id.to_string(),
    })?;
    let connection = open_connection(&context.database_path)?;
    let recorded: Option<String> = connection
        .query_row(
            "SELECT materialized_content_ref FROM attempt_workspaces
             WHERE repo_id = ?1 AND attempt_id = ?2",
            params![context.repo_id, attempt_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(recorded) = recorded else {
        return Ok(());
    };
    // Only native `forge-tree:` refs are ever recorded (the git arm of workspace
    // materialization records nothing); stay permissive on any other family.
    let Some(tree_id) = recorded.strip_prefix(forge_content::FORGE_TREE_PREFIX) else {
        return Ok(());
    };
    // Same labels helper family `save` uses (`local_private_path_exclusions`), minus
    // its file-existence check: this guard only needs the label *paths*, and the
    // labeled files live in the workspace dir, not necessarily under `cwd`.
    let excluded: std::collections::BTreeSet<String> =
        local_private_path_labels(cwd, "attempt", attempt_id)?
            .into_iter()
            .map(|label| label.path)
            .collect();
    let workspace = context
        .root_path
        .join(attempt_workspace_rel_path(&context, attempt_id)?);
    let tree = forge_content_native::ObjectId::parse(tree_id)?;
    let drifted = forge_content_native::tree_equality_drift(
        &context.root_path,
        &workspace,
        &tree,
        &excluded,
    )?;
    if drifted.is_empty() {
        return Ok(());
    }
    Err(ForgeError::WorkspaceDrift { paths: drifted }.into())
}
