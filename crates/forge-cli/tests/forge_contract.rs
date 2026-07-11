//! NER U3/U4: `forge contract lint | freeze | brief` end-to-end through the CLI.
//!
//! U3 slice: AE4 (unknown top-level key errors, no frozen revision) and AE6
//! (metacharacter / non-cargo acceptance refused at lint with
//! CONTRACT_GRAMMAR_VIOLATION), plus lint-clean freeze reading back as revision
//! 1, wrong-visibility primitive rejection, relative-path canonicalization
//! (R19), and global-policy freeze under the reserved id.
//!
//! U4 slice (R5/R6): `contract brief` byte-parity with `tools/ccx/ccx-brief.py`.
//! The primary assertion compares native stdout to checked-in EXPECTED byte
//! fixtures under `tests/fixtures/contract-briefs/`. Those EXPECTED files
//! (`expected-brief-full.txt`, `expected-brief-missing-b.txt`) were generated once
//! by running `python3 tools/ccx/ccx-brief.py --contracts-dir <fixtures>
//! brief-task.yaml` over the same YAML inputs (the full case with all four files
//! present; the missing case with `brief-neighbor-b.yaml` absent). An additional
//! `#[ignore]`d test re-runs the live Python emitter for a defense-in-depth cross
//! check where `python3` + PyYAML are available.

mod common;

use common::TestRepo;
use serde_json::Value;

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json envelope")
}

/// A native repo with `crates/` scaffolding so the lint rules that read the repo
/// working tree (R2 caps, R3 primitives) have something to resolve against.
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

/// Count frozen revisions in the ledger — proves whether a freeze happened.
fn revision_count(repo: &TestRepo) -> i64 {
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
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

/// A lint-clean v1 task contract whose file must be `demo.yaml` (id ccx-demo).
const CLEAN_CONTRACT: &str = "\
schema: ccx.contract.v1
id: ccx-demo
revision: 1
ticket: NER-999
task: Do a small thing
interface: Build the thing in the module.
acceptance:
  fix:
    - cargo test -p forge-core
  guard:
    - cargo clippy --workspace --all-targets -- -D warnings
allowed_changes:
  paths: [crates/forge-core/src/lib.rs]
authority: {source: human, confidence: high, reviewer: test}
";

#[test]
fn lint_clean_contract_freezes_and_reads_back_as_revision_1() {
    let repo = init_repo();
    write(&repo, "contracts/demo.yaml", CLEAN_CONTRACT);

    // Lint is clean.
    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .success(),
    );
    assert_eq!(lint["status"], "success");
    assert_eq!(lint["data"]["verdict"], "clean");
    assert_eq!(lint["data"]["contract_id"], "ccx-demo");

    // Freeze records revision 1.
    let freeze = json_output(
        repo.forge()
            .args(["--json", "contract", "freeze", "contracts/demo.yaml"])
            .assert()
            .success(),
    );
    assert_eq!(freeze["status"], "success");
    assert_eq!(freeze["data"]["revision"]["revision"], 1);
    assert_eq!(freeze["data"]["revision"]["contract_id"], "ccx-demo");
    assert_eq!(freeze["data"]["revision"]["lint_clean"], true);
    // Exact source bytes are stored verbatim (R1).
    assert_eq!(freeze["data"]["revision"]["source_yaml"], CLEAN_CONTRACT);
    assert_eq!(revision_count(&repo), 1);
}

#[test]
fn unknown_top_level_key_errors_naming_the_key_and_freezes_nothing() {
    // Covers AE4.
    let repo = init_repo();
    let contract = format!("{CLEAN_CONTRACT}surprise_key: nope\n");
    write(&repo, "contracts/demo.yaml", &contract);

    // Lint refuses, naming the offending key.
    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(lint["status"], "error");
    assert_eq!(error_code(&lint), "CONTRACT_LINT_FAILED");
    let violations = lint["errors"][0]["details"]["violations"].to_string();
    assert!(
        violations.contains("surprise_key"),
        "error must name the unknown key: {violations}"
    );

    // Freeze refuses too, and no revision is created.
    let freeze = json_output(
        repo.forge()
            .args(["--json", "contract", "freeze", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&freeze), "CONTRACT_LINT_FAILED");
    assert_eq!(
        revision_count(&repo),
        0,
        "no frozen revision on lint failure"
    );
}

#[test]
fn metacharacter_acceptance_refused_with_grammar_violation() {
    // Covers AE6 (shell-metacharacter form).
    let repo = init_repo();
    let contract = CLEAN_CONTRACT.replace(
        "    - cargo test -p forge-core",
        "    - cargo test -p forge-core; rm -rf /",
    );
    write(&repo, "contracts/demo.yaml", &contract);

    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&lint), "CONTRACT_GRAMMAR_VIOLATION");
    assert_eq!(revision_count(&repo), 0);

    // Freeze surfaces the same typed code.
    let freeze = json_output(
        repo.forge()
            .args(["--json", "contract", "freeze", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&freeze), "CONTRACT_GRAMMAR_VIOLATION");
    assert_eq!(revision_count(&repo), 0);
}

#[test]
fn non_cargo_acceptance_refused_with_grammar_violation() {
    // Covers AE6 (non-cargo command form).
    let repo = init_repo();
    let contract = CLEAN_CONTRACT.replace("    - cargo test -p forge-core", "    - make test");
    write(&repo, "contracts/demo.yaml", &contract);

    let lint = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&lint), "CONTRACT_GRAMMAR_VIOLATION");
    assert_eq!(revision_count(&repo), 0);
}

#[test]
fn wrong_visibility_primitive_errors() {
    let repo = init_repo();
    // A crate whose primitive is pub(crate), fenced off from consumers because
    // every allowed_changes path lies outside the owning crate (R3).
    write(
        &repo,
        "crates/mycrate/src/lib.rs",
        "pub(crate) fn my_primitive() {}\n",
    );
    let contract = "\
schema: ccx.contract.v1
id: ccx-demo
revision: 1
ticket: NER-999
task: Use the primitive
interface: Consume my_primitive from another crate.
acceptance:
  fix:
    - cargo test -p forge-core
allowed_changes:
  paths: [crates/other/src/lib.rs]
authority: {source: human, confidence: high, reviewer: test}
primitives:
  - {name: my_primitive, crate: mycrate, visibility: pub}
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
        violations.contains("my_primitive") && violations.contains("pub(crate)"),
        "must flag the fenced-off primitive: {violations}"
    );
}

#[test]
fn relative_and_absolute_operand_paths_are_equivalent() {
    // Edge: R19 canonicalization — a relative operand resolves identically to an
    // absolute one.
    let repo = init_repo();
    write(&repo, "contracts/demo.yaml", CLEAN_CONTRACT);

    let relative = json_output(
        repo.forge()
            .args(["--json", "contract", "lint", "contracts/demo.yaml"])
            .assert()
            .success(),
    );
    assert_eq!(relative["data"]["verdict"], "clean");

    let absolute_path = repo.path().join("contracts/demo.yaml");
    let absolute = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "lint",
                absolute_path.to_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(absolute["data"]["verdict"], "clean");
    // Both canonicalize to the same real path in the reported contract field.
    assert_eq!(relative["data"]["contract"], absolute["data"]["contract"]);
}

#[test]
fn global_policy_file_freezes_under_reserved_id() {
    let repo = init_repo();
    let policy = "\
schema: ccx.contract.v1
kind: global_policy
mechanics:
  build_system: cargo
rules:
  - Error handling is anyhow throughout.
unknown_rule: >
  If the brief does not license a decision, STOP and surface the unknown.
";
    write(&repo, "contracts/_global-policy.yaml", policy);

    // Global policy lints clean under the reduced rule set (no task-contract
    // shape / unknown-key strictness).
    let lint = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "lint",
                "contracts/_global-policy.yaml",
            ])
            .assert()
            .success(),
    );
    assert_eq!(lint["data"]["verdict"], "clean");
    assert_eq!(lint["data"]["contract_id"], "_global-policy");

    let freeze = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "freeze",
                "contracts/_global-policy.yaml",
            ])
            .assert()
            .success(),
    );
    assert_eq!(freeze["data"]["revision"]["contract_id"], "_global-policy");
    assert_eq!(freeze["data"]["revision"]["revision"], 1);
    assert_eq!(freeze["data"]["revision"]["source_yaml"], policy);
}

#[test]
fn freeze_replay_is_idempotent() {
    // R18: replaying a freeze request-id returns the original revision, not a
    // second one.
    let repo = init_repo();
    write(&repo, "contracts/demo.yaml", CLEAN_CONTRACT);
    let args = [
        "--json",
        "--request-id",
        "freeze-1",
        "contract",
        "freeze",
        "contracts/demo.yaml",
    ];
    let first = json_output(repo.forge().args(args).assert().success());
    assert_eq!(first["data"]["revision"]["revision"], 1);
    let replay = json_output(repo.forge().args(args).assert().success());
    assert_eq!(replay["status"], "success");
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(
        revision_count(&repo),
        1,
        "replay must not create a second revision"
    );
}

// ---------------------------------------------------------------------------
// U4: `forge contract brief` — byte-parity with tools/ccx/ccx-brief.py
// ---------------------------------------------------------------------------

/// Directory of the checked-in brief fixtures (contract YAML + Python-generated
/// EXPECTED bytes). The EXPECTED files were generated once from
/// `tools/ccx/ccx-brief.py` over the same YAML inputs (see the file header note in
/// the module docstring above); the native emitter must reproduce them exactly.
fn brief_fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/contract-briefs")
}

fn brief_fixture(name: &str) -> Vec<u8> {
    std::fs::read(brief_fixtures_dir().join(name)).expect("read brief fixture")
}

/// Copy a checked-in fixture YAML verbatim into the repo's `contracts/` dir so its
/// bytes flow into the frozen revision unchanged (R1) — the same bytes the Python
/// emitter read when the EXPECTED fixtures were generated.
fn install_brief_fixture(repo: &TestRepo, name: &str) {
    let bytes = brief_fixture(name);
    let path = repo.path().join("contracts").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).expect("create contracts dir");
    std::fs::write(path, bytes).expect("install fixture");
}

fn freeze_fixture(repo: &TestRepo, name: &str) {
    repo.forge()
        .args(["--json", "contract", "freeze", &format!("contracts/{name}")])
        .assert()
        .success();
}

/// The four fixture YAML files a full brief needs on disk (so the task lints), in
/// the order the task declares its neighbors.
const BRIEF_YAML_FILES: [&str; 4] = [
    "_global-policy.yaml",
    "brief-neighbor-a.yaml",
    "brief-neighbor-b.yaml",
    "brief-task.yaml",
];

#[test]
fn brief_matches_ccx_brief_py_expected_bytes() {
    // Primary U4 contract (R5): the native brief is byte-for-byte identical to the
    // Python emitter's output for the same frozen inputs. All four files are on
    // disk so the task lints; all four are frozen so every neighbor resolves.
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    freeze_fixture(&repo, "_global-policy.yaml");
    freeze_fixture(&repo, "brief-neighbor-a.yaml");
    freeze_fixture(&repo, "brief-neighbor-b.yaml");
    freeze_fixture(&repo, "brief-task.yaml");

    let output = repo
        .forge()
        .args(["contract", "brief", "ccx-brief-task"])
        .assert()
        .success();
    let stdout = &output.get_output().stdout;
    assert_eq!(
        stdout,
        &brief_fixture("expected-brief-full.txt"),
        "native brief must be byte-identical to ccx-brief.py output"
    );
}

#[test]
fn brief_missing_neighbor_reproduces_marker_bytes() {
    // A declared neighbor with NO frozen revision reproduces the Python MISSING
    // marker bytes and the brief still succeeds (exit 0), matching ccx-brief.py's
    // file-absent behavior. All four YAML are on disk so the task still lints;
    // neighbor-b is deliberately NOT frozen.
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    freeze_fixture(&repo, "_global-policy.yaml");
    freeze_fixture(&repo, "brief-neighbor-a.yaml");
    freeze_fixture(&repo, "brief-task.yaml");

    let output = repo
        .forge()
        .args(["contract", "brief", "ccx-brief-task"])
        .assert()
        .success();
    assert_eq!(
        &output.get_output().stdout,
        &brief_fixture("expected-brief-missing-b.txt"),
        "a declared-but-unfrozen neighbor must reproduce the Python MISSING marker bytes"
    );
}

#[test]
fn brief_is_byte_stable_across_invocations() {
    // R5: emitting the same frozen inputs twice yields identical bytes.
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    for name in BRIEF_YAML_FILES {
        freeze_fixture(&repo, name);
    }
    let first = repo
        .forge()
        .args(["contract", "brief", "ccx-brief-task"])
        .assert()
        .success();
    let second = repo
        .forge()
        .args(["contract", "brief", "ccx-brief-task"])
        .assert()
        .success();
    assert_eq!(
        first.get_output().stdout,
        second.get_output().stdout,
        "the same frozen inputs must emit byte-identical briefs"
    );
}

#[test]
fn brief_json_carries_text_and_out_writes_file() {
    // R23/operator surface: the --json envelope carries the brief text and neighbor
    // resolution; --out writes the same bytes to a file.
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    for name in BRIEF_YAML_FILES {
        freeze_fixture(&repo, name);
    }
    let expected = brief_fixture("expected-brief-full.txt");

    let envelope = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "brief",
                "ccx-brief-task",
                "--out",
                "brief.txt",
            ])
            .assert()
            .success(),
    );
    assert_eq!(envelope["status"], "success");
    assert_eq!(envelope["data"]["contract_id"], "ccx-brief-task");
    assert_eq!(
        envelope["data"]["brief"].as_str().unwrap().as_bytes(),
        expected.as_slice()
    );
    let neighbors = envelope["data"]["neighbors"].as_array().unwrap();
    assert_eq!(neighbors.len(), 2);
    assert_eq!(neighbors[0]["id"], "ccx-brief-neighbor-a");
    assert_eq!(neighbors[0]["present"], true);
    assert_eq!(neighbors[1]["id"], "ccx-brief-neighbor-b");

    // --out wrote the identical bytes to the file.
    let written = std::fs::read(repo.path().join("brief.txt")).expect("read out file");
    assert_eq!(written, expected);
}

#[test]
fn brief_refuses_when_contract_not_frozen() {
    // Fail-closed: no frozen revision for the requested id → typed
    // CONTRACT_NOT_FROZEN, no stdout brief.
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    freeze_fixture(&repo, "_global-policy.yaml");
    // brief-task is never frozen.
    let envelope = json_output(
        repo.forge()
            .args(["--json", "contract", "brief", "ccx-brief-task"])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&envelope), "CONTRACT_NOT_FROZEN");
}

/// Live parity check against the Python emitter itself (not the checked-in EXPECTED
/// bytes). Ignored by default because `python3` + PyYAML are not guaranteed on
/// every machine; the checked-in `expected-brief-full.txt` is the primary
/// assertion. Run explicitly with `cargo test -- --ignored`.
#[test]
#[ignore]
fn brief_matches_live_python_emitter() {
    let repo = init_repo();
    for name in BRIEF_YAML_FILES {
        install_brief_fixture(&repo, name);
    }
    for name in BRIEF_YAML_FILES {
        freeze_fixture(&repo, name);
    }
    let native = repo
        .forge()
        .args(["contract", "brief", "ccx-brief-task"])
        .assert()
        .success();

    // The repo root holds tools/ccx/ccx-brief.py; drive it over the fixture dir.
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root");
    let brief_py = repo_root.join("tools/ccx/ccx-brief.py");
    let fixtures = brief_fixtures_dir();
    let python = std::process::Command::new("python3")
        .arg(&brief_py)
        .arg("--contracts-dir")
        .arg(&fixtures)
        .arg(fixtures.join("brief-task.yaml"))
        .output()
        .expect("run ccx-brief.py");
    assert!(
        python.status.success(),
        "ccx-brief.py failed: {}",
        String::from_utf8_lossy(&python.stderr)
    );
    assert_eq!(
        native.get_output().stdout,
        python.stdout,
        "native brief must match the live ccx-brief.py emitter byte-for-byte"
    );
}

// ===========================================================================
// U5: `forge contract run` + `forge contract integrate`
// ===========================================================================

/// The `<name>.yaml` file name a `ccx-<name>` id requires (lint R1 correspondence).
fn contract_file_name(id: &str) -> String {
    format!("{}.yaml", id.strip_prefix("ccx-").unwrap_or(id))
}

/// A lint-clean v1 task contract for `id`, optionally declaring `depends_on`.
fn contract_yaml(id: &str, depends_on: &[&str]) -> String {
    let mut yaml = format!(
        "schema: ccx.contract.v1\n\
id: {id}\n\
revision: 1\n\
ticket: NER-999\n\
task: Do a small thing\n\
interface: Build the thing in the module.\n\
acceptance:\n  fix:\n    - cargo test -p forge-core\n\
allowed_changes:\n  paths: [crates/forge-core/src/lib.rs]\n\
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

/// Install `id`'s YAML on disk (so lint can resolve it as a neighbor/dependency).
fn install_contract(repo: &TestRepo, id: &str, depends_on: &[&str]) {
    write(
        repo,
        &format!("contracts/{}", contract_file_name(id)),
        &contract_yaml(id, depends_on),
    );
}

/// Install then freeze `id` (dependencies must already be installed on disk).
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

/// Freeze the reserved global policy so `contract run`'s brief emission succeeds.
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

/// A native repo with a frozen global policy — the run precondition.
fn run_repo() -> TestRepo {
    let repo = init_repo();
    freeze_global_policy(&repo);
    repo
}

/// Run `contract run` over `ids`, asserting the process exit code, and return the
/// `--json` envelope. A stop pairs exit 2 with envelope status success (R25).
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

/// A fake agent (executed via `sh -c` in the scratch workspace) that files a
/// well-formed four-field UNKNOWN.md stop.
const STOP_AGENT: &str =
    "printf 'What: need the shape\\nWhy: brief omits it\\nKind: blocking\\nEvidence: src/lib.rs:1\\n' > UNKNOWN.md";

/// A fake agent that produces a real (non-empty) patch by APPENDING to a file, so
/// each task in a chain adds a distinct delta over its (base + deps) baseline.
const EDIT_AGENT: &str = "echo change >> out.txt";

fn stop_count(repo: &TestRepo) -> i64 {
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    conn.query_row("SELECT COUNT(*) FROM contract_stops", [], |row| row.get(0))
        .expect("count stops")
}

#[test]
fn contract_run_two_task_chain_stop_halts_exit_2() {
    // Covers AE1 (R8/R9/R14/R25): task 1 files UNKNOWN.md → exit 2, open stop with
    // four fields, task 2 never runs, tally reports a successful stop.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    let env = contract_run(&repo, &["ccx-a", "ccx-b"], STOP_AGENT, 2);
    assert_eq!(env["status"], "success"); // a stop is never an envelope error (R25)
    assert_eq!(env["data"]["outcome"], "stopped");
    assert_eq!(env["data"]["exit_code"], 2);
    assert_eq!(env["data"]["malformed"], false);
    let stop_id = env["data"]["stop_id"].as_str().expect("stop id");
    let run_id = env["data"]["run_id"].as_str().expect("run id");

    // The stop carries all four redacted fields, un-malformed.
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    let (what, why, kind, evidence, malformed): (String, String, String, String, i64) = conn
        .query_row(
            "SELECT what_needed, why_unanswered, kind, evidence, malformed FROM contract_stops WHERE id = ?1",
            [stop_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("stop row");
    assert!(what.contains("need the shape"));
    assert!(why.contains("brief omits"));
    assert_eq!(kind, "blocking");
    assert!(evidence.contains("src/lib.rs:1"));
    assert_eq!(malformed, 0);

    // Task 1 (ccx-a) is a successful stop; task 2 (ccx-b) never executed (skipped).
    let mut stmt = conn
        .prepare(
            "SELECT task_id, outcome FROM contract_run_tasks WHERE run_id = ?1 ORDER BY task_index",
        )
        .unwrap();
    let tasks: Vec<(String, String)> = stmt
        .query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        tasks,
        vec![
            ("ccx-a".to_string(), "stopped".to_string()),
            ("ccx-b".to_string(), "skipped".to_string()),
        ]
    );
    // The run is recorded as a stop, not a failure.
    let run_outcome: String = conn
        .query_row(
            "SELECT outcome FROM contract_runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(run_outcome, "stopped");
}

#[test]
fn contract_run_refuses_when_contract_has_open_stop() {
    // Covers AE2 (R10): a contract with an open stop refuses a new run with a typed
    // error naming the stop id — and no agent session starts.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    // First run files a stop.
    let first = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = first["data"]["stop_id"].as_str().unwrap().to_string();

    // A second run is refused; the canary proves the agent never ran.
    let canary = repo.path().join("open-stop-canary");
    let agent = format!("touch {} && echo x > out.txt", canary.display());
    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "run", "ccx-a", "--agent-cmd", &agent])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_OPEN_STOP");
    let ids = env["errors"][0]["details"]["stop_ids"].to_string();
    assert!(
        ids.contains(&stop_id),
        "refusal names the blocking stop: {ids}"
    );
    assert!(
        !canary.exists(),
        "no agent session may start on an open-stop refusal"
    );
}

#[test]
fn contract_run_dependent_refusal_names_blocking_stop() {
    // Covers AE9 (refusal half, R10): a dependent's run refusal names the dependency's
    // blocking stop id.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    // Open a stop on the dependency ccx-a.
    let stop = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = stop["data"]["stop_id"].as_str().unwrap().to_string();

    // Running the chain (whose closure includes the blocked dependency ccx-a) is
    // refused, naming ccx-a's stop — a dependent cannot execute against an
    // unanswered unknown.
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
    assert_eq!(error_code(&env), "CONTRACT_OPEN_STOP");
    assert!(env["errors"][0]["details"]["stop_ids"]
        .to_string()
        .contains(&stop_id));
}

#[test]
fn contract_run_nonzero_exit_without_unknown_fails_exit_1() {
    // Covers AE5 (R11/R14): a crashed agent (nonzero exit, no UNKNOWN.md) records a
    // failed run with exit 1.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], "exit 3", 1);
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["outcome"], "failed");
    assert_eq!(env["data"]["exit_code"], 1);
    assert_eq!(stop_count(&repo), 0, "a crash is not a stop");
}

#[test]
fn contract_run_empty_patch_fails_exit_1() {
    // Covers AE8 (R11/R14): a zero exit with an empty patch records a failed run,
    // exit 1 — an empty patch never passes as success.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], "true", 1);
    assert_eq!(env["data"]["outcome"], "failed");
    assert_eq!(env["data"]["exit_code"], 1);
    assert!(env["data"]["reason"]
        .as_str()
        .unwrap()
        .contains("empty patch"));
}

#[test]
fn contract_run_stale_unknown_refuses_no_session_no_stop() {
    // Covers AE10 (R26): a stale UNKNOWN.md at the workspace root refuses with a typed
    // error before any agent session, and no stop record is created.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    write(&repo, "UNKNOWN.md", "leftover from a prior run\n");
    let canary = repo.path().join("stale-canary");
    let agent = format!("touch {} && echo x > out.txt", canary.display());
    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "run", "ccx-a", "--agent-cmd", &agent])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "STALE_UNKNOWN_FILE");
    assert!(!canary.exists(), "no agent session may start");
    assert_eq!(stop_count(&repo), 0, "no stop record is created");
}

#[test]
fn contract_run_malformed_unknown_opens_and_blocks() {
    // Malformed ingest (R8/R25): an UNKNOWN.md missing the four fields still opens a
    // stop, flagged malformed, and still blocks a rerun.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], "echo 'help I am stuck' > UNKNOWN.md", 2);
    assert_eq!(env["data"]["outcome"], "stopped");
    assert_eq!(env["data"]["malformed"], true);
    assert_eq!(env["data"]["code"], "CONTRACT_STOP_MALFORMED");

    // A malformed stop still blocks the next run (Leg 3).
    let rerun = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-a",
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&rerun), "CONTRACT_OPEN_STOP");
}

#[test]
fn contract_run_replay_returns_recorded_without_reexecuting_agent() {
    // KTD6: replaying a `contract run` request-id returns the recorded result and
    // NEVER re-executes the agent — proven by a canary the agent would recreate.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let canary = repo.path().join("replay-canary");
    let agent = format!("touch {} && echo change > out.txt", canary.display());
    let args = [
        "--json",
        "--request-id",
        "run-1",
        "contract",
        "run",
        "ccx-a",
        "--agent-cmd",
        &agent,
    ];
    let first = json_output(repo.forge().args(args).assert().code(0));
    assert_eq!(first["data"]["outcome"], "completed");
    let run_id = first["data"]["run_id"].as_str().unwrap().to_string();
    assert!(canary.exists(), "the first run executes the agent");
    std::fs::remove_file(&canary).expect("clear canary");

    // Replay: same request-id returns the recorded result; the agent does NOT run.
    let replay = json_output(repo.forge().args(args).assert().code(0));
    assert_eq!(replay["status"], "success");
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert!(!canary.exists(), "replay must not re-execute the agent");
    let runs: i64 = {
        let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM contract_runs", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(runs, 1, "replay must not record a second run");
    assert!(!run_id.is_empty());
}

#[test]
fn contract_run_per_id_dep_mismatch_refused_naming_missing() {
    // R20: an out-of-chain dependency with no acknowledging --dep is refused, naming
    // exactly the missing id — no silent count guard.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    // Run ccx-b alone (ccx-a is out-of-chain, unacknowledged).
    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-b",
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .failure(),
    );
    let message = env["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ccx-a") && message.contains("not acknowledged"),
        "refusal must name the missing dependency: {message}"
    );
}

#[test]
fn contract_integrate_single_contract_creates_linked_attempt() {
    // Covers R27/KTD8: a completed run with no dependencies integrates onto HEAD as an
    // attempt linked to the run and to contract@revision.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let run = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    let run_id = run["data"]["run_id"].as_str().unwrap().to_string();

    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "integrate", &run_id])
            .assert()
            .success(),
    );
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["run_id"], run_id);
    let attempt_id = env["data"]["attempt_id"].as_str().expect("attempt id");
    let intent_id = env["data"]["intent_id"].as_str().expect("intent id");

    // The attempt exists and its synthesized intent encodes the contract marker.
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let (exists, intent_text): (i64, String) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM attempts WHERE id = ?1), (SELECT text FROM intents WHERE id = ?2)",
            [attempt_id, intent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("attempt + intent rows");
    assert_eq!(exists, 1);
    assert!(
        intent_text.starts_with("contract ccx-a@rev1"),
        "intent: {intent_text}"
    );
}

#[test]
fn contract_integrate_before_deps_accepted_is_refused() {
    // KTD8: integrating a dependent before its dependency is accepted into HEAD is a
    // typed refusal naming the dependency.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    // Complete the whole chain (both tasks edit a file).
    let run = contract_run(&repo, &["ccx-a", "ccx-b"], EDIT_AGENT, 0);
    let run_id = run["data"]["run_id"].as_str().unwrap().to_string();

    // ccx-b's dependency ccx-a is not yet accepted → refusal.
    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "integrate", &run_id])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
    assert!(env["errors"][0]["details"]["reason"]
        .to_string()
        .contains("ccx-a"));
}

#[test]
fn contract_run_resume_replays_completed_tasks_without_reexecuting() {
    // KTD9: after a halted chain (task 1 completed, task 2 failed), a rerun with
    // --resume restarts at the halted task against the recorded completed-task
    // output — task 1's agent is NOT re-executed (canary), and the chain completes.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    // Phase 1: an agent that succeeds for ccx-a but crashes for ccx-b. The prompt
    // arrives on stdin; grep discriminates the task by its contract id.
    write(
        &repo,
        "fail-b.sh",
        "#!/bin/sh\nif grep -q 'id: ccx-b' >/dev/null 2>&1; then exit 3; fi\necho change >> out.txt\n",
    );
    let fail_b = format!("sh {}", repo.path().join("fail-b.sh").display());
    let halted = contract_run(&repo, &["ccx-a", "ccx-b"], &fail_b, 1);
    assert_eq!(halted["data"]["outcome"], "failed");
    let halted_run_id = halted["data"]["run_id"].as_str().unwrap().to_string();

    // Phase 2: resume. The agent touches a canary if it ever sees ccx-a's brief.
    let canary = repo.path().join("resume-canary");
    write(
        &repo,
        "resume-agent.sh",
        &format!(
            "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in *'id: ccx-a'*) touch {} ;; esac\necho more >> out.txt\n",
            canary.display()
        ),
    );
    let resume_agent = format!("sh {}", repo.path().join("resume-agent.sh").display());
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
                &resume_agent,
                "--resume",
                &halted_run_id,
            ])
            .assert()
            .code(0),
    );
    assert_eq!(env["data"]["outcome"], "completed");
    assert!(
        !canary.exists(),
        "resume must not re-execute the completed task's agent"
    );

    // The resumed run's per-task rows: ccx-a replayed as completed, ccx-b completed.
    let run_id = env["data"]["run_id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT task_id, outcome FROM contract_run_tasks WHERE run_id = ?1 ORDER BY task_index",
        )
        .unwrap();
    let tasks: Vec<(String, String)> = stmt
        .query_map([run_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        tasks,
        vec![
            ("ccx-a".to_string(), "completed".to_string()),
            ("ccx-b".to_string(), "completed".to_string()),
        ]
    );
}

#[test]
fn contract_run_resume_after_triage_resumes_from_stopped_task() {
    // KTD9 (resume-after-triage): a chain halted by a STOP on task 2 resumes from
    // that task once the stop is resolved — task 1 is replayed from its recorded
    // output (canary proves its agent is NOT re-executed) and the chain completes.
    // Leg 3 stays intact: before the stop is resolved, --resume is still refused.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    // Phase 1: ccx-a completes; ccx-b files a well-formed stop.
    write(
        &repo,
        "stop-b.sh",
        "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in *'id: ccx-b'*)\nprintf 'What: need the shape\\nWhy: brief omits it\\nKind: blocking\\nEvidence: src/lib.rs:1\\n' > UNKNOWN.md\nexit 0 ;;\nesac\necho change >> out.txt\n",
    );
    let stop_b = format!("sh {}", repo.path().join("stop-b.sh").display());
    let halted = contract_run(&repo, &["ccx-a", "ccx-b"], &stop_b, 2);
    assert_eq!(halted["data"]["outcome"], "stopped");
    let halted_run_id = halted["data"]["run_id"].as_str().unwrap().to_string();
    let stop_id = halted["data"]["stop_id"].as_str().unwrap().to_string();

    // Leg 3 unchanged: resuming while the stop is open is refused, no agent runs.
    let refused = json_output(
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
                "--resume",
                &halted_run_id,
            ])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&refused), "CONTRACT_OPEN_STOP");

    // Triage: resolve the stop. U8 ships the resolve subcommand; until then this
    // direct state flip is the test's stand-in for a recorded triage resolution.
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    conn.execute(
        "UPDATE contract_stops SET state = 'resolved', resolution_kind = 'rejection',
                resolution_rationale = 'test triage stand-in' WHERE id = ?1",
        [&stop_id],
    )
    .expect("resolve stop (U8 stand-in)");

    // Phase 2: resume. ccx-a is replayed (canary must stay silent); ccx-b runs.
    let canary = repo.path().join("triage-resume-canary");
    write(
        &repo,
        "resume-b.sh",
        &format!(
            "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in *'id: ccx-a'*) touch {} ;; esac\necho more >> out.txt\n",
            canary.display()
        ),
    );
    let resume_agent = format!("sh {}", repo.path().join("resume-b.sh").display());
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
                &resume_agent,
                "--resume",
                &halted_run_id,
            ])
            .assert()
            .code(0),
    );
    assert_eq!(env["data"]["outcome"], "completed");
    assert!(
        !canary.exists(),
        "resume-after-triage must not re-execute the completed task's agent"
    );
}

#[test]
fn contract_run_fresh_forces_full_reexecution() {
    // KTD9: `--fresh` forces a full re-run even when `--resume` names a resumable
    // halted run — the completed task's agent DOES execute again (canary fires).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    // Phase 1: halt the chain at ccx-b (agent crashes on ccx-b's brief).
    write(
        &repo,
        "fail-b.sh",
        "#!/bin/sh\nif grep -q 'id: ccx-b' >/dev/null 2>&1; then exit 3; fi\necho change >> out.txt\n",
    );
    let fail_b = format!("sh {}", repo.path().join("fail-b.sh").display());
    let halted = contract_run(&repo, &["ccx-a", "ccx-b"], &fail_b, 1);
    let halted_run_id = halted["data"]["run_id"].as_str().unwrap().to_string();

    // Phase 2: --fresh + --resume. The canary proves ccx-a's agent re-executed.
    let canary = repo.path().join("fresh-canary");
    write(
        &repo,
        "fresh-agent.sh",
        &format!(
            "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in *'id: ccx-a'*) touch {} ;; esac\necho more >> out.txt\n",
            canary.display()
        ),
    );
    let fresh_agent = format!("sh {}", repo.path().join("fresh-agent.sh").display());
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
                &fresh_agent,
                "--resume",
                &halted_run_id,
                "--fresh",
            ])
            .assert()
            .code(0),
    );
    assert_eq!(env["data"]["outcome"], "completed");
    assert!(
        canary.exists(),
        "--fresh must re-execute the previously completed task's agent"
    );
}

#[test]
fn contract_run_resume_with_unavailable_recorded_output_is_refused() {
    // KTD9: when a recorded completed-task output no longer applies onto the
    // rebuilt baseline (here: the patch content object is gone — a stand-in for a
    // GC'd or corrupted object), resume refuses with the typed
    // CONTRACT_NOT_INTEGRABLE and the message names the fresh-run guidance.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    write(
        &repo,
        "fail-b.sh",
        "#!/bin/sh\nif grep -q 'id: ccx-b' >/dev/null 2>&1; then exit 3; fi\necho change >> out.txt\n",
    );
    let fail_b = format!("sh {}", repo.path().join("fail-b.sh").display());
    let halted = contract_run(&repo, &["ccx-a", "ccx-b"], &fail_b, 1);
    let halted_run_id = halted["data"]["run_id"].as_str().unwrap().to_string();

    // Point the completed task's recorded patch at a well-formed ref whose object
    // does not exist in the store.
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let missing_ref = format!("forge-tree:{}", "0".repeat(64));
    conn.execute(
        "UPDATE contract_run_tasks SET patch_content_ref = ?1
         WHERE run_id = ?2 AND task_id = 'ccx-a'",
        rusqlite::params![missing_ref, halted_run_id],
    )
    .expect("tamper recorded patch ref");

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
                "--resume",
                &halted_run_id,
            ])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
    let reason = env["errors"][0]["details"]["reason"]
        .as_str()
        .unwrap_or_default();
    assert!(
        reason.contains("no longer applies") && reason.contains("--fresh"),
        "refusal must carry fresh-run guidance: {reason}"
    );
}

#[test]
fn contract_run_resume_of_unknown_run_is_refused() {
    // KTD9: resuming a run that does not exist is a refusal, not a silent fresh run.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-a",
                "--agent-cmd",
                EDIT_AGENT,
                "--resume",
                "contract_run_nonexistent",
            ])
            .assert()
            .failure(),
    );
    assert_eq!(env["status"], "error");
    let message = env["errors"][0]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no recorded run to resume"),
        "unexpected: {message}"
    );
}

#[test]
fn contract_integrate_incomplete_run_is_refused() {
    // A stopped (not completed) run cannot be integrated (typed refusal, KTD8).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let run = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let run_id = run["data"]["run_id"].as_str().unwrap().to_string();
    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "integrate", &run_id])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
}

#[test]
fn contract_run_agent_stderr_excerpt_is_stored_redacted() {
    // R7/R16: the agent subprocess stderr is captured on the per-task run row as a
    // redacted excerpt — a secret-looking token the agent prints to stderr never
    // enters the signed ledger row in the clear (redact-before-sign, KTD3).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);

    // The agent produces a real patch (so the task completes) and leaks a
    // secret-looking assignment to stderr.
    let agent = "echo change >> out.txt; echo 'password=hunter2SECRETvalue' 1>&2";
    let env = contract_run(&repo, &["ccx-a"], agent, 0);
    assert_eq!(env["data"]["outcome"], "completed");
    let run_id = env["data"]["run_id"].as_str().unwrap();

    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let stderr_excerpt: String = conn
        .query_row(
            "SELECT agent_stderr_excerpt FROM contract_run_tasks WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .expect("stderr excerpt stored on the task row");
    assert!(
        stderr_excerpt.contains("[REDACTED]"),
        "stderr must be redacted before storage: {stderr_excerpt:?}"
    );
    assert!(
        !stderr_excerpt.contains("hunter2SECRETvalue"),
        "the raw secret must never be persisted: {stderr_excerpt:?}"
    );
}
