//! Phase 1a (NER-131) concurrency + durability exit-criteria tests (U6).
//!
//! These spawn **real OS processes** (`std::process::Command`, launched from
//! threads) against one shared `.forge/forge.db`. The threads are only a launch
//! harness — the unit of concurrency must be a separate process, since an
//! in-process multi-threaded test sharing one connection would not exercise WAL
//! multi-process semantics.
//!
//! Exit criteria proven here:
//! - ≥8 concurrent processes complete the compete loop with zero `SQLITE_BUSY`
//!   and zero ID collisions (U3 + U4).
//! - A command retried with the same `--request-id` under concurrency creates
//!   exactly one set of domain rows (U5 / R6).
//! - No object-write durability path swallows an fsync error (U1 / R1, asserted
//!   statically). The exhaustive 10k single-process mint-uniqueness check lives
//!   in `forge-core` (`ten_thousand_mints_have_no_collisions`); here we add the
//!   cross-process uniqueness assertion.

mod common;

use common::TestRepo;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Number of concurrent worker processes. The roadmap exit criterion is ≥8.
const WORKERS: usize = 8;

/// Substrings (lowercased) that indicate a `SQLITE_BUSY`-class error reached the
/// user — exactly the failure WAL + `busy_timeout` + `BEGIN IMMEDIATE` + the
/// bounded retry must prevent. `517` is `SQLITE_BUSY_SNAPSHOT`.
const BUSY_MARKERS: &[&str] = &[
    "database is locked",
    "database is busy",
    "sqlite_busy",
    "busysnapshot",
    "(517)",
    "code 517",
];

/// Tables whose `id` column is minted by `forge_core::new_id` / `unique_suffix`.
const MINTED_ID_TABLES: &[&str] = &[
    "repositories",
    "operations",
    "views",
    "intents",
    "attempts",
    "snapshots",
    "evidence",
    "proposals",
    "proposal_revisions",
    "check_results",
    "decisions",
    "publications",
];

struct ForgeRun {
    envelope_ok: bool,
    stdout: String,
    stderr: String,
    json: Option<Value>,
}

impl ForgeRun {
    fn error_code(&self) -> Option<String> {
        self.json
            .as_ref()
            .and_then(|v| v["errors"][0]["code"].as_str())
            .map(str::to_string)
    }

    fn error_message(&self) -> Option<String> {
        self.json
            .as_ref()
            .and_then(|v| v["errors"][0]["message"].as_str())
            .map(str::to_string)
    }

    fn operation_id(&self) -> Option<String> {
        self.json
            .as_ref()
            .and_then(|v| v["operation_id"].as_str())
            .map(str::to_string)
    }
}

fn forge_bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("forge")
}

/// Run `forge <args>` as a child process in `repo` and capture its envelope.
fn run_forge(repo: &Path, args: &[&str]) -> ForgeRun {
    let output = std::process::Command::new(forge_bin())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn forge process");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json: Option<Value> = serde_json::from_str(&stdout).ok();
    let envelope_ok = json
        .as_ref()
        .map(|v| v["status"] == Value::String("success".to_string()))
        .unwrap_or(false);
    ForgeRun {
        envelope_ok,
        stdout,
        stderr,
        json,
    }
}

fn assert_no_busy(run: &ForgeRun, context: &str) {
    let haystack = format!("{}\n{}", run.stdout, run.stderr).to_lowercase();
    for marker in BUSY_MARKERS {
        assert!(
            !haystack.contains(marker),
            "{context}: a SQLITE_BUSY-class error surfaced (marker {marker:?}).\nstdout: {}\nstderr: {}",
            run.stdout,
            run.stderr
        );
    }
}

fn open_db(repo: &Path) -> Connection {
    Connection::open(repo.join(".forge/forge.db")).expect("open forge.db")
}

fn count(connection: &Connection, sql: &str, param: &str) -> i64 {
    connection
        .query_row(sql, [param], |row| row.get(0))
        .expect("count query")
}

/// Belt-and-suspenders cross-process uniqueness check: gather every minted id
/// across all tables into one set and assert no two rows share an id. Within a
/// table the PK already forbids duplicates (a collision would have surfaced as a
/// constraint error in a worker's output, caught by the per-worker assertions);
/// the differing `<prefix>_` bodies make cross-table collisions impossible too,
/// so this is a defensive backstop on top of those.
fn assert_no_id_collisions(repo: &Path) {
    let connection = open_db(repo);
    let mut all_ids: Vec<String> = Vec::new();
    for table in MINTED_ID_TABLES {
        let mut statement = connection
            .prepare(&format!("SELECT id FROM {table}"))
            .expect("prepare id query");
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query ids")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect ids");
        all_ids.extend(ids);
    }
    let unique: HashSet<&String> = all_ids.iter().collect();
    assert_eq!(
        unique.len(),
        all_ids.len(),
        "minted IDs collided across processes ({} rows, {} distinct)",
        all_ids.len(),
        unique.len()
    );
}

/// One competing rival: keep launching `attempt start --intent <id>` until it
/// wins the optimistic operation-DAG advance. Models the roadmap's "agents fan
/// out in parallel and retry" — the only acceptable transient is the logical
/// `current operation changed` CAS conflict (the repo-level advisory lock that
/// would let them all proceed without conflict is deferred to Phase 1b). A
/// SQLITE_BUSY-class error or any other failure fails the test immediately.
fn run_competing_attempt(repo: &Path, intent_id: &str, worker: usize) -> bool {
    const MAX_ATTEMPTS: usize = 80;
    for attempt in 0..MAX_ATTEMPTS {
        // No `--request-id`: each retry is an independent command, so a
        // CAS-conflict failure recorded for an id can never be replayed back.
        let run = run_forge(repo, &["--json", "attempt", "start", "--intent", intent_id]);
        assert_no_busy(&run, &format!("worker {worker}, attempt {attempt}"));
        if run.envelope_ok {
            return true;
        }
        let message = run.error_message().unwrap_or_default();
        assert!(
            message.contains("current operation changed"),
            "worker {worker}: unexpected non-CAS failure (code {:?}): {}",
            run.error_code(),
            run.stdout
        );
        // Jittered backoff desynchronizes the rivals so the CAS loop converges.
        thread::sleep(Duration::from_millis(8 + (worker as u64 % 7)));
    }
    false
}

#[test]
fn init_opens_database_in_wal_mode() {
    let repo = TestRepo::new_git();
    let init = run_forge(repo.path(), &["--json", "init"]);
    assert!(init.envelope_ok, "init failed: {}", init.stdout);

    // WAL is persisted in the database header, so a fresh connection observes it.
    let connection = open_db(repo.path());
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("query journal_mode");
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "journal_mode should be WAL after init"
    );
}

#[test]
fn concurrent_init_of_same_repo_is_race_safe() {
    // The wedge inits a repo once before agents fan out; that single init can race
    // itself (e.g. two orchestrators). With the .forge write lock (NER-132 U5),
    // exactly one process performs the real init and the rest observe its committed
    // row and report already_initialized — never a raw UNIQUE-constraint error
    // masked as NOT_A_GIT_REPOSITORY, never SQLITE_BUSY.
    let repo = TestRepo::new_git();
    let repo_path = repo.path().to_path_buf();

    let handles: Vec<_> = (0..WORKERS)
        .map(|_| {
            let repo_path = repo_path.clone();
            thread::spawn(move || run_forge(&repo_path, &["--json", "init"]))
        })
        .collect();
    let runs: Vec<ForgeRun> = handles
        .into_iter()
        .map(|h| h.join().expect("init thread"))
        .collect();

    let mut fresh_inits = 0;
    for (worker, run) in runs.iter().enumerate() {
        assert_no_busy(run, &format!("init worker {worker}"));
        assert!(
            run.envelope_ok,
            "init worker {worker} failed: {}\n{}",
            run.stdout, run.stderr
        );
        assert!(
            !format!("{}\n{}", run.stdout, run.stderr).contains("UNIQUE constraint"),
            "raw SQLite UNIQUE text leaked from a racing init: {}",
            run.stdout
        );
        assert_ne!(
            run.error_code().as_deref(),
            Some("NOT_A_GIT_REPOSITORY"),
            "a racing init was misreported as NOT_A_GIT_REPOSITORY: {}",
            run.stdout
        );
        let already_initialized = run
            .json
            .as_ref()
            .and_then(|v| v["data"]["already_initialized"].as_bool())
            .expect("already_initialized in init envelope");
        if !already_initialized {
            fresh_inits += 1;
        }
    }
    assert_eq!(
        fresh_inits, 1,
        "exactly one process performs the real init; the rest report already_initialized"
    );

    // And exactly one repository row exists regardless of the race.
    let repos: i64 = open_db(&repo_path)
        .query_row("SELECT COUNT(*) FROM repositories", [], |row| row.get(0))
        .expect("count repositories");
    assert_eq!(
        repos, 1,
        "concurrent init created exactly one repository row"
    );
}

#[test]
fn eight_competing_processes_complete_without_sqlite_busy() {
    let repo = TestRepo::new_git();
    assert!(run_forge(repo.path(), &["--json", "init"]).envelope_ok);

    // The shared intent the rivals compete under.
    let start = run_forge(repo.path(), &["--json", "start", "compete"]);
    assert!(start.envelope_ok, "start failed: {}", start.stdout);
    let intent_id = start.json.unwrap()["data"]["intent_id"]
        .as_str()
        .expect("intent_id")
        .to_string();

    let repo_path = repo.path().to_path_buf();
    let handles: Vec<_> = (0..WORKERS)
        .map(|worker| {
            let repo_path = repo_path.clone();
            let intent_id = intent_id.clone();
            thread::spawn(move || run_competing_attempt(&repo_path, &intent_id, worker))
        })
        .collect();
    let successes = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread"))
        .filter(|won| *won)
        .count();

    // Every rival completes the compete loop: zero SQLITE_BUSY is enforced inside
    // run_competing_attempt; here we confirm all of them ultimately won the CAS.
    assert_eq!(
        successes, WORKERS,
        "all {WORKERS} rivals should create an attempt within the retry budget"
    );

    // The shared intent now has WORKERS rival attempts plus the seed attempt.
    let connection = open_db(repo.path());
    let attempts_for_intent = count(
        &connection,
        "SELECT COUNT(*) FROM attempts WHERE intent_id = ?1",
        &intent_id,
    );
    assert_eq!(attempts_for_intent, (WORKERS + 1) as i64);

    assert_no_id_collisions(repo.path());
}

#[test]
fn concurrent_same_request_id_creates_exactly_one_snapshot() {
    let repo = TestRepo::new_git();
    assert!(run_forge(repo.path(), &["--json", "init"]).envelope_ok);

    // A single active attempt (top-level `start` auto-attaches it).
    let start = run_forge(repo.path(), &["--json", "start", "idempotent-save"]);
    assert!(start.envelope_ok, "start failed: {}", start.stdout);
    let attempt_id = start.json.unwrap()["data"]["attempt_id"]
        .as_str()
        .expect("attempt_id")
        .to_string();

    // Give save something to capture.
    std::fs::write(repo.path().join("README.md"), "concurrent save\n").expect("write readme");

    const REQUEST_ID: &str = "shared-save-request";
    let repo_path = repo.path().to_path_buf();
    let handles: Vec<_> = (0..WORKERS)
        .map(|_| {
            let repo_path = repo_path.clone();
            let attempt_id = attempt_id.clone();
            thread::spawn(move || {
                run_forge(
                    &repo_path,
                    &[
                        "--json",
                        "--request-id",
                        REQUEST_ID,
                        "save",
                        "--attempt",
                        &attempt_id,
                    ],
                )
            })
        })
        .collect();
    let outcomes: Vec<ForgeRun> = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread"))
        .collect();

    // Exactly one writer mutates; the rest replay the winner's success (R6). No
    // worker sees SQLITE_BUSY, and every worker resolves to one operation id.
    let mut operation_ids: HashSet<String> = HashSet::new();
    for (worker, run) in outcomes.iter().enumerate() {
        assert_no_busy(run, &format!("save worker {worker}"));
        assert!(
            run.envelope_ok,
            "save worker {worker} neither wrote nor replayed: {}",
            run.stdout
        );
        operation_ids.insert(run.operation_id().expect("operation_id"));
    }
    assert_eq!(
        operation_ids.len(),
        1,
        "all concurrent same-request-id saves must resolve to one operation id"
    );

    let connection = open_db(repo.path());
    let snapshots = count(
        &connection,
        "SELECT COUNT(*) FROM snapshots WHERE attempt_id = ?1",
        &attempt_id,
    );
    assert_eq!(
        snapshots, 1,
        "concurrent same-request-id save must create exactly one snapshot row"
    );
    let operations = count(
        &connection,
        "SELECT COUNT(*) FROM operations WHERE request_id = ?1",
        REQUEST_ID,
    );
    assert_eq!(
        operations, 1,
        "concurrent same-request-id save must record exactly one operation row"
    );
}

#[test]
fn concurrent_same_request_id_for_different_command_conflicts() {
    let repo = TestRepo::new_git();
    assert!(run_forge(repo.path(), &["--json", "init"]).envelope_ok);

    // Commit an operation under the shared id first, so every racing `save`
    // observes a committed `start` row for a different command.
    let start = run_forge(
        repo.path(),
        &["--json", "--request-id", "shared-id", "start", "scoped"],
    );
    assert!(start.envelope_ok, "start failed: {}", start.stdout);
    std::fs::write(repo.path().join("README.md"), "changed\n").expect("write readme");

    let repo_path = repo.path().to_path_buf();
    let handles: Vec<_> = (0..WORKERS)
        .map(|_| {
            let repo_path = repo_path.clone();
            thread::spawn(move || {
                run_forge(&repo_path, &["--json", "--request-id", "shared-id", "save"])
            })
        })
        .collect();
    let outcomes: Vec<ForgeRun> = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread"))
        .collect();

    for (worker, run) in outcomes.iter().enumerate() {
        assert_no_busy(run, &format!("conflict worker {worker}"));
        assert_eq!(
            run.error_code().as_deref(),
            Some("REQUEST_ID_CONFLICT"),
            "worker {worker} should conflict, got: {}",
            run.stdout
        );
    }
}

/// Rewrite a freshly-init'd v2 DB into a GENUINE v1 (NER-133 U4): drop both
/// columns 002 adds and set the recorded version back to 1. We must drop the
/// columns (not just lower the version) — re-applying 002 over a DB that still
/// carries them would hit "duplicate column name". SQLite allows dropping both
/// because each is a column-level definition (the `attached_attempt_id` FK is a
/// column-level `REFERENCES`, not a table constraint).
fn rewrite_to_genuine_v1(repo: &Path) {
    let connection = open_db(repo);
    connection
        .execute_batch(
            "ALTER TABLE repositories DROP COLUMN content_backend;
             ALTER TABLE current_state DROP COLUMN attached_attempt_id;
             DELETE FROM schema_migrations WHERE version > 1;",
        )
        .expect("rewrite to genuine v1");
}

#[test]
fn concurrent_commands_against_behind_db_both_upgrade_and_succeed() {
    // Two processes run mutating commands against the same behind (v1) DB. The
    // transient `migrate()` lock (acquired BEFORE each command's per-command lock,
    // never nested) serializes the upgrade against an in-flight unrelated mutating
    // command on the same `.forge/forge.lock`: both upgrade transparently and
    // succeed, the schema lands at HEAD with exactly one set of version rows, and
    // no SQLITE_BUSY-class error surfaces — proving migrate-vs-locked-writer does
    // not deadlock.
    let repo = TestRepo::new_git();
    assert!(run_forge(repo.path(), &["--json", "init"]).envelope_ok);
    rewrite_to_genuine_v1(repo.path());

    let repo_path = repo.path().to_path_buf();
    // Distinct intents + no shared --request-id, so each command is independent and
    // must fully succeed (not replay). `start` is mutating, so each also takes the
    // per-command lock after migrate releases the transient one.
    let handles: Vec<_> = (0..2)
        .map(|worker| {
            let repo_path = repo_path.clone();
            thread::spawn(move || {
                let intent = format!("behind-db-upgrade-{worker}");
                run_forge(&repo_path, &["--json", "start", &intent])
            })
        })
        .collect();
    let outcomes: Vec<ForgeRun> = handles
        .into_iter()
        .map(|h| h.join().expect("worker thread"))
        .collect();

    for (worker, run) in outcomes.iter().enumerate() {
        assert_no_busy(run, &format!("behind-db worker {worker}"));
        assert!(
            run.envelope_ok,
            "behind-db worker {worker} did not succeed: {}\nstderr: {}",
            run.stdout, run.stderr
        );
    }

    let connection = open_db(repo.path());
    // Exactly one set of version rows: the second upgrader's `INSERT OR IGNORE`
    // is a no-op once the first commits version 21 (probe the HEAD migration, whose
    // dedup the race actually exercises — code-review F5).
    let version_rows = count(
        &connection,
        "SELECT COUNT(*) FROM schema_migrations WHERE version = ?1",
        "21",
    );
    assert_eq!(version_rows, 1, "exactly one version-21 row after the race");
    let head: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .expect("max version");
    assert_eq!(head, 22, "schema reached HEAD after the concurrent upgrade");
    // The upgrade actually applied: 002's columns are present.
    let has_backend: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('repositories') WHERE name = 'content_backend'",
            [],
            |row| row.get(0),
        )
        .expect("probe content_backend");
    assert_eq!(has_backend, 1, "002 re-added content_backend");
}

#[test]
fn no_swallowed_sync_remains_on_durability_paths() {
    // R1 exit criterion: no `let _ = .*sync` may remain on an object-write
    // durability path. Scan the native content store's source.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let durability_source = workspace_root.join("crates/forge-content-native/src/lib.rs");
    let source = std::fs::read_to_string(&durability_source).expect("read native store source");

    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            continue;
        }
        let swallows_sync = line.contains("let _") && line.to_lowercase().contains("sync");
        assert!(
            !swallows_sync,
            "{}:{}: a sync error is swallowed on a durability path: {line}",
            durability_source.display(),
            index + 1
        );
    }
}
