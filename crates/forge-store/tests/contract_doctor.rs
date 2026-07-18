//! U9: two-sided `forge doctor` coverage over the contract family (R17/KTD2).
//!
//! These drive the PUBLIC store API against a real native repo and assert that
//! `doctor` re-walks the tamper chain AND checks `expected_signed_subjects` for every
//! new kind (contract, run, stop, verdict). They live here rather than inline so the
//! `contract.rs` domain module stays under its 3000-line ceiling; the pub(crate)
//! high-water/enumeration internals keep a tiny inline unit test alongside the code.

use forge_store::{
    doctor, freeze_contract_revision, record_contract_run, record_contract_run_with_stop,
    record_contract_run_with_verdicts, record_contract_verify_verdicts, resolve_contract_stop,
    ContractRunTaskInput, ContractRunVerdictInput, FreezeContractRevisionInput,
    OpenContractStopInput, RecordContractRunInput, ResolveContractStopInput, SignatureFindingKind,
    StopFieldReconstruction,
};
use rusqlite::params;
use std::path::Path;

fn init_native_repo() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("temp dir");
    forge_store::init_repository(temp.path(), None, "native".to_string())
        .expect("init native repository");
    temp
}

fn frozen(contract_id: &str, yaml: &str) -> FreezeContractRevisionInput {
    FreezeContractRevisionInput {
        contract_id: contract_id.to_string(),
        source_yaml: yaml.to_string(),
        lint_clean: true,
        resolution_kind: None,
        resolution_rationale: None,
    }
}

fn one_task(contract_id: &str, outcome: &str) -> ContractRunTaskInput {
    ContractRunTaskInput {
        task_id: contract_id.to_string(),
        task_index: 0,
        outcome: outcome.to_string(),
        patch_content_ref: None,
        agent_exit_code: Some(0),
        agent_stdout_excerpt: None,
        agent_stderr_excerpt: None,
    }
}

fn run_input(contract_id: &str, revision: i64, outcome: &str, exit: i64) -> RecordContractRunInput {
    RecordContractRunInput {
        contract_id: contract_id.to_string(),
        revision,
        base_head: Some("HEAD0".to_string()),
        dependency_stack_json: None,
        outcome: outcome.to_string(),
        exit_code: exit,
        agent_exit_code: Some(0),
        patch_content_ref: None,
        tasks: vec![one_task(
            contract_id,
            if outcome == "completed" {
                "completed"
            } else if outcome == "stopped" {
                "stopped"
            } else {
                "failed"
            },
        )],
    }
}

fn stop_input(contract_id: &str, revision: i64, what: &str) -> OpenContractStopInput {
    OpenContractStopInput {
        contract_id: contract_id.to_string(),
        revision,
        run_id: None,
        task_id: Some(contract_id.to_string()),
        what_needed: Some(what.to_string()),
        why_unanswered: Some("brief omits it".to_string()),
        kind: Some("blocking".to_string()),
        evidence: Some("src/lib.rs:1".to_string()),
        malformed: false,
    }
}

/// Build a repo populated with EVERY contract-family kind and lifecycle state, so a
/// green `doctor` proves the two-sided (signature + op-chain) coverage over the full
/// population (U9 flagship, R17/KTD2).
fn full_population_repo() -> tempfile::TempDir {
    let temp = init_native_repo();
    freeze_contract_revision(temp.path(), None, frozen("_global-policy", "policy\n"))
        .expect("freeze policy");
    freeze_contract_revision(temp.path(), None, frozen("c1", "id: c1\n")).expect("freeze c1");

    // A plain completed run.
    record_contract_run(temp.path(), None, run_input("c1", 1, "completed", 0)).expect("completed");

    // A blast-violated run WITH verdicts (atomic run + verdict rows).
    record_contract_run_with_verdicts(
        temp.path(),
        None,
        run_input("c1", 1, "blast_violation", 3),
        vec![ContractRunVerdictInput {
            revision: 1,
            task_id: Some("c1".to_string()),
            verdict_kind: "blast".to_string(),
            command: None,
            passed: false,
            detail: Some("forbidden path .forge/loot".to_string()),
            evidence_id: None,
        }],
    )
    .expect("blast run + verdicts");

    // A verified run: a completed run plus fix/guard/aggregate verify verdicts.
    let verified =
        record_contract_run(temp.path(), None, run_input("c1", 1, "completed", 0)).expect("run");
    record_contract_verify_verdicts(
        temp.path(),
        None,
        &verified.run_id,
        vec![
            ContractRunVerdictInput {
                revision: 1,
                task_id: Some("c1".to_string()),
                verdict_kind: "fix".to_string(),
                command: Some("cargo test".to_string()),
                passed: true,
                detail: None,
                evidence_id: None,
            },
            ContractRunVerdictInput {
                revision: 1,
                task_id: Some("c1".to_string()),
                verdict_kind: "guard".to_string(),
                command: Some("cargo clippy".to_string()),
                passed: true,
                detail: None,
                evidence_id: None,
            },
            ContractRunVerdictInput {
                revision: 1,
                task_id: Some("c1".to_string()),
                verdict_kind: "aggregate".to_string(),
                command: None,
                passed: true,
                detail: None,
                evidence_id: None,
            },
        ],
        "passed",
        0,
    )
    .expect("verify verdicts");

    // A stopped run + open stop, then RESOLVE it (the mutable stop re-sign path plus a
    // bump revision).
    let (_run, stop) = record_contract_run_with_stop(
        temp.path(),
        None,
        run_input("c1", 1, "stopped", 2),
        stop_input("c1", 1, "need the shape"),
    )
    .expect("stopped run + stop");
    resolve_contract_stop(
        temp.path(),
        None,
        ResolveContractStopInput {
            stop_id: stop.stop_id.clone(),
            resolution_kind: "rejection".to_string(),
            resolution_rationale: Some("brief already covers it".to_string()),
            source_yaml: "id: c1\n".to_string(),
            reconstruction: StopFieldReconstruction::default(),
        },
    )
    .expect("resolve stop");

    // A SECOND stopped run leaving an OPEN stop (open-lifecycle coverage) against rev 2.
    record_contract_run_with_stop(
        temp.path(),
        None,
        run_input("c1", 2, "stopped", 2),
        stop_input("c1", 2, "still unclear"),
    )
    .expect("second stopped run + open stop");
    temp
}

fn open_db(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(root.join(".forge/forge.db")).expect("open db")
}

#[test]
fn doctor_green_over_full_contract_population() {
    // The U9 flagship: doctor re-walks the tamper chain AND checks
    // expected_signed_subjects for every new kind (frozen contracts incl. global
    // policy, completed/stopped/blast-violated/verified runs, resolved + open stops,
    // and blast/fix/guard/aggregate verdicts) with zero issues (R17/KTD2).
    let temp = full_population_repo();
    let report = doctor(temp.path()).expect("doctor");
    assert!(
        report.ok,
        "doctor must be green over the full contract population: {:?}",
        report.issues
    );
    assert!(
        report.signature_issues.is_empty(),
        "no signature findings expected: {:?}",
        report.signature_issues
    );
    assert!(
        report.tampered_rows.is_empty(),
        "no tampered rows expected: {:?}",
        report.tampered_rows
    );
}

#[test]
fn tampered_stop_field_fails_doctor() {
    // Flip a byte in a stop's signed free-text field via direct SQL. doctor's
    // signature pass recomputes the stop digest from the row and no longer matches the
    // signed digest → a DigestMismatch finding (row-content tamper detection).
    let temp = init_native_repo();
    freeze_contract_revision(temp.path(), None, frozen("c1", "id: c1\n")).expect("freeze");
    let (_run, stop) = record_contract_run_with_stop(
        temp.path(),
        None,
        run_input("c1", 1, "stopped", 2),
        stop_input("c1", 1, "need the shape"),
    )
    .expect("stopped run + stop");

    open_db(temp.path())
        .execute(
            "UPDATE contract_stops SET what_needed = 'TAMPERED' WHERE id = ?1",
            params![stop.stop_id],
        )
        .expect("tamper");

    let report = doctor(temp.path()).expect("doctor");
    assert!(!report.ok, "doctor must fail over a tampered stop");
    assert!(
        report.signature_issues.iter().any(|finding| {
            finding.subject_kind == "contract_stop"
                && finding.subject_id == stop.stop_id
                && finding.kind == SignatureFindingKind::DigestMismatch
        }),
        "expected a DigestMismatch for the tampered stop: {:?}",
        report.signature_issues
    );
}

#[test]
fn unsigned_contract_row_is_flagged() {
    // expected_signed_subjects enumeration is REAL, not vacuous: a contract-family row
    // whose signature is removed is a MissingSignature. (The doctor-green flagship
    // proves the no-false-positive direction: legitimately-signed rows never flagged.)
    let temp = init_native_repo();
    let record =
        freeze_contract_revision(temp.path(), None, frozen("c1", "id: c1\n")).expect("freeze");
    open_db(temp.path())
        .execute(
            "DELETE FROM ledger_signatures WHERE subject_kind = 'contract' AND subject_id = ?1",
            params![record.revision_row_id],
        )
        .expect("delete signature");
    let report = doctor(temp.path()).expect("doctor");
    assert!(!report.ok, "an unsigned contract row must fail doctor");
    assert!(
        report.signature_issues.iter().any(|finding| {
            finding.subject_kind == "contract"
                && finding.subject_id == record.revision_row_id
                && finding.kind == SignatureFindingKind::MissingSignature
        }),
        "expected MissingSignature for the unsigned revision: {:?}",
        report.signature_issues
    );
}

#[test]
fn co_deleted_stop_row_and_signature_flagged_by_doctor() {
    // F5: a contract-family op recovers its folded digest from the FROZEN op
    // state_json, and the signature pass only fires while ledger_signatures survive.
    // So co-deleting a stop ROW and its signature rows evades BOTH passes — the exact
    // hole that would leave doctor green and unblock Leg-3. The referenced-row
    // cross-check closes it.
    let temp = init_native_repo();
    freeze_contract_revision(temp.path(), None, frozen("c1", "id: c1\n")).expect("freeze");
    let (_run, stop) = record_contract_run_with_stop(
        temp.path(),
        None,
        run_input("c1", 1, "stopped", 2),
        stop_input("c1", 1, "need the shape"),
    )
    .expect("stopped run + stop");

    let db = open_db(temp.path());
    db.execute(
        "DELETE FROM ledger_signatures WHERE subject_kind = 'contract_stop' AND subject_id = ?1",
        params![stop.stop_id],
    )
    .expect("delete stop signature");
    db.execute(
        "DELETE FROM contract_stops WHERE id = ?1",
        params![stop.stop_id],
    )
    .expect("delete stop row");

    let report = doctor(temp.path()).expect("doctor");
    assert!(
        !report.ok,
        "a co-deleted stop row + signature must fail doctor (F5)"
    );
    assert!(
        report.contract_row_issues.iter().any(|finding| {
            finding.table == "contract_stops" && finding.subject_id == stop.stop_id
        }),
        "expected a ReferencedRowMissing for the deleted stop: {:?}",
        report.contract_row_issues
    );
    // The signature pass is genuinely blind here (its signature was co-deleted), which
    // is why the cross-check is load-bearing.
    assert!(
        !report
            .signature_issues
            .iter()
            .any(|f| f.subject_id == stop.stop_id),
        "the co-deleted signature evades the signature pass by construction"
    );
}

#[test]
fn co_deleted_run_row_and_signature_flagged_by_doctor() {
    // F5: same hole for a run row + its signature.
    let temp = init_native_repo();
    freeze_contract_revision(temp.path(), None, frozen("c1", "id: c1\n")).expect("freeze");
    let run =
        record_contract_run(temp.path(), None, run_input("c1", 1, "completed", 0)).expect("run");

    let db = open_db(temp.path());
    db.execute(
        "DELETE FROM ledger_signatures WHERE subject_kind = 'contract_run' AND subject_id = ?1",
        params![run.run_id],
    )
    .expect("delete run signature");
    // Per-task rows FK-reference the run; drop them before the run row.
    db.execute(
        "DELETE FROM contract_run_tasks WHERE run_id = ?1",
        params![run.run_id],
    )
    .expect("delete task rows");
    db.execute(
        "DELETE FROM contract_runs WHERE id = ?1",
        params![run.run_id],
    )
    .expect("delete run row");

    let report = doctor(temp.path()).expect("doctor");
    assert!(
        !report.ok,
        "a co-deleted run row + signature must fail doctor (F5)"
    );
    assert!(
        report.contract_row_issues.iter().any(|finding| {
            finding.table == "contract_runs" && finding.subject_id == run.run_id
        }),
        "expected a ReferencedRowMissing for the deleted run: {:?}",
        report.contract_row_issues
    );
}
