//! CCX native-contract hardening slice (validated code-review findings F2–F7):
//! `--dep` ack validation, acceptance non-vacuity + acceptance-internal unknown keys,
//! the verify empty-acceptance floor, replay of a recorded run/verify returning the
//! recorded exit code, revision-bound verdicts, and mid-chain failure recording.
//!
//! These live in a separate integration binary from `forge_contract.rs` to keep both
//! files under the ADR-0001 Rust line-count ceiling. The helper scaffolding below is
//! the minimal subset needed here (each `tests/*.rs` is an independent crate).

mod common;

use common::TestRepo;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Shared scaffolding (minimal subset mirrored from forge_contract.rs)
// ---------------------------------------------------------------------------

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json envelope")
}

fn init_repo() -> TestRepo {
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    repo
}

fn write(repo: &TestRepo, rel: &str, content: &str) {
    let path = repo.path().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create dirs");
    }
    std::fs::write(path, content).expect("write file");
}

fn revision_count(repo: &TestRepo) -> i64 {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM contract_revisions", [], |row| {
        row.get(0)
    })
    .expect("count revisions")
}

fn error_code(envelope: &Value) -> &str {
    envelope["errors"][0]["code"]
        .as_str()
        .expect("error code present")
}

fn contract_file_name(id: &str) -> String {
    format!("{}.yaml", id.strip_prefix("ccx-").unwrap_or(id))
}

fn contract_yaml(id: &str, depends_on: &[&str]) -> String {
    let mut yaml = format!(
        "schema: ccx.contract.v1\n\
id: {id}\n\
revision: 1\n\
ticket: NER-999\n\
task: Do a small thing\n\
interface: Build the thing in the module.\n\
acceptance:\n  fix:\n    - cargo test -p forge-core\n\
allowed_changes:\n  paths: [crates/forge-core/src/lib.rs, out.txt]\n\
authority: {{source: human, confidence: high, reviewer: test}}\n"
    );
    if !depends_on.is_empty() {
        yaml.push_str("depends_on:\n");
        for dep in depends_on {
            yaml.push_str(&format!("  - {dep}\n"));
        }
    }
    yaml
}

fn install_contract(repo: &TestRepo, id: &str, depends_on: &[&str]) {
    write(
        repo,
        &format!("contracts/{}", contract_file_name(id)),
        &contract_yaml(id, depends_on),
    );
}

fn freeze_contract(repo: &TestRepo, id: &str, depends_on: &[&str]) {
    install_contract(repo, id, depends_on);
    repo.forge()
        .args([
            "--json",
            "contract",
            "freeze",
            &format!("contracts/{}", contract_file_name(id)),
        ])
        .assert()
        .success();
}

fn freeze_global_policy(repo: &TestRepo) {
    let policy = "schema: ccx.contract.v1\nkind: global_policy\nrules:\n  - anyhow throughout.\n";
    write(repo, "contracts/_global-policy.yaml", policy);
    repo.forge()
        .args([
            "--json",
            "contract",
            "freeze",
            "contracts/_global-policy.yaml",
        ])
        .assert()
        .success();
}

fn run_repo() -> TestRepo {
    let repo = init_repo();
    freeze_global_policy(&repo);
    repo
}

fn contract_run(repo: &TestRepo, ids: &[&str], agent_cmd: &str, expect_exit: i32) -> Value {
    let mut args: Vec<String> = vec!["--json".into(), "contract".into(), "run".into()];
    for id in ids {
        args.push((*id).to_string());
    }
    if ids.len() > 1 {
        args.push("--chain".into());
    }
    args.push("--agent-cmd".into());
    args.push(agent_cmd.to_string());
    json_output(repo.forge().args(&args).assert().code(expect_exit))
}

const STOP_AGENT: &str =
    "printf 'What: need the shape\\nWhy: brief omits it\\nKind: blocking\\nEvidence: src/lib.rs:1\\n' > UNKNOWN.md";

const EDIT_AGENT: &str = "echo change >> out.txt";

fn task_outcome(repo: &TestRepo, run_id: &str, task_id: &str) -> String {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    conn.query_row(
        "SELECT outcome FROM contract_run_tasks WHERE run_id = ?1 AND task_id = ?2",
        rusqlite::params![run_id, task_id],
        |r| r.get(0),
    )
    .expect("task row")
}

const VERIFY_CARGO_TOML: &str =
    "[package]\nname = \"verifyfixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const GOOD_LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
const MISFORMATTED_LIB: &str = "pub fn add(a:i32,b:i32)->i32{a+b}\n";

fn install_verify_crate(repo: &TestRepo, misformatted: bool) {
    write(repo, "Cargo.toml", VERIFY_CARGO_TOML);
    write(
        repo,
        "src/lib.rs",
        if misformatted {
            MISFORMATTED_LIB
        } else {
            GOOD_LIB
        },
    );
}

fn freeze_verify_contract(repo: &TestRepo, id: &str, fix: &[&str], guard: &[&str]) {
    let mut yaml = format!(
        "schema: ccx.contract.v1\n\
id: {id}\n\
revision: 1\n\
ticket: NER-999\n\
task: Add two integers.\n\
interface: Provide an add function.\n\
acceptance:\n  fix:\n"
    );
    for cmd in fix {
        yaml.push_str(&format!("    - {cmd}\n"));
    }
    if !guard.is_empty() {
        yaml.push_str("  guard:\n");
        for cmd in guard {
            yaml.push_str(&format!("    - {cmd}\n"));
        }
    }
    yaml.push_str(
        "allowed_changes:\n  paths: [out.txt]\n\
authority: {source: human, confidence: high, reviewer: test}\n",
    );
    write(
        repo,
        &format!("contracts/{}", contract_file_name(id)),
        &yaml,
    );
    repo.forge()
        .args([
            "--json",
            "contract",
            "freeze",
            &format!("contracts/{}", contract_file_name(id)),
        ])
        .assert()
        .success();
}

fn contract_verify(repo: &TestRepo, target: &str, expect_exit: i32) -> Value {
    json_output(
        repo.forge()
            .args(["--json", "contract", "verify", target])
            .assert()
            .code(expect_exit),
    )
}

fn verify_verdict_count(repo: &TestRepo, run_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).expect("open db");
    conn.query_row(
        "SELECT COUNT(*) FROM contract_run_verdicts
         WHERE run_id = ?1 AND verdict_kind IN ('fix','guard','aggregate')",
        [run_id],
        |r| r.get(0),
    )
    .expect("count verify verdicts")
}

fn completed_verify_run(repo: &TestRepo, id: &str) -> String {
    let env = contract_run(repo, &[id], EDIT_AGENT, 0);
    assert_eq!(env["data"]["outcome"], "completed");
    env["data"]["run_id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// F2: --dep ack validation (bound to the named contract, completed, same-base)
// ---------------------------------------------------------------------------

#[test]
fn contract_run_dep_ref_of_different_contract_refused() {
    // F2: a --dep ack ref that resolves to a run/task for a DIFFERENT contract than
    // the named dependency is a typed CONTRACT_NOT_INTEGRABLE refusal — a mismatched
    // patch must never be silently overlaid.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-other", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    // A completed run of ccx-other exists, but it is NOT a run of ccx-a.
    let other = contract_run(&repo, &["ccx-other"], EDIT_AGENT, 0);
    let other_id = other["data"]["run_id"].as_str().unwrap().to_string();

    // Acknowledge ccx-a with a ref that actually points at ccx-other's run.
    let dep = format!("ccx-a={other_id}");
    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-b",
                "--dep",
                dep.as_str(),
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
}

#[test]
fn contract_run_dep_taskid_of_completed_task_in_stopped_run_resolves() {
    // F2: a --dep naming a COMPLETED task inside an otherwise-STOPPED run resolves via
    // that task's own per-task patch (previously refused, because a stopped run has no
    // run-level patch). Build a chain ccx-a -> ccx-d that stops AT ccx-d while ccx-a
    // completes: a conditional agent edits when out.txt is absent (ccx-a's scratch is
    // base-only) and files a stop when out.txt is present (ccx-d's scratch already
    // carries ccx-a's overlaid patch), so ccx-a completes and ccx-d halts.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-d", &["ccx-a"]);
    let conditional_agent =
        "if [ -f out.txt ]; then printf 'What: shape\\nWhy: brief omits it\\nKind: blocking\\nEvidence: out.txt:1\\n' > UNKNOWN.md; else echo change >> out.txt; fi";
    let stopped = contract_run(&repo, &["ccx-a", "ccx-d"], conditional_agent, 2);
    let stopped_id = stopped["data"]["run_id"].as_str().unwrap().to_string();
    assert_eq!(stopped["data"]["outcome"], "stopped");
    assert_eq!(task_outcome(&repo, &stopped_id, "ccx-a"), "completed");
    assert_eq!(task_outcome(&repo, &stopped_id, "ccx-d"), "stopped");

    // Acknowledge ccx-a via the STOPPED run's ref: it resolves the completed ccx-a task's
    // own per-task patch even though the run's target (ccx-d) stopped.
    let dep = format!("ccx-a={stopped_id}");
    freeze_contract(&repo, "ccx-c", &["ccx-a"]);
    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-c",
                "--dep",
                dep.as_str(),
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .code(0),
    );
    assert_eq!(env["data"]["outcome"], "completed");
}

// ---------------------------------------------------------------------------
// F3: acceptance non-vacuity + acceptance-internal unknown keys + verify floor
// ---------------------------------------------------------------------------

#[test]
fn lint_empty_fix_set_errors() {
    // F3(a): the fix set is the task's own proof and must be non-vacuous — an empty
    // fix set would verify green having executed nothing. Guard-only is not enough.
    let repo = init_repo();
    let contract = "\
schema: ccx.contract.v1
id: ccx-demo
revision: 1
ticket: NER-999
task: Do a small thing
interface: Build the thing in the module.
acceptance:
  guard:
    - cargo test -p forge-core
allowed_changes:
  paths: [crates/forge-core/src/lib.rs]
authority: {source: human, confidence: high, reviewer: test}
";
    write(&repo, "contracts/demo.yaml", contract);
    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&lint), "CONTRACT_LINT_FAILED");
    let violations = lint["errors"][0]["details"]["violations"].to_string();
    assert!(
        violations.contains("fix must contain at least one command"),
        "must flag the empty fix set: {violations}"
    );
    assert_eq!(revision_count(&repo), 0);
}

#[test]
fn lint_unknown_acceptance_key_errors_naming_key() {
    // F3(b): only fix/guard are legal inside a v1 acceptance mapping — a typo'd key
    // (`fixes:`) is a hard error naming the stray key, not a silently-dropped,
    // vacuously-green contract.
    let repo = init_repo();
    let contract = "\
schema: ccx.contract.v1
id: ccx-demo
revision: 1
ticket: NER-999
task: Do a small thing
interface: Build the thing in the module.
acceptance:
  fixes:
    - cargo test -p forge-core
allowed_changes:
  paths: [crates/forge-core/src/lib.rs]
authority: {source: human, confidence: high, reviewer: test}
";
    write(&repo, "contracts/demo.yaml", contract);
    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&lint), "CONTRACT_LINT_FAILED");
    let violations = lint["errors"][0]["details"]["violations"].to_string();
    assert!(
        violations.contains("fixes"),
        "must name the stray acceptance key: {violations}"
    );
    assert_eq!(revision_count(&repo), 0);
}

#[test]
fn contract_verify_empty_acceptance_refused() {
    // F3(c): verify is fail-closed and does not trust that a frozen revision was
    // lint-clean. A revision whose acceptance parses to ZERO commands is refused typed
    // rather than recording a vacuous green aggregate. Since lint (F3a) now blocks an
    // empty fix at authoring, we simulate a non-lint-clean frozen revision reaching
    // verify by rewriting the stored source_yaml directly (the same tamper technique
    // the doctor tests use), then confirm verify refuses.
    let repo = run_repo();
    install_verify_crate(&repo, false);
    freeze_verify_contract(
        &repo,
        "ccx-vempty",
        &["cargo build"],
        &["cargo fmt --check"],
    );
    let run_id = completed_verify_run(&repo, "ccx-vempty");

    {
        let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
        let empty = "\
schema: ccx.contract.v1
id: ccx-vempty
revision: 1
ticket: NER-999
task: Add two integers.
interface: Provide an add function.
acceptance: {}
allowed_changes:
  paths: [out.txt]
authority: {source: human, confidence: high, reviewer: test}
";
        conn.execute(
            "UPDATE contract_revisions SET source_yaml = ?1 WHERE contract_id = 'ccx-vempty'",
            [empty],
        )
        .expect("rewrite acceptance to empty");
    }

    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "verify", &run_id])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
    assert_eq!(
        verify_verdict_count(&repo, &run_id),
        0,
        "an empty-acceptance verify records no verdicts"
    );
}

// ---------------------------------------------------------------------------
// F4: replay of a recorded run/verify returns the recorded exit code
// ---------------------------------------------------------------------------

#[test]
fn contract_run_replay_stopped_run_exits_2() {
    // F4: a same-request-id replay of a STOPPED run returns the recorded exit code (2),
    // not a SUCCESS fallback (0). Without persisting exit_code into the run op state and
    // merging it on replay, `main` would silently exit 0 and mask the halt.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let args = [
        "--json",
        "--request-id",
        "run-stop-1",
        "contract",
        "run",
        "ccx-a",
        "--agent-cmd",
        STOP_AGENT,
    ];
    let first = json_output(repo.forge().args(args).assert().code(2));
    assert_eq!(first["data"]["outcome"], "stopped");
    assert_eq!(first["data"]["exit_code"], 2);

    let replay = json_output(repo.forge().args(args).assert().code(2));
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(replay["data"]["exit_code"], 2);
    assert_eq!(replay["data"]["outcome"], "stopped");
}

#[test]
fn contract_verify_replay_guard_regressed_exits_4() {
    // F4: a same-request-id replay of a guard-regressed verify returns exit 4, not 0.
    let repo = run_repo();
    install_verify_crate(&repo, true);
    freeze_verify_contract(&repo, "ccx-vgr", &["cargo build"], &["cargo fmt --check"]);
    let run_id = completed_verify_run(&repo, "ccx-vgr");
    let args = [
        "--json",
        "--request-id",
        "verify-gr-1",
        "contract",
        "verify",
        run_id.as_str(),
    ];
    let first = json_output(repo.forge().args(args).assert().code(4));
    assert_eq!(first["data"]["outcome"], "guard_regressed");
    assert_eq!(first["data"]["exit_code"], 4);

    let replay = json_output(repo.forge().args(args).assert().code(4));
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(replay["data"]["exit_code"], 4);
    assert_eq!(replay["data"]["outcome"], "guard_regressed");
}

// ---------------------------------------------------------------------------
// F6: verdict rows are revision-bound
// ---------------------------------------------------------------------------

#[test]
fn contract_verify_verdicts_record_revision() {
    // F6: every verdict row carries the frozen revision it evaluated, so a signed
    // verdict is bound to the exact revision's acceptance.
    let repo = run_repo();
    install_verify_crate(&repo, false);
    freeze_verify_contract(&repo, "ccx-vrev", &["cargo build"], &["cargo fmt --check"]);
    let run_id = completed_verify_run(&repo, "ccx-vrev");
    let _ = contract_verify(&repo, &run_id, 0);

    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let non_rev1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contract_run_verdicts WHERE run_id = ?1 AND revision != 1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(non_rev1, 0, "every verdict is bound to revision 1");
    let rev1: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contract_run_verdicts WHERE run_id = ?1 AND revision = 1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        rev1 >= 3,
        "fix + guard + aggregate verdicts recorded with revision"
    );
}

// ---------------------------------------------------------------------------
// F7: a mid-chain internal error still records a failed run
// ---------------------------------------------------------------------------

#[test]
fn contract_run_midchain_store_error_records_failed_run() {
    // F7: a fallible per-task step that errors mid-chain must still record a `failed`
    // run so the run id is recoverable and dependents are marked skipped — then surface
    // the error. Trigger: brief emission fails because the global policy is NOT frozen
    // (init_repo, unlike run_repo, does not freeze it), which errors inside the task
    // loop after materialization.
    let repo = init_repo(); // global policy intentionally NOT frozen
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-a",
                "ccx-b",
                "--chain",
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .failure(),
    );
    assert_eq!(env["status"], "error");

    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let (run_id, outcome): (String, String) = conn
        .query_row(
            "SELECT id, outcome FROM contract_runs ORDER BY created_at_ms DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("a failed run was recorded despite the mid-chain error");
    assert_eq!(outcome, "failed");
    assert_eq!(task_outcome(&repo, &run_id, "ccx-a"), "failed");
    assert_eq!(task_outcome(&repo, &run_id, "ccx-b"), "skipped");
    let stderr: Option<String> = conn
        .query_row(
            "SELECT agent_stderr_excerpt FROM contract_run_tasks WHERE run_id = ?1 AND task_id = 'ccx-a'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        stderr.is_some(),
        "the mid-chain error is recorded on the failing task row"
    );
}
