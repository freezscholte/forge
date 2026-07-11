mod common;

use common::{forge_in, TestRepo};
use serde_json::Value;
use std::collections::HashSet;

/// Every `ForgeError` code the published registry must name, mirrored from the
/// store enum (kept in lockstep by the store's drift-guard test).
const FORGE_ERROR_CODES: &[&str] = &[
    "STALE_BASE",
    "DIRTY_WORKTREE",
    "AMBIGUOUS_ATTEMPT",
    "UNKNOWN_ATTEMPT",
    "AMBIGUOUS_PROPOSAL",
    "UNKNOWN_PROPOSAL",
    "UNKNOWN_INTENT",
    "NO_ACTIVE_ATTEMPT",
    "NO_SNAPSHOT",
    "NO_PROPOSAL",
    "NOT_ACCEPTED",
    "REJECTED",
    "BRANCH_EXISTS",
    "NOT_INITIALIZED",
    "REQUEST_ID_CONFLICT",
    "CONFLICT",
    "SCHEMA_VERSION_UNSUPPORTED",
    "MIGRATION_FAILED",
    "ATTEMPT_WORKTREE_MISMATCH",
    "CHECK_NOT_PASSED",
    "EVIDENCE_TAMPERED",
    "PROVENANCE_MISMATCH",
    "LOCAL_SIGNATURE_MISMATCH",
    "MISSING_PROVENANCE_TRAILER",
    "NATIVE_HISTORY_CORRUPT",
    "CONFLICT_SET_NOT_FOUND",
    "GC_PLAN_CHANGED",
    "UNSUPPORTED_CONTENT_BACKEND",
    "TRUST_POLICY_UNMET",
    "UNSUPPORTED_TRUST_LEVEL",
    "UNSUPPORTED_STRUCTURED_GATE",
    "VISIBILITY_POLICY_INVALID",
    "VISIBILITY_POLICY_UNMET",
    "EMBARGO_WORKFLOW_REQUIRED",
    "EMBARGO_STATE_INVALID",
    "ORG_NOT_ENABLED",
    "ORG_ALREADY_ENABLED",
    "ORG_AUTHORITY_REQUIRED",
    "PRIVATE_CONTENT_INVALID",
    "PRIVATE_DECRYPT_AUTHORITY_MISSING",
    "CONTRACT_LINT_FAILED",
    "CONTRACT_NOT_FROZEN",
    "CONTRACT_OPEN_STOP",
    "STALE_UNKNOWN_FILE",
    "CONTRACT_STOP_MALFORMED",
    "CONTRACT_BLAST_VIOLATION",
    "CONTRACT_GUARD_REGRESSED",
    "CONTRACT_FIX_FAILED",
    "CONTRACT_GRAMMAR_VIOLATION",
];

/// The CCX contract typed codes (KTD10) that U2 registers. Kept as a named subset
/// so the round-trip assertion below reads as "every contract code an agent must be
/// able to enumerate via `forge schema` is present".
const CONTRACT_ERROR_CODES: &[&str] = &[
    "CONTRACT_LINT_FAILED",
    "CONTRACT_NOT_FROZEN",
    "CONTRACT_OPEN_STOP",
    "STALE_UNKNOWN_FILE",
    "CONTRACT_STOP_MALFORMED",
    "CONTRACT_BLAST_VIOLATION",
    "CONTRACT_GUARD_REGRESSED",
    "CONTRACT_FIX_FAILED",
    "CONTRACT_GRAMMAR_VIOLATION",
];

/// Run `forge schema --json` and return the full response envelope.
fn schema_envelope(path: &std::path::Path) -> Value {
    let output = forge_in(path)
        .args(["--json", "schema"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).expect("valid json contract")
}

/// The contract document itself lives under the envelope's `data` field.
fn schema_in(path: &std::path::Path) -> Value {
    schema_envelope(path)["data"].clone()
}

#[test]
fn schema_emits_versioned_contract_without_a_repo() {
    // A bare temp dir with no .forge and no git: the contract is static and must
    // not depend on a repository, migrate, lock, or cwd state.
    let temp = tempfile::tempdir().expect("temp dir");
    let envelope = schema_envelope(temp.path());
    // The envelope itself, and the contract document it carries, both pin the
    // schema version.
    assert_eq!(envelope["schema_version"], "forge.cli.v0");
    assert_eq!(envelope["status"], "success");
    let doc = &envelope["data"];
    assert_eq!(doc["schema_version"], "forge.cli.v0");
    assert!(doc["notes"].is_object(), "contract carries notes");
    assert!(doc["notes"]["retryable"].is_string());
    assert!(doc["notes"]["secret_protection"].is_string());
    assert!(doc["envelope"].is_object());
}

#[test]
fn schema_emits_the_same_contract_inside_a_repo() {
    // The contract is static: running inside a real git repo (even an
    // un-`init`-ed one) must produce the identical document — `schema` never
    // touches the repository, migrate, or the lock.
    let repo = TestRepo::new_git();
    let in_repo = schema_in(repo.path());
    let bare = schema_in(tempfile::tempdir().expect("temp dir").path());
    assert_eq!(in_repo, bare, "schema contract must be repo-independent");
}

#[test]
fn registry_contains_every_forge_error_code_plus_lock_timeout() {
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());

    let codes: HashSet<String> = doc["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .map(|entry| entry["code"].as_str().expect("code string").to_string())
        .collect();

    for code in FORGE_ERROR_CODES {
        assert!(
            codes.contains(*code),
            "published registry is missing ForgeError code {code}"
        );
    }
    assert!(
        codes.contains("LOCK_TIMEOUT"),
        "published registry is missing CLI code LOCK_TIMEOUT"
    );
}

#[test]
fn schema_registers_every_contract_code_as_non_retryable() {
    // CCX U2 (KTD10/R25): every contract typed code round-trips through
    // `forge schema --json` — a cold-start agent operator can enumerate them — and
    // each is a non-retryable domain outcome (no after_ms backoff).
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let errors = doc["errors"].as_array().expect("errors array");

    for code in CONTRACT_ERROR_CODES {
        let entry = errors
            .iter()
            .find(|entry| entry["code"] == *code)
            .unwrap_or_else(|| panic!("published registry is missing contract code {code}"));
        assert_eq!(entry["retryable"], false, "{code} must be non-retryable");
        assert!(entry["after_ms"].is_null(), "{code} carries no backoff");
        assert!(
            entry["details_keys"].is_array(),
            "{code} documents its details keys"
        );
    }
}

#[test]
fn schema_includes_review_surface_commands() {
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let commands: HashSet<String> = doc["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .map(|entry| entry["command"].as_str().expect("command").to_string())
        .collect();

    assert!(commands.contains("review show"));
    assert!(commands.contains("review export"));
    assert!(commands.contains("review open"));
}

#[test]
fn conflict_and_lock_timeout_are_retryable() {
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let errors = doc["errors"].as_array().expect("errors array");

    let find = |code: &str| -> &Value {
        errors
            .iter()
            .find(|entry| entry["code"] == code)
            .unwrap_or_else(|| panic!("registry entry for {code}"))
    };

    assert_eq!(find("CONFLICT")["retryable"], true);
    assert_eq!(find("LOCK_TIMEOUT")["retryable"], true);
    // A spot-check that a deterministic code is non-retryable.
    assert_eq!(find("NOT_INITIALIZED")["retryable"], false);
}

#[test]
fn lock_timeout_and_conflict_after_ms_share_the_backoff_constant() {
    // FIX E: the three historically-duplicated `50` backoff constants are now one
    // shared `forge_protocol::RETRY_BACKOFF_MS`. The published LOCK_TIMEOUT and
    // CONFLICT `after_ms` must both equal it, so a future edit to the constant can
    // never silently desync the published contract from the runtime classifier.
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let errors = doc["errors"].as_array().expect("errors array");
    let after_ms = |code: &str| -> u64 {
        errors
            .iter()
            .find(|entry| entry["code"] == code)
            .unwrap_or_else(|| panic!("registry entry for {code}"))["after_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("{code} after_ms is a number"))
    };
    assert_eq!(after_ms("LOCK_TIMEOUT"), forge_protocol::RETRY_BACKOFF_MS);
    assert_eq!(after_ms("CONFLICT"), forge_protocol::RETRY_BACKOFF_MS);
}

/// Every CLI-level code that `main.rs` can construct directly (never via
/// `ForgeError`). FIX G drift guard: a CLI code added to `main.rs` but omitted from
/// `schema.rs`'s hand-append is caught here.
const CLI_LEVEL_CODES: &[&str] = &[
    "LOCK_TIMEOUT",
    "COMMAND_FAILED",
    "NOT_A_GIT_REPOSITORY",
    "UNKNOWN_ARGUMENT",
    "MISSING_ARGUMENT",
    "USAGE_ERROR",
    "CONFIRMATION_REQUIRED",
];

#[test]
fn registry_contains_every_cli_level_code() {
    // FIX G: assert the published registry names the COMPLETE set of CLI-level
    // codes in addition to every ForgeError code — so a future hand-appended CLI
    // code that is forgotten in schema.rs fails this test.
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let codes: HashSet<String> = doc["errors"]
        .as_array()
        .expect("errors array")
        .iter()
        .map(|entry| entry["code"].as_str().expect("code string").to_string())
        .collect();

    for code in CLI_LEVEL_CODES {
        assert!(
            codes.contains(*code),
            "published registry is missing CLI-level code {code}"
        );
    }
    for code in FORGE_ERROR_CODES {
        assert!(
            codes.contains(*code),
            "published registry is missing ForgeError code {code}"
        );
    }
}

#[test]
fn commands_list_the_lifecycle() {
    let temp = tempfile::tempdir().expect("temp dir");
    let doc = schema_in(temp.path());
    let commands: HashSet<String> = doc["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .map(|entry| {
            entry["command"]
                .as_str()
                .expect("command string")
                .to_string()
        })
        .collect();

    for expected in [
        "init",
        "save",
        "accept",
        "diff",
        "merge",
        "conflict list",
        "conflict show",
        "conflict resolve",
        "trust policy",
        "visibility policy",
        "visibility set",
        "visibility path set",
        "visibility grant",
        "visibility revoke",
        "visibility check",
        "key status",
        "key rotate",
        "org status",
        "org init",
        "org encryption bind-local",
        "org decrypt-authority",
        "sync export",
        "sync inspect",
        "sync fetch",
        "sync pull",
        "sync push",
        "export branch",
        "schema",
    ] {
        assert!(
            commands.contains(expected),
            "commands[] is missing {expected}"
        );
    }
}
