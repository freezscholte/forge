//! Doctor / signing two-sided coverage for the contract family (U9/KTD2).
//!
//! Split out of `contract.rs` as a behavior-preserving structural move (ADR-0001):
//! this module holds the `forge doctor` signature-pass helpers that recompute and
//! enumerate contract-family subjects, while the record/read/write surface stays
//! in `contract.rs`.
//!
//! `forge doctor` verifies the contract family from BOTH sides in one slice (KTD2):
//! - Signature pass (`signing::current_subject_digest`): recompute each signed
//!   subject's canonical digest from its CURRENT row via [`contract_subject_digest`]
//!   and compare to `signed_digest` — a field edit is a `DigestMismatch`, a deleted
//!   row a `SubjectMissing`.
//! - Expected-signed enumeration ([`expected_contract_signed_subjects`]): every
//!   contract-family row MUST carry a valid signature or it is a `MissingSignature`.
//! - Op-chain re-walk (doctor's `op_domain_digest`) recovers each op's folded digest
//!   from its view `subject_digest`, so op reorder/deletion is still a `broken_link`.

use super::contract::{
    contract_revision_digest, contract_run_digest, contract_stop_digest, contract_verdict_digest,
    map_contract_revision_row, map_contract_stop_row, ContractRunTaskInput,
    ContractRunVerdictInput, RecordContractRunInput, SUBJECT_KIND_CONTRACT,
    SUBJECT_KIND_CONTRACT_RUN, SUBJECT_KIND_CONTRACT_STOP, SUBJECT_KIND_CONTRACT_VERDICT,
};
use super::*;

/// The per-kind signature high-water mark for the contract family. The four kinds are
/// born signature-capable in migration 022 (no era where an unsigned contract row
/// could legitimately exist), so the grandfather boundary is 0 — every row is
/// expected-signed. Named so the pattern matches the evidence/decision markers.
pub(crate) const CONTRACT_SIGNATURE_HIGH_WATER: i64 = 0;

/// Recompute a contract-family subject's canonical content digest from its CURRENT
/// row content, for `forge doctor`'s signature pass (U9/KTD2). Returns `None` when
/// the subject row no longer exists (a deleted-row `SubjectMissing`), and `Some`
/// with the freshly recomputed digest otherwise — which the caller compares to the
/// stored `signed_digest` to detect an out-of-band field edit (`DigestMismatch`).
/// The four digest functions are the SAME ones the writes fold, so a healthy repo
/// recomputes bit-identically. `subject_id` is the signed row id (revision row id,
/// run id, stop id, or verdict id).
pub(crate) fn contract_subject_digest(
    conn: &Connection,
    subject_kind: &str,
    subject_id: &str,
) -> Result<Option<String>> {
    match subject_kind {
        SUBJECT_KIND_CONTRACT => {
            let record = conn
                .query_row(
                    "SELECT id, contract_id, revision, state, source_yaml, lint_clean,
                            predecessor_revision, resolution_kind, resolution_rationale,
                            content_hash, created_at_ms
                     FROM contract_revisions WHERE id = ?1",
                    params![subject_id],
                    map_contract_revision_row,
                )
                .optional()?;
            Ok(record.map(|r| {
                contract_revision_digest(
                    &r.contract_id,
                    r.revision,
                    &r.state,
                    &r.source_yaml,
                    r.lint_clean,
                    r.predecessor_revision,
                    r.resolution_kind.as_deref(),
                    r.resolution_rationale.as_deref(),
                    r.created_at_ms,
                )
            }))
        }
        SUBJECT_KIND_CONTRACT_RUN => {
            let run = conn
                .query_row(
                    "SELECT id, contract_id, revision, base_head, dependency_stack_json, outcome,
                            exit_code, agent_exit_code, patch_content_ref, content_hash, created_at_ms
                     FROM contract_runs WHERE id = ?1",
                    params![subject_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                            row.get::<_, i64>(10)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                contract_id,
                revision,
                base_head,
                dependency_stack_json,
                outcome,
                exit_code,
                agent_exit_code,
                patch_content_ref,
                created_at_ms,
            )) = run
            else {
                return Ok(None);
            };
            let mut statement = conn.prepare(
                "SELECT task_id, task_index, outcome, patch_content_ref, agent_exit_code,
                        agent_stdout_excerpt, agent_stderr_excerpt
                 FROM contract_run_tasks WHERE run_id = ?1 ORDER BY task_index",
            )?;
            let tasks = statement
                .query_map(params![subject_id], |row| {
                    Ok(ContractRunTaskInput {
                        task_id: row.get(0)?,
                        task_index: row.get(1)?,
                        outcome: row.get(2)?,
                        patch_content_ref: row.get(3)?,
                        agent_exit_code: row.get(4)?,
                        agent_stdout_excerpt: row.get(5)?,
                        agent_stderr_excerpt: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let input = RecordContractRunInput {
                contract_id,
                revision,
                base_head,
                dependency_stack_json,
                outcome,
                exit_code,
                agent_exit_code,
                patch_content_ref,
                tasks,
            };
            Ok(Some(contract_run_digest(&input, created_at_ms)))
        }
        SUBJECT_KIND_CONTRACT_STOP => {
            let stop = conn
                .query_row(
                    "SELECT id, contract_id, revision, run_id, task_id, what_needed, why_unanswered,
                            kind, evidence, malformed, state, resolution_kind, resolution_rationale,
                            resolving_revision, content_hash, created_at_ms, updated_at_ms
                     FROM contract_stops WHERE id = ?1",
                    params![subject_id],
                    map_contract_stop_row,
                )
                .optional()?;
            Ok(stop.map(|s| {
                contract_stop_digest(
                    &s.contract_id,
                    s.revision,
                    s.run_id.as_deref(),
                    s.task_id.as_deref(),
                    s.what_needed.as_deref(),
                    s.why_unanswered.as_deref(),
                    s.kind.as_deref(),
                    s.evidence.as_deref(),
                    s.malformed,
                    &s.state,
                    s.resolution_kind.as_deref(),
                    s.resolution_rationale.as_deref(),
                    s.resolving_revision,
                    s.created_at_ms,
                )
            }))
        }
        SUBJECT_KIND_CONTRACT_VERDICT => {
            let verdict = conn
                .query_row(
                    "SELECT run_id, task_id, verdict_kind, command, passed, detail, evidence_id,
                            created_at_ms
                     FROM contract_run_verdicts WHERE id = ?1",
                    params![subject_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            ContractRunVerdictInput {
                                task_id: row.get(1)?,
                                verdict_kind: row.get(2)?,
                                command: row.get(3)?,
                                passed: row.get::<_, i64>(4)? != 0,
                                detail: row.get(5)?,
                                evidence_id: row.get(6)?,
                            },
                            row.get::<_, i64>(7)?,
                        ))
                    },
                )
                .optional()?;
            Ok(verdict.map(|(run_id, input, created_at_ms)| {
                contract_verdict_digest(&run_id, &input, created_at_ms)
            }))
        }
        _ => Ok(None),
    }
}

/// Enumerate every contract-family row that MUST carry a valid local signature,
/// as `(subject_kind, subject_id, current_content_hash)` triples for
/// `signing::expected_signed_subjects` (U9/KTD2). All four kinds are born signed in
/// migration 022, so every row above [`CONTRACT_SIGNATURE_HIGH_WATER`] (i.e. every
/// row) is enumerated. The `content_hash` is the CURRENT digest; the signature pass
/// separately confirms it recomputes from the row content.
pub(crate) fn expected_contract_signed_subjects(
    conn: &Connection,
) -> Result<Vec<(String, String, String)>> {
    let mut subjects = Vec::new();
    for (kind, table) in [
        (SUBJECT_KIND_CONTRACT, "contract_revisions"),
        (SUBJECT_KIND_CONTRACT_RUN, "contract_runs"),
        (SUBJECT_KIND_CONTRACT_STOP, "contract_stops"),
        (SUBJECT_KIND_CONTRACT_VERDICT, "contract_run_verdicts"),
    ] {
        let sql = format!("SELECT id, content_hash FROM {table} WHERE rowid > ?1 ORDER BY rowid");
        let mut statement = conn.prepare(&sql)?;
        for row in statement.query_map(params![CONTRACT_SIGNATURE_HIGH_WATER], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
            let (id, digest) = row?;
            subjects.push((kind.to_string(), id, digest));
        }
    }
    Ok(subjects)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal native-backend repo: no git binary needed, and `init` seeds the
    /// `current_state` singleton the op-log chain advances.
    fn init_native_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temp dir");
        crate::init_repository(temp.path(), None, "native".to_string())
            .expect("init native repository");
        temp
    }

    fn frozen_input(contract_id: &str, yaml: &str) -> FreezeContractRevisionInput {
        FreezeContractRevisionInput {
            contract_id: contract_id.to_string(),
            source_yaml: yaml.to_string(),
            lint_clean: true,
            resolution_kind: None,
            resolution_rationale: None,
        }
    }

    // Two-sided doctor coverage over the full contract population, tamper
    // detection, and unsigned-row flagging live in the public-API integration test
    // `tests/contract_doctor.rs` (keeps this domain module under its line ceiling).
    // These inline tests cover the pub(crate) high-water/enumeration internals only.

    #[test]
    fn contract_signature_high_water_is_zero_and_enumerates_all_rows() {
        // The four kinds are born signed in migration 022 (no pre-signing era), so
        // the per-kind grandfather boundary is 0 and every frozen row is enumerated
        // as expected-signed (U9/KTD2).
        assert_eq!(CONTRACT_SIGNATURE_HIGH_WATER, 0);
        let temp = init_native_repo();
        let record = freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze");
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        let expected = expected_contract_signed_subjects(&connection).expect("enumerate");
        assert!(expected.iter().any(|(kind, id, digest)| {
            kind == SUBJECT_KIND_CONTRACT
                && id == &record.revision_row_id
                && digest == &record.content_hash
        }));
    }
}
