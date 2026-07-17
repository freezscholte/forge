//! CCX native contracts (NER U5): run-resolution and integrate-linkage store
//! helpers, split out of `contract.rs` so the run orchestration surface does not
//! push that module toward the 3000-line ceiling (a prior review flagged the
//! trajectory). The run/stop/verdict WRITE functions themselves stay in
//! `contract.rs`; this sibling holds the U5 read/link helpers the CLI needs:
//!
//! - resolve a run by run-id OR by a completed task-id (`contract_run_by_ref`);
//! - list open stops across a contract's dependency closure (`open_stops_for_contracts`),
//!   the Leg-3 refusal input (R10/AE2/AE9);
//! - decide whether a dependency contract is already accepted into HEAD
//!   (`contract_integration_accepted`), the integrate deps-gate (R27/KTD8);
//! - record the run↔attempt+intent integration link in the op-log spine
//!   (`record_contract_integration`), replay-safe under `--request-id` (R18/KTD6).

use super::*;

use crate::contract::{ContractRunRecord, ContractStopRecord};

/// The synthesized-intent marker prefix an `integrate` attempt's intent text
/// carries, so a later deps-gate can recognize an accepted contract integration
/// (KTD8). `contract <id>@rev<k> ...` — see [`contract_integration_intent_text`].
pub const CONTRACT_INTENT_PREFIX: &str = "contract ";

/// The synthesized intent text for an integration attempt (KTD8): stable, prefix
/// so an accepted-integration query can find it by `contract_id`.
pub fn contract_integration_intent_text(contract_id: &str, revision: i64, task_id: &str) -> String {
    format!("{CONTRACT_INTENT_PREFIX}{contract_id}@rev{revision} task {task_id}")
}

/// Resolve a run by its run-id, or — failing that — by a `task_id` recorded on one
/// of its per-task rows (KTD8/KTD9: `integrate <run-or-task-id>`). Returns the full
/// run record (including per-task rows) via [`crate::contract::contract_run`].
pub fn contract_run_by_ref(cwd: &Path, target: &str) -> Result<Option<ContractRunRecord>> {
    if let Some(run) = crate::contract::contract_run(cwd, target)? {
        return Ok(Some(run));
    }
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let run_id: Option<String> = connection
        .query_row(
            "SELECT run_id FROM contract_run_tasks
             WHERE repo_id = ?1 AND task_id = ?2
             ORDER BY created_at_ms DESC, id DESC LIMIT 1",
            params![context.repo_id, target],
            |row| row.get(0),
        )
        .optional()?;
    match run_id {
        Some(run_id) => crate::contract::contract_run(cwd, &run_id),
        None => Ok(None),
    }
}

/// Every OPEN stop whose `contract_id` is in `contract_ids` (a chain's dependency
/// closure), oldest first. The Leg-3 refusal (R10) names these stop ids.
pub fn open_stops_for_contracts(
    cwd: &Path,
    contract_ids: &[String],
) -> Result<Vec<ContractStopRecord>> {
    let mut blocking = Vec::new();
    for contract_id in contract_ids {
        blocking.extend(crate::contract::contract_stops(
            cwd,
            Some(contract_id),
            true,
        )?);
    }
    Ok(blocking)
}

/// Whether contract `dep_contract_id` is already accepted into HEAD (KTD8).
///
/// Anti-spoof (B3/KTD8): a dependency counts as accepted iff its task's completed
/// run was integrated by `forge contract integrate` AND that integration's proposal
/// carries an `accept` decision. The gate joins through the RECORDED integration
/// link — the `contract_integrated` op-log view whose `state_json.attempt_id`
/// [`record_contract_integration`] wrote — not through free intent text. Matching
/// `intents.text LIKE 'contract <dep>@%'` (the prior predicate) was spoofable: an
/// ordinary `forge start "contract <dep>@rev1 ..."` → save → propose → accept
/// produces an accepted decision whose intent text satisfies the LIKE without any
/// genuine contract run ever having been integrated. Binding to the integration
/// link closes that: an attacker cannot forge a `contract_integrated` op that links
/// to a real, signed contract run they did not produce.
pub fn contract_integration_accepted(cwd: &Path, dep_contract_id: &str) -> Result<bool> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let accepted: bool = connection
        .query_row(
            "SELECT 1
             FROM operations o
             JOIN views v ON v.id = o.resulting_view_id
             JOIN attempts a ON a.id = json_extract(v.state_json, '$.attempt_id')
             JOIN proposals p ON p.attempt_id = a.id
             JOIN decisions d ON d.proposal_id = p.id
             WHERE o.repo_id = ?1
               AND o.kind = 'contract_integrated'
               AND json_extract(v.state_json, '$.contract_id') = ?2
               AND d.decision = 'accept'
             LIMIT 1",
            params![context.repo_id, dep_contract_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(accepted)
}

/// The recorded run↔attempt+intent integration link (R27/KTD8).
#[derive(Debug, Clone, Serialize)]
pub struct ContractIntegrationRecord {
    pub run_id: String,
    pub contract_id: String,
    pub revision: i64,
    pub task_id: String,
    pub attempt_id: String,
    pub intent_id: String,
}

/// Record the integration link in the op-log spine (chained, replay-safe). The
/// attempt itself is created by the CLI via `start_attempt`; this folds the link
/// ids into the ledger so `integrate` is a first-class, replayable operation
/// (R18/KTD6) whose `--request-id` retry never re-creates the attempt.
pub fn record_contract_integration(
    cwd: &Path,
    request_id: Option<String>,
    link: ContractIntegrationRecord,
) -> Result<ContractIntegrationRecord> {
    let context = open_repository(cwd)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let mut hasher = Sha256::new();
        for part in [
            link.run_id.as_str(),
            link.contract_id.as_str(),
            link.task_id.as_str(),
            link.attempt_id.as_str(),
            link.intent_id.as_str(),
        ] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        let digest = hasher.finalize();
        let content_hash = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        insert_operation_view_chained(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "contract integrate".to_string(),
                kind: "contract_integrated".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({
                    "lifecycle": "contract_integrated",
                    "run_id": link.run_id,
                    "contract_id": link.contract_id,
                    "revision": link.revision,
                    "task_id": link.task_id,
                    "attempt_id": link.attempt_id,
                    "intent_id": link.intent_id,
                    // The exact domain digest this op folds into its chain link. Doctor's
                    // op-chain re-walk (`op_domain_digest`, U9/KTD2) recovers it from HERE
                    // — uniform with every other contract-family op. Omitting it made the
                    // integrate op an unrecoverable `broken_link` under `forge doctor`
                    // (doctor recomputed with digest=None while this op folded Some(..)).
                    "subject_digest": content_hash,
                }),
            },
            Some(&content_hash),
        )?;
        Ok(link.clone())
    })
}
