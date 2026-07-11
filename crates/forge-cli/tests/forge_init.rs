mod common;

use common::{forge_in, TestRepo};
use predicates::prelude::*;
use rusqlite::Connection;
use serde_json::Value;

#[test]
fn version_exits_successfully_without_subcommand() {
    let assert = assert_cmd::Command::cargo_bin("forge")
        .expect("forge binary")
        .arg("--version")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.starts_with("forge "),
        "version output should name the binary, got {stdout:?}"
    );
}

#[test]
fn stubbed_command_returns_json_envelope_and_echoes_request_id() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .args(["--json", "--request-id", "req-u1", "start", "tight scope"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["schema_version"], "forge.cli.v0");
    assert_eq!(json["command"], "start");
    assert_eq!(json["request_id"], "req-u1");
    assert_eq!(json["status"], "error");
    assert!(json["data"].is_object());
    assert!(json["warnings"].as_array().unwrap().is_empty());
    assert_eq!(json["errors"][0]["code"], "NOT_INITIALIZED");
    assert_eq!(json["retry"]["retryable"], false);
}

#[test]
fn confirmation_command_returns_structured_error_in_json_mode() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .args(["--json", "restore", "snap_123"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["command"], "restore");
    assert_eq!(json["errors"][0]["code"], "CONFIRMATION_REQUIRED");
    assert_eq!(json["errors"][0]["details"]["snapshot_id"], "snap_123");
}

#[test]
fn restore_requires_yes_in_human_mode() {
    let repo = TestRepo::new_git();

    repo.forge()
        .args(["restore", "snap_123"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("restore requires --yes"));
}

#[test]
fn human_stub_output_is_concise() {
    let repo = TestRepo::new_git();

    repo.forge()
        .arg("save")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not initialized"))
        .stdout(predicate::str::is_empty());
}

#[test]
fn init_creates_sqlite_metadata_and_initial_operation_view() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .args(["--json", "--request-id", "req-init", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["schema_version"], "forge.cli.v0");
    assert_eq!(json["command"], "init");
    assert_eq!(json["request_id"], "req-init");
    assert_eq!(json["status"], "success");
    assert_eq!(json["data"]["already_initialized"], false);
    assert!(json["operation_id"].as_str().unwrap().starts_with("op_"));

    let db_path = repo.path().join(".forge/forge.db");
    assert!(db_path.exists());

    let connection = Connection::open(db_path).expect("open forge db");
    let counts: (i64, i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM repositories),
                (SELECT COUNT(*) FROM operations),
                (SELECT COUNT(*) FROM views)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("count metadata");
    assert_eq!(counts, (1, 1, 1));

    let current: (String, String) = connection
        .query_row(
            "SELECT current_operation_id, current_view_id FROM current_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current state");
    assert_eq!(current.0, json["data"]["current_operation_id"]);
    assert_eq!(current.1, json["data"]["current_view_id"]);
}

#[test]
fn init_is_idempotent() {
    let repo = TestRepo::new_git();

    repo.forge().args(["--json", "init"]).assert().success();
    let output = repo
        .forge()
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["data"]["already_initialized"], true);

    let connection = Connection::open(repo.path().join(".forge/forge.db")).expect("open forge db");
    let repo_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM repositories", [], |row| row.get(0))
        .expect("count repositories");
    assert_eq!(repo_count, 1);
}

#[test]
fn at_head_database_reports_healthy_schema_on_normal_command() {
    // This test previously DROPped `content_backend` while leaving version=2 and
    // asserted the next command auto-repaired the column. The NER-133 U3
    // version-gated migration runner deliberately does NOT auto-repair structural
    // drift on an at-HEAD DB (it skips when MAX(version) == HEAD); genuine v1->v2
    // upgrade is covered by `forge-store`'s `migrations::tests` (genesis case B),
    // where the FK on `attached_attempt_id` can be handled without the artificial
    // one-column state that DROP-on-a-v2-DB would create. Here we assert the new
    // behavior: a normal command on an unmodified at-HEAD DB reports a healthy
    // schema at the current head.
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();

    let output = repo
        .forge()
        .args(["--json", "doctor"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["data"]["ok"], true);
    assert_eq!(json["data"]["schema_version"], 22);
}

/// FIX K (U3): structural drift on an at-HEAD (version=2) DB is NOT auto-repaired
/// by the version-gated migration runner — the runner skips entirely when
/// MAX(version) == HEAD, so it never re-runs 002 to re-add a manually-dropped
/// column. The drift is instead SURFACED, not silently fixed.
///
/// We DROP `repositories.content_backend` from an at-HEAD DB, run a normal command,
/// and assert: (a) the column is NOT re-added (proving no auto-repair — the
/// documented capability change vs. the old unconditional ALTERs), and (b) the
/// drift surfaces rather than being swallowed. Note: because `open_repository`
/// SELECTs `content_backend`, both the normal command and `forge doctor` surface
/// the structural drift as an error rather than a clean `issues[]` drift report —
/// the achievable end-to-end signal is that doctor does NOT report `ok: true` and
/// the column was never auto-repaired.
#[test]
fn at_head_structural_drift_is_surfaced_not_auto_repaired() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();

    let db_path = repo.path().join(".forge/forge.db");
    {
        let connection = Connection::open(&db_path).expect("open forge db");
        // Drop a HEAD-schema column while leaving MAX(version) at HEAD.
        connection
            .execute("ALTER TABLE repositories DROP COLUMN content_backend", [])
            .expect("drop content_backend");
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("read version");
        assert_eq!(version, 22, "DB must still be stamped at HEAD");
    }

    // Run a normal command: the migration runner skips at HEAD and must NOT
    // auto-repair (re-add) the dropped column.
    let _ = repo.forge().args(["--json", "show"]).assert();

    // The column was NOT re-added — at-HEAD drift is not auto-repaired.
    {
        let connection = Connection::open(&db_path).expect("re-open forge db");
        let has_content_backend: bool = connection
            .prepare("PRAGMA table_info(repositories)")
            .expect("prepare table_info")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query columns")
            .any(|c| c.expect("column name") == "content_backend");
        assert!(
            !has_content_backend,
            "at-HEAD drift must NOT be auto-repaired (column stays dropped)"
        );
        // Version is unchanged — the runner did not re-stamp or re-run a migration.
        let version: i64 = connection
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("read version");
        assert_eq!(version, 22);
    }

    // `forge doctor` surfaces the structural drift rather than reporting a healthy
    // schema: it does not return `ok: true`. (Because `open_repository` reads the
    // dropped column, doctor surfaces it as a structural error, not a clean drift
    // report — the point FIX K pins is that drift is surfaced, not auto-repaired.)
    let output = repo
        .forge()
        .args(["--json", "doctor"])
        .assert()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_ne!(
        json["data"]["ok"], true,
        "doctor must NOT report a healthy schema on a drifted at-HEAD DB"
    );
    assert_eq!(json["status"], "error", "structural drift is surfaced");
}

#[test]
fn init_outside_git_repo_returns_structured_error() {
    let temp_dir = tempfile::tempdir().expect("temp dir");

    let output = forge_in(temp_dir.path())
        .args(["--json", "init"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["command"], "init");
    assert_eq!(json["status"], "error");
    assert_eq!(json["errors"][0]["code"], "NOT_A_GIT_REPOSITORY");
}

#[test]
fn json_mode_reports_missing_subcommand_as_envelope() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .arg("--json")
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["schema_version"], "forge.cli.v0");
    assert_eq!(json["command"], "forge");
    assert_eq!(json["status"], "error");
    assert_eq!(json["errors"][0]["code"], "MISSING_ARGUMENT");
}

#[test]
fn json_mode_reports_missing_export_branch_name_as_envelope() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .args(["--json", "export", "branch"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["command"], "export branch");
    assert_eq!(json["errors"][0]["code"], "MISSING_ARGUMENT");
}

#[test]
fn json_mode_reports_unknown_argument_as_envelope() {
    let repo = TestRepo::new_git();

    let output = repo
        .forge()
        .args(["--json", "--unknown", "init"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["command"], "init");
    assert_eq!(json["errors"][0]["code"], "UNKNOWN_ARGUMENT");
}

/// NER-143 R9: a native `init` inside an existing forge repo's subtree must be
/// REFUSED, not silently create a nested `.forge` that `forge_root`'s
/// nearest-ancestor routing would shadow (and whose objects look unreachable to
/// the outer repo's gc — a Phase-8 deletion hazard). The refusal message must be
/// path-free (S1).
#[test]
fn native_init_nested_in_existing_repo_subtree_is_refused() {
    let temp_dir = tempfile::tempdir().expect("temp dir");

    // Outer native repo at the temp root (no git needed for native init).
    forge_in(temp_dir.path())
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();

    // A subdirectory inside the outer repo's tree.
    let nested = temp_dir.path().join("nested");
    std::fs::create_dir(&nested).expect("create nested dir");

    let output = forge_in(&nested)
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let json: Value = serde_json::from_slice(&output).expect("valid json");
    assert_eq!(json["command"], "init");
    assert_eq!(json["status"], "error");
    // The nested `.forge` must NOT have been created.
    assert!(
        !nested.join(".forge/forge.db").exists(),
        "a refused nested init must not leave a nested .forge/forge.db"
    );
    // S1: the refusal must not leak a filesystem path in any error string.
    let needle = nested.to_string_lossy();
    let rendered = serde_json::to_string(&json).expect("re-serialize envelope");
    assert!(
        !rendered.contains(&*needle),
        "nested-init refusal leaked a path: {rendered}"
    );
}
