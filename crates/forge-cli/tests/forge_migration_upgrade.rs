//! HEAD+1 refusal at the command boundary (NER-133 U4).
//!
//! `command_result` runs `forge_store::migrate(cwd)` before any per-command lock
//! and before the pre-flight replay check. A DB stamped at HEAD+1 (written by a
//! newer Forge) must be refused with `SCHEMA_VERSION_UNSUPPORTED`, and — for a
//! mutating command — the refusal MUST short-circuit before
//! `record_failed_operation`, so the binary never writes into a schema it is
//! explicitly refusing. We assert that by pinning the `operations` row count
//! across the refused command.

mod common;

use common::TestRepo;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

fn db_path(repo: &TestRepo) -> std::path::PathBuf {
    repo.path().join(".forge/forge.db")
}

fn open(db: &Path) -> Connection {
    Connection::open(db).expect("open forge.db")
}

/// Stamp `schema_migrations` with a HEAD+1 row, simulating a DB written by a newer
/// Forge. The `init`-created ledger already carries the `checksum` column. Migration
/// 022 (contracts) added the current HEAD, so HEAD is now 22 and HEAD+1 is 23 (version
/// 22 is a valid current version that the runner would accept — the refusal test
/// requires a genuinely-ahead version).
fn stamp_future_version(db: &Path) {
    let conn = open(db);
    conn.execute(
        "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (23, 'future', 0)",
        [],
    )
    .expect("stamp future version");
}

fn operations_count(db: &Path) -> i64 {
    open(db)
        .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
        .expect("count operations")
}

#[test]
fn head_plus_one_refuses_mutating_command_without_writing() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let db = db_path(&repo);

    stamp_future_version(&db);
    let before = operations_count(&db);

    // A mutating command: the refusal must short-circuit BEFORE
    // `record_failed_operation`, so no new `operations` row is written.
    let out = json_output(
        repo.forge()
            .args(["--json", "start", "x"])
            .assert()
            .failure(),
    );
    assert_eq!(out["errors"][0]["code"], "SCHEMA_VERSION_UNSUPPORTED");

    let after = operations_count(&db);
    assert_eq!(
        before, after,
        "HEAD+1 refusal must not write an operations row (short-circuit before record_failed_operation)"
    );
}

#[test]
fn head_plus_one_refuses_read_only_command() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let db = db_path(&repo);

    stamp_future_version(&db);

    let out = json_output(repo.forge().args(["--json", "show"]).assert().failure());
    assert_eq!(out["errors"][0]["code"], "SCHEMA_VERSION_UNSUPPORTED");
}
