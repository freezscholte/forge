//! NER-362 slice 4: the `provenance_detail` read model — ledger enrichment for
//! blame, keyed by ids a blame payload already carries.
//!
//! The store tests drive `forge_store::init_repository` + `start_attempt` for a
//! real intent row, then insert the downstream proposal/check/decision rows
//! directly (the read model only joins on rows, not on how they were produced).
//! Every lookup must be read-only and every unknown id must degrade to `None`.

use std::path::Path;
use std::process::Command;

/// Create a one-commit git repo and `forge init` it, returning the temp dir.
fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    run_git(dir.path(), &["init"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    std::fs::write(dir.path().join("README.md"), "x").expect("seed file");
    run_git(dir.path(), &["add", "."]);
    run_git(dir.path(), &["commit", "-m", "init"]);
    forge_store::init_repository(dir.path(), None, "git".to_string()).expect("forge init");
    dir
}

fn run_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git")
        .status;
    assert!(status.success(), "git {args:?} failed");
}

fn head(dir: &Path) -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .output()
        .expect("rev-parse");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn open_db(dir: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(".forge/forge.db")).expect("open forge.db")
}

/// Start an attempt (creating a real intents row) and seed the downstream
/// proposal / proposal_revision rows the enrichment joins against. Returns
/// (intent_id, proposal_revision_id).
fn seed_intent_and_revision(dir: &Path, intent_text: &str) -> (String, String) {
    let base = head(dir);
    let start = forge_store::start_attempt(dir, None, intent_text.to_string(), base.clone(), None)
        .expect("start attempt");
    let connection = open_db(dir);
    let repo_id: String = connection
        .query_row(
            "SELECT repo_id FROM current_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("repo_id");
    connection
        .execute(
            "INSERT INTO snapshots (id, repo_id, attempt_id, parent_snapshot_id, content_ref, changed_paths_json, created_at_ms)
             VALUES ('snap-1', ?1, ?2, NULL, 'ref-1', '[]', 1000)",
            rusqlite::params![repo_id, start.attempt_id],
        )
        .expect("insert snapshot");
    connection
        .execute(
            "INSERT INTO proposals (id, repo_id, attempt_id, snapshot_id, base_head, content_ref, status, created_at_ms)
             VALUES ('prop-1', ?1, ?2, 'snap-1', ?3, 'ref-1', 'open', 1000)",
            rusqlite::params![repo_id, start.attempt_id, base],
        )
        .expect("insert proposal");
    connection
        .execute(
            "INSERT INTO proposal_revisions (id, proposal_id, snapshot_id, content_ref, changed_paths_json, created_at_ms)
             VALUES ('rev-1', 'prop-1', 'snap-1', 'ref-1', '[]', 1000)",
            [],
        )
        .expect("insert revision");
    (start.intent_id, "rev-1".to_string())
}

#[test]
fn joins_intent_title_decision_status_and_latest_check_status() {
    let repo = init_repo();
    let (intent_id, revision_id) = seed_intent_and_revision(repo.path(), "ship the enrichment");
    let connection = open_db(repo.path());
    let repo_id: String = connection
        .query_row(
            "SELECT repo_id FROM current_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("repo_id");
    // Two check rows: the LATEST (by created_at_ms) must win, not the first.
    connection
        .execute(
            "INSERT INTO check_results (id, repo_id, proposal_id, proposal_revision_id, status, reason, created_at_ms)
             VALUES ('check-old', ?1, 'prop-1', ?2, 'failed', 'first run', 1000)",
            rusqlite::params![repo_id, revision_id],
        )
        .expect("insert older check");
    connection
        .execute(
            "INSERT INTO check_results (id, repo_id, proposal_id, proposal_revision_id, status, reason, created_at_ms)
             VALUES ('check-new', ?1, 'prop-1', ?2, 'passed', 'rerun', 2000)",
            rusqlite::params![repo_id, revision_id],
        )
        .expect("insert newer check");
    connection
        .execute(
            "INSERT INTO decisions (id, repo_id, proposal_id, proposal_revision_id, decision, created_at_ms)
             VALUES ('dec-1', ?1, 'prop-1', ?2, 'accepted', 3000)",
            rusqlite::params![repo_id, revision_id],
        )
        .expect("insert decision");

    let detail = forge_store::provenance_detail(
        repo.path(),
        &intent_id,
        Some(revision_id.as_str()),
        Some("dec-1"),
    )
    .expect("enrichment lookup");

    assert_eq!(detail.intent_id, intent_id);
    assert_eq!(detail.intent_title.as_deref(), Some("ship the enrichment"));
    assert_eq!(detail.decision_id.as_deref(), Some("dec-1"));
    assert_eq!(detail.decision_status.as_deref(), Some("accepted"));
    assert_eq!(detail.check_status.as_deref(), Some("passed"));
}

#[test]
fn unknown_ids_degrade_to_none_never_error() {
    let repo = init_repo();
    // History that predates ledger rows: none of these ids exist anywhere.
    let detail = forge_store::provenance_detail(
        repo.path(),
        "intent-ghost",
        Some("rev-ghost"),
        Some("dec-ghost"),
    )
    .expect("unknown ids must not error");
    assert_eq!(detail.intent_id, "intent-ghost");
    assert_eq!(detail.intent_title, None);
    assert_eq!(detail.decision_id.as_deref(), Some("dec-ghost"));
    assert_eq!(detail.decision_status, None);
    assert_eq!(detail.check_status, None);
}

#[test]
fn absent_revision_and_decision_ids_stay_none() {
    let repo = init_repo();
    let (intent_id, _revision_id) = seed_intent_and_revision(repo.path(), "partial provenance");
    let detail = forge_store::provenance_detail(repo.path(), &intent_id, None, None)
        .expect("intent-only lookup");
    assert_eq!(detail.intent_title.as_deref(), Some("partial provenance"));
    assert_eq!(detail.decision_id, None);
    assert_eq!(detail.decision_status, None);
    assert_eq!(detail.check_status, None);
}

#[test]
fn lookup_is_read_only() {
    let repo = init_repo();
    let (intent_id, revision_id) = seed_intent_and_revision(repo.path(), "no writes");
    let count_rows = |dir: &Path| -> (i64, i64) {
        let connection = open_db(dir);
        let operations: i64 = connection
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .expect("count operations");
        let total_changes: i64 = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM intents) + (SELECT COUNT(*) FROM decisions)
                        + (SELECT COUNT(*) FROM check_results)",
                [],
                |row| row.get(0),
            )
            .expect("count ledger rows");
        (operations, total_changes)
    };
    let before = count_rows(repo.path());
    forge_store::provenance_detail(repo.path(), &intent_id, Some(revision_id.as_str()), None)
        .expect("lookup");
    assert_eq!(
        count_rows(repo.path()),
        before,
        "enrichment must not write any row"
    );
}
