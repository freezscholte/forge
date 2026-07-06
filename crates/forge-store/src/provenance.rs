//! NER-362 slice 4: ledger enrichment for blame — a read-only presentation
//! detail over `intents` / `decisions` / `check_results` rows, keyed by ids a
//! blame payload already carries. Tip resolution stays owned by the blame
//! path (slice 3): this module never reads the ledger to pick a tip, and
//! unknown or missing ids degrade to `None` fields rather than erroring so
//! blame over history that predates some ledger rows still renders fully.

use crate::{open_connection, open_repository};
use anyhow::Result;
use rusqlite::OptionalExtension;
use std::path::Path;

/// Ledger detail for one distinct (intent, revision, decision) tuple.
#[derive(Debug, Clone)]
pub struct ProvenanceDetail {
    pub intent_id: String,
    pub intent_title: Option<String>,
    pub decision_id: Option<String>,
    pub decision_status: Option<String>,
    pub check_status: Option<String>,
}

/// Look up the enrichment detail for one (intent, revision, decision) tuple.
///
/// Read-only: opens the repository database, runs at most three point
/// lookups, and writes nothing. Callers deduplicate per distinct tuple so the
/// query count is O(distinct commits), not O(lines).
pub fn provenance_detail(
    cwd: &Path,
    intent_id: &str,
    proposal_revision_id: Option<&str>,
    decision_id: Option<&str>,
) -> Result<ProvenanceDetail> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let intent_title: Option<String> = connection
        .query_row(
            "SELECT text FROM intents WHERE id = ?1",
            [intent_id],
            |row| row.get(0),
        )
        .optional()?;
    let decision_status: Option<String> = match decision_id {
        Some(id) => connection
            .query_row(
                "SELECT decision FROM decisions WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?,
        None => None,
    };
    let check_status: Option<String> = match proposal_revision_id {
        Some(id) => connection
            .query_row(
                "SELECT status FROM check_results WHERE proposal_revision_id = ?1
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                [id],
                |row| row.get(0),
            )
            .optional()?,
        None => None,
    };
    Ok(ProvenanceDetail {
        intent_id: intent_id.to_string(),
        intent_title,
        decision_id: decision_id.map(str::to_string),
        decision_status,
        check_status,
    })
}
