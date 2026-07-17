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

/// Install then freeze `id` with a custom `allowed_changes.paths` inline YAML list
/// (U6 blast tests that need a specific allowlist — e.g. one that tries to weaken the
/// default-forbid set).
fn freeze_contract_with_paths(repo: &TestRepo, id: &str, paths_yaml: &str) {
    let yaml = format!(
        "schema: ccx.contract.v1\n\
id: {id}\n\
revision: 1\n\
ticket: NER-999\n\
task: Do a small thing\n\
interface: Build the thing in the module.\n\
acceptance:\n  fix:\n    - cargo test -p forge-core\n\
allowed_changes:\n  paths: {paths_yaml}\n\
authority: {{source: human, confidence: high, reviewer: test}}\n"
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
fn contract_run_transitive_open_stop_through_acked_dep_refuses() {
    // B1 (R10 Leg-3): the refusal must walk the FULL transitive dependency closure,
    // not just the chain contracts plus directly-acknowledged deps. Chain: c → b → a
    // (c depends on b, b on a). Acknowledge b for a run of c; b's FROZEN depends_on
    // names a, and a has an open stop reached only THROUGH b. The run must refuse
    // naming a's stop, and no agent session may start.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    freeze_contract(&repo, "ccx-c", &["ccx-b"]);

    // A completed run of a, then of b (acknowledging a), so a real b-run ref exists
    // to acknowledge when running c — all BEFORE any stop is opened.
    let a_run = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    let a_run_id = a_run["data"]["run_id"].as_str().unwrap().to_string();
    let dep_a = format!("ccx-a={a_run_id}");
    let b_run = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-b",
                "--dep",
                dep_a.as_str(),
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .code(0),
    );
    let b_run_id = b_run["data"]["run_id"].as_str().unwrap().to_string();

    // Now open a stop on the DEEP dependency a (a fresh run of a that stops).
    let a_stop = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let a_stop_id = a_stop["data"]["stop_id"].as_str().unwrap().to_string();

    // Run c, acknowledging b. a is neither in the chain nor directly acknowledged —
    // it is reached only by expanding b's frozen depends_on. The transitive closure
    // must still find a's open stop and refuse; the canary proves no agent ran.
    let canary = repo.path().join("transitive-closure-canary");
    let agent = format!("touch {} && echo x >> out.txt", canary.display());
    let dep_b = format!("ccx-b={b_run_id}");
    let env = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-c",
                "--dep",
                dep_b.as_str(),
                "--agent-cmd",
                agent.as_str(),
            ])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_OPEN_STOP");
    assert!(
        env["errors"][0]["details"]["stop_ids"]
            .to_string()
            .contains(&a_stop_id),
        "refusal must name the transitively-reached blocking stop on ccx-a"
    );
    assert!(
        !canary.exists(),
        "no agent session may start on a transitive open-stop refusal"
    );
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
fn contract_integrate_deps_gate_rejects_spoofed_intent_accept() {
    // B3/KTD8 (anti-spoof): the deps-accepted gate must bind to a genuine recorded
    // contract integration, NOT to intent text. An ordinary
    // `forge start "contract ccx-a@rev1 ..."` → save → propose → accept produces an
    // accepted decision whose intent text would satisfy the old `LIKE 'contract
    // ccx-a@%'` predicate WITHOUT any real integration. Integrating the dependent
    // ccx-b must still be refused because ccx-a was never truly integrated.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);
    let run = contract_run(&repo, &["ccx-a", "ccx-b"], EDIT_AGENT, 0);
    let run_id = run["data"]["run_id"].as_str().unwrap().to_string();

    // Spoof: a genuine accepted decision whose intent text mimics an integration
    // marker for ccx-a, produced through the ordinary lifecycle (never
    // `forge contract integrate`).
    repo.forge()
        .args(["--json", "start", "contract ccx-a@rev1 task ccx-a"])
        .assert()
        .success();
    write(&repo, "spoof.txt", "not a real integration\n");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    repo.forge().args(["--json", "accept"]).assert().success();

    // The dependent's integration must STILL be refused: ccx-a's dependency was
    // spoofed, not integrated, so the genuine integration-link gate finds nothing.
    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "integrate", &run_id])
            .assert()
            .failure(),
    );
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
    assert!(
        env["errors"][0]["details"]["reason"]
            .to_string()
            .contains("ccx-a"),
        "refusal must name the un-integrated dependency"
    );
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

    // Triage: resolve the stop via the real U8 subcommand (explicit rejection).
    let resolved = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "resolve",
                &stop_id,
                "--reject",
                "--rationale",
                "brief already covers it; not a real gap",
            ])
            .assert()
            .success(),
    );
    assert_eq!(resolved["status"], "success");
    assert_eq!(resolved["data"]["resolved_stop"]["state"], "resolved");

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

// ===========================================================================
// U6: blast-radius postflight (R12/R16/AE7)
// ===========================================================================

/// Count `blast` verdict rows for a run, optionally filtered to failures.
fn blast_verdict_details(repo: &TestRepo, run_id: &str, passed: bool) -> Vec<String> {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(detail, '') FROM contract_run_verdicts
             WHERE run_id = ?1 AND verdict_kind = 'blast' AND passed = ?2
             ORDER BY id",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![run_id, passed as i64], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows
}

fn task_outcome(repo: &TestRepo, run_id: &str, task_id: &str) -> String {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    conn.query_row(
        "SELECT outcome FROM contract_run_tasks WHERE run_id = ?1 AND task_id = ?2",
        rusqlite::params![run_id, task_id],
        |r| r.get(0),
    )
    .expect("task row")
}

#[test]
fn contract_run_blast_forge_path_violation_exit_3() {
    // Covers AE7 (R12/R14/R25): a produced patch that touches `.forge/` yields exit 3,
    // a blast verdict naming the forbidden path, envelope outcome blast_violation, and
    // no dependent task executes. The agent also edits an allowed file so the patch is
    // non-empty (the `.forge/` write alone is snapshot-excluded — the fs walk still
    // catches it).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    let agent = "mkdir -p .forge && echo x > .forge/canary && echo change >> out.txt";
    let env = contract_run(&repo, &["ccx-a", "ccx-b"], agent, 3);
    assert_eq!(env["status"], "success"); // exit 3 pairs with a success envelope (R25)
    assert_eq!(env["data"]["outcome"], "blast_violation");
    assert_eq!(env["data"]["exit_code"], 3);
    assert_eq!(env["data"]["code"], "CONTRACT_BLAST_VIOLATION");
    assert_eq!(env["data"]["secret_content_detected"], false);
    let run_id = env["data"]["run_id"].as_str().unwrap().to_string();

    // The violation names the forbidden path (path only, never content).
    let violations = env["data"]["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["path"] == ".forge/canary" && v["class"] == "forbidden_path"),
        "violation must name .forge/canary: {violations:?}"
    );

    // A failing blast verdict names the path.
    let details = blast_verdict_details(&repo, &run_id, false);
    assert!(
        details.iter().any(|d| d.contains(".forge/canary")),
        "a failing blast verdict must name the path: {details:?}"
    );

    // The dependent ccx-b never ran (skipped).
    assert_eq!(task_outcome(&repo, &run_id, "ccx-a"), "failed");
    assert_eq!(task_outcome(&repo, &run_id, "ccx-b"), "skipped");
}

#[test]
fn contract_run_blast_outside_allowlist_violation() {
    // A change outside the contract's allowed_changes.paths globs (and not a facade)
    // is a blast violation (R12) — exit 3.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    // out.txt is allowed; stray.txt is not.
    let agent = "echo change >> out.txt && echo stray > stray.txt";
    let env = contract_run(&repo, &["ccx-a"], agent, 3);
    assert_eq!(env["data"]["outcome"], "blast_violation");
    let violations = env["data"]["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["path"] == "stray.txt" && v["class"] == "forbidden_path"),
        "stray.txt must be an outside-allowlist violation: {violations:?}"
    );
}

#[test]
fn contract_run_blast_facade_decl_only_change_passes() {
    // A declaration-only edit to a default facade file (crates/forge-cli/src/main.rs)
    // is permitted even though it is outside the allowlist (statement-aware facade
    // allowance ported from ccx-blast.py) — the run completes exit 0.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let agent = "mkdir -p crates/forge-cli/src && printf 'pub mod alpha;\\npub mod beta;\\n' > crates/forge-cli/src/main.rs";
    let env = contract_run(&repo, &["ccx-a"], agent, 0);
    assert_eq!(env["data"]["outcome"], "completed");
}

#[test]
fn contract_run_blast_facade_executable_change_violates() {
    // A NON-declaration (executable) edit to a facade file is NOT covered by the
    // facade allowance — it is a blast violation (exit 3).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let agent = "mkdir -p crates/forge-cli/src && printf 'fn main() { evil(); }\\n' > crates/forge-cli/src/main.rs";
    let env = contract_run(&repo, &["ccx-a"], agent, 3);
    assert_eq!(env["data"]["outcome"], "blast_violation");
    let violations = env["data"]["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["path"] == "crates/forge-cli/src/main.rs"),
        "the non-decl facade edit must violate: {violations:?}"
    );
}

#[test]
fn contract_run_blast_default_forbid_not_weakenable() {
    // R12: a contract that EXPLICITLY allows `.forge/**` still cannot weaken the
    // non-weakenable default-forbid list — a `.forge/` write is still a violation.
    let repo = run_repo();
    freeze_contract_with_paths(&repo, "ccx-a", "[.forge/**, out.txt]");
    let agent = "mkdir -p .forge && echo x > .forge/loot && echo change >> out.txt";
    let env = contract_run(&repo, &["ccx-a"], agent, 3);
    assert_eq!(env["data"]["outcome"], "blast_violation");
    let violations = env["data"]["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["path"] == ".forge/loot" && v["class"] == "forbidden_path"),
        "default-forbid must win over an explicit allow: {violations:?}"
    );
}

#[test]
fn contract_run_blast_secret_content_refused_patch_not_persisted() {
    // R16 detect-and-refuse: an allowed file whose post-state content carries a
    // secret-looking assignment fails the run (exit 3, secret_content class), the run
    // record carries NO patch ref, and the raw secret bytes are absent from the run
    // record (asserted via direct DB query).
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    // out.txt is an allowed path; its content trips the shared secret detector.
    let secret = "AKIASEKRETTESTMARKER1234";
    let agent = format!("printf 'api_key = \"{secret}\"\\n' > out.txt");
    let env = contract_run(&repo, &["ccx-a"], &agent, 3);
    assert_eq!(env["data"]["outcome"], "blast_violation");
    assert_eq!(env["data"]["secret_content_detected"], true);
    let run_id = env["data"]["run_id"].as_str().unwrap().to_string();
    let violations = env["data"]["violations"].as_array().expect("violations");
    assert!(
        violations
            .iter()
            .any(|v| v["path"] == "out.txt" && v["class"] == "secret_content"),
        "the secret file must be named (path only): {violations:?}"
    );

    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    // The run row carries no patch ref (the offending tree is left unreferenced for GC).
    let run_patch: Option<String> = conn
        .query_row(
            "SELECT patch_content_ref FROM contract_runs WHERE id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        run_patch.is_none(),
        "run must carry no patch ref: {run_patch:?}"
    );
    let task_patch: Option<String> = conn
        .query_row(
            "SELECT patch_content_ref FROM contract_run_tasks WHERE run_id = ?1 AND task_id = 'ccx-a'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        task_patch.is_none(),
        "task must carry no patch ref: {task_patch:?}"
    );

    // The raw secret bytes never enter the run record (DB) in any column.
    let leaked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contract_run_tasks
             WHERE run_id = ?1 AND (
                COALESCE(agent_stdout_excerpt, '') LIKE '%AKIASEKRETTESTMARKER%'
                OR COALESCE(agent_stderr_excerpt, '') LIKE '%AKIASEKRETTESTMARKER%'
                OR COALESCE(patch_content_ref, '') LIKE '%AKIASEKRETTESTMARKER%')",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        leaked, 0,
        "the raw secret must never be persisted in the run record"
    );
    let verdict_leak: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM contract_run_verdicts
             WHERE run_id = ?1 AND COALESCE(detail, '') LIKE '%AKIASEKRETTESTMARKER%'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        verdict_leak, 0,
        "the verdict detail must name the path only, never the secret"
    );
}

#[test]
fn contract_run_clean_records_blast_pass_verdict() {
    // Regression: a clean run still completes exit 0, AND records a per-task `blast`
    // pass verdict so the ledger shows the check ran green.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    assert_eq!(env["data"]["outcome"], "completed");
    let run_id = env["data"]["run_id"].as_str().unwrap().to_string();
    let passes = blast_verdict_details(&repo, &run_id, true);
    assert!(
        !passes.is_empty(),
        "a clean run must record at least one blast pass verdict"
    );
}

// ===========================================================================
// U7: `forge contract verify` — fix/guard re-verification on a rebuilt base
// ===========================================================================
//
// The scratch base a verify rebuilds is the run's full post-state tree, so the
// acceptance commands must run against a real cargo project committed as the run
// base. The fixture is a TINY, dependency-free standalone crate at the repo root:
// `cargo fmt --check` (no compile) and `cargo build` (a trivial compile) are both
// cheap and hermetic (no network). The agent only edits the allowed `out.txt`, so
// blast stays clean and the run completes — leaving the crate's formatting state
// (well-formed vs. deliberately mis-formatted) as the lever that drives fix/guard
// pass/fail deterministically.

const VERIFY_CARGO_TOML: &str =
    "[package]\nname = \"verifyfixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
/// rustfmt-clean, compiles — both `cargo build` and `cargo fmt --check` pass.
const GOOD_LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
/// Compiles fine (so `cargo build` passes) but mis-formatted (so `cargo fmt --check`
/// fails) — the single lever for the exit-4 (guard regressed) and exit-2 (fix failed)
/// cases, swapping which command is fix vs. guard.
const MISFORMATTED_LIB: &str = "pub fn add(a:i32,b:i32)->i32{a+b}\n";

/// Install the standalone cargo fixture crate into the repo worktree so the run base
/// (and therefore the rebuilt verify scratch) contains a real cargo project.
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

/// Freeze a lint-clean verify contract for `id` with the given fix/guard sets. The
/// agent only touches `out.txt` (the sole allowed path), so the run completes clean.
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

/// Run `contract verify <target>`, asserting the process exit code, returning the JSON.
fn contract_verify(repo: &TestRepo, target: &str, expect_exit: i32) -> Value {
    json_output(
        repo.forge()
            .args(["--json", "contract", "verify", target])
            .assert()
            .code(expect_exit),
    )
}

/// Count verify verdict rows (fix/guard/aggregate) for a run — excludes the `blast`
/// verdict the run itself records, so it isolates what verify wrote.
fn verify_verdict_count(repo: &TestRepo, run_id: &str) -> i64 {
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    conn.query_row(
        "SELECT COUNT(*) FROM contract_run_verdicts
         WHERE run_id = ?1 AND verdict_kind IN ('fix','guard','aggregate')",
        [run_id],
        |r| r.get(0),
    )
    .expect("count verify verdicts")
}

/// A completed run of a single verify contract, returning its run id.
fn completed_verify_run(repo: &TestRepo, id: &str) -> String {
    let env = contract_run(repo, &[id], EDIT_AGENT, 0);
    assert_eq!(env["data"]["outcome"], "completed");
    env["data"]["run_id"].as_str().unwrap().to_string()
}

#[test]
fn contract_verify_all_green_passes_and_records_aggregate() {
    // All fix + guard green → exit 0, outcome passed, an aggregate verdict recorded.
    let repo = run_repo();
    install_verify_crate(&repo, false);
    freeze_verify_contract(&repo, "ccx-vok", &["cargo build"], &["cargo fmt --check"]);
    let run_id = completed_verify_run(&repo, "ccx-vok");

    let env = contract_verify(&repo, &run_id, 0);
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["outcome"], "passed");
    assert_eq!(env["data"]["exit_code"], 0);
    assert!(
        env["data"].get("code").is_none(),
        "clean verify has no code"
    );

    // One fix + one guard + one aggregate verdict row, all passed.
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    let agg_passed: i64 = conn
        .query_row(
            "SELECT passed FROM contract_run_verdicts
             WHERE run_id = ?1 AND verdict_kind = 'aggregate'",
            [&run_id],
            |r| r.get(0),
        )
        .expect("aggregate verdict exists");
    assert_eq!(agg_passed, 1, "aggregate must be a pass on all-green");
    assert_eq!(
        verify_verdict_count(&repo, &run_id),
        3,
        "fix + guard + aggregate verdict rows"
    );
}

#[test]
fn contract_verify_guard_regression_exit_4() {
    // Covers AE3 (R13/R14/R25): fix passes, one guard regresses → exit 4,
    // CONTRACT_GUARD_REGRESSED in the envelope, per-command verdict rows for every
    // entry including the failed guard. The mis-formatted crate compiles (fix
    // `cargo build` passes) but fails `cargo fmt --check` (the guard).
    let repo = run_repo();
    install_verify_crate(&repo, true);
    freeze_verify_contract(
        &repo,
        "ccx-vguard",
        &["cargo build"],
        &["cargo fmt --check"],
    );
    let run_id = completed_verify_run(&repo, "ccx-vguard");

    let env = contract_verify(&repo, &run_id, 4);
    assert_eq!(env["status"], "success"); // a regression is not an envelope error (R25)
    assert_eq!(env["data"]["outcome"], "guard_regressed");
    assert_eq!(env["data"]["exit_code"], 4);
    assert_eq!(env["data"]["code"], "CONTRACT_GUARD_REGRESSED");

    // A verdict row exists for EVERY entry, and the failed guard is recorded failing.
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    let fix_passed: i64 = conn
        .query_row(
            "SELECT passed FROM contract_run_verdicts
             WHERE run_id = ?1 AND verdict_kind = 'fix' AND command = 'cargo build'",
            [&run_id],
            |r| r.get(0),
        )
        .expect("fix verdict row exists");
    assert_eq!(fix_passed, 1, "fix (cargo build) must pass");
    let guard_passed: i64 = conn
        .query_row(
            "SELECT passed FROM contract_run_verdicts
             WHERE run_id = ?1 AND verdict_kind = 'guard' AND command = 'cargo fmt --check'",
            [&run_id],
            |r| r.get(0),
        )
        .expect("failed guard verdict row exists");
    assert_eq!(
        guard_passed, 0,
        "the regressed guard must be recorded failing"
    );
}

#[test]
fn contract_verify_fix_failure_exit_2_guards_still_run() {
    // Fix failure → exit 2, CONTRACT_FIX_FAILED — and guards STILL run (verify-task.sh
    // record-completeness). Same mis-formatted crate, but now `cargo fmt --check` is
    // the FIX (fails) and `cargo build` is the GUARD (still executed, passes).
    let repo = run_repo();
    install_verify_crate(&repo, true);
    freeze_verify_contract(&repo, "ccx-vfix", &["cargo fmt --check"], &["cargo build"]);
    let run_id = completed_verify_run(&repo, "ccx-vfix");

    let env = contract_verify(&repo, &run_id, 2);
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["outcome"], "fix_failed");
    assert_eq!(env["data"]["exit_code"], 2);
    assert_eq!(env["data"]["code"], "CONTRACT_FIX_FAILED");

    // The guard ran despite the fix failure — its verdict row proves it (R13).
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(db).expect("open db");
    let guard_passed: i64 = conn
        .query_row(
            "SELECT passed FROM contract_run_verdicts
             WHERE run_id = ?1 AND verdict_kind = 'guard' AND command = 'cargo build'",
            [&run_id],
            |r| r.get(0),
        )
        .expect("guard verdict row exists even though the fix failed");
    assert_eq!(guard_passed, 1, "guards always run and are recorded (R13)");
}

#[test]
fn contract_verify_grammar_violation_executes_nothing() {
    // Fail-closed standalone (R15): a frozen revision whose acceptance carries a
    // non-cargo command (simulating a tampered/non-lint-clean frozen contract reaching
    // verify) is refused CONTRACT_GRAMMAR_VIOLATION with ZERO commands executed — no
    // verify verdict row is recorded (the canary). Verify never trusts that freeze
    // linted the contract clean.
    let repo = run_repo();
    install_verify_crate(&repo, false);
    freeze_verify_contract(&repo, "ccx-vgrammar", &["cargo build"], &[]);
    let run_id = completed_verify_run(&repo, "ccx-vgrammar");

    // Tamper the frozen revision's stored bytes to inject an eval-sink command that
    // lint would have refused — verify must re-gate it before executing anything.
    let db = repo.path().join(".forge/forge.db");
    let conn = rusqlite::Connection::open(&db).expect("open db");
    conn.execute(
        "UPDATE contract_revisions SET source_yaml = ?1
         WHERE contract_id = 'ccx-vgrammar' AND revision = 1",
        rusqlite::params![
            "schema: ccx.contract.v1\nid: ccx-vgrammar\nrevision: 1\n\
acceptance:\n  fix:\n    - rm -rf /\n"
        ],
    )
    .expect("tamper revision yaml");
    drop(conn);

    let env = contract_verify(&repo, &run_id, 1);
    assert_eq!(env["status"], "error");
    assert_eq!(error_code(&env), "CONTRACT_GRAMMAR_VIOLATION");
    // Canary: nothing executed → no verify verdict row was recorded for the run.
    assert_eq!(
        verify_verdict_count(&repo, &run_id),
        0,
        "a grammar violation must execute and record nothing"
    );
}

#[test]
fn contract_verify_replay_returns_recorded_without_reexecuting() {
    // R18/KTD6: replaying a verify request-id returns the recorded result WITHOUT
    // re-executing the acceptance commands — the verify verdict rows do not grow.
    let repo = run_repo();
    install_verify_crate(&repo, false);
    freeze_verify_contract(
        &repo,
        "ccx-vreplay",
        &["cargo build"],
        &["cargo fmt --check"],
    );
    let run_id = completed_verify_run(&repo, "ccx-vreplay");

    let args = [
        "--json",
        "--request-id",
        "verify-1",
        "contract",
        "verify",
        &run_id,
    ];
    let first = json_output(repo.forge().args(args).assert().code(0));
    assert_eq!(first["data"]["outcome"], "passed");
    let after_first = verify_verdict_count(&repo, &run_id);
    assert_eq!(
        after_first, 3,
        "first verify records fix + guard + aggregate"
    );

    let replay = json_output(repo.forge().args(args).assert().code(0));
    assert_eq!(replay["status"], "success");
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(
        verify_verdict_count(&repo, &run_id),
        after_first,
        "replay must not re-execute or record a second verdict batch"
    );
}

#[test]
fn contract_verify_of_stopped_run_is_typed_refusal() {
    // A stopped run has no produced patch to verify → typed refusal, no verdicts.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let stopped = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let run_id = stopped["data"]["run_id"].as_str().unwrap().to_string();

    let env = contract_verify(&repo, &run_id, 1);
    assert_eq!(env["status"], "error");
    assert_eq!(error_code(&env), "CONTRACT_NOT_INTEGRABLE");
    assert_eq!(
        verify_verdict_count(&repo, &run_id),
        0,
        "an incomplete run yields no verify verdicts"
    );
}

// ===========================================================================
// U8: query (`stops` / `show` / `verdicts`) + triage (`resolve`)
// ===========================================================================

/// Run `contract resolve` and return the `--json` envelope.
fn contract_resolve(repo: &TestRepo, extra: &[&str]) -> Value {
    let mut args: Vec<String> = vec!["--json".into(), "contract".into(), "resolve".into()];
    for a in extra {
        args.push((*a).to_string());
    }
    json_output(repo.forge().args(&args).assert().success())
}

#[test]
fn contract_stops_open_lists_both_contracts_with_triageable_fields() {
    // Covers AE9 (query half, R23): two contracts each with an open stop are both
    // listed by `stops --open` with the four triageable fields and the malformed flag.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-c", &[]);

    // Each independent contract files its own well-formed stop.
    contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    contract_run(&repo, &["ccx-c"], STOP_AGENT, 2);
    assert_eq!(stop_count(&repo), 2);

    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "stops", "--open"])
            .assert()
            .success(),
    );
    assert_eq!(env["status"], "success");
    assert_eq!(env["data"]["count"], 2);
    let stops = env["data"]["stops"].as_array().expect("stops array");
    assert_eq!(stops.len(), 2);
    let ids: Vec<&str> = stops
        .iter()
        .map(|s| s["contract_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"ccx-a") && ids.contains(&"ccx-c"), "{ids:?}");
    for stop in stops {
        // All four triageable fields plus the malformed flag and blocking status.
        assert!(stop["what_needed"]
            .as_str()
            .unwrap()
            .contains("need the shape"));
        assert!(stop["why_unanswered"]
            .as_str()
            .unwrap()
            .contains("brief omits"));
        assert_eq!(stop["kind"], "blocking");
        assert!(stop["evidence"].as_str().unwrap().contains("src/lib.rs:1"));
        assert_eq!(stop["malformed"], false);
        assert_eq!(stop["blocking"], true);
        assert_eq!(stop["state"], "open");
        assert_eq!(stop["revision"], 1);
    }
}

#[test]
fn contract_resolve_revision_bump_clears_stop_and_licenses_rerun() {
    // Covers R10/R24 end-to-end: a run is refused while a stop is open; a
    // revision-bump resolution clears it and licenses a fresh rerun.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let stopped = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = stopped["data"]["stop_id"].as_str().unwrap().to_string();

    // Refused while open.
    let refused = json_output(
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
    assert_eq!(error_code(&refused), "CONTRACT_OPEN_STOP");

    // Revise the contract on disk (still lint-clean, same id) and resolve --revised.
    let revised = contract_yaml("ccx-a", &[]).replace(
        "interface: Build the thing in the module.",
        "interface: Build the thing in the module (clarified after triage).",
    );
    write(&repo, "contracts/a.yaml", &revised);
    let resolved = contract_resolve(&repo, &[&stop_id, "--revised", "contracts/a.yaml"]);
    assert_eq!(resolved["data"]["resolution_kind"], "revision");
    assert_eq!(resolved["data"]["resolved_stop"]["state"], "resolved");
    assert_eq!(resolved["data"]["revision"]["revision"], 2);
    assert_eq!(resolved["data"]["resolved_stop"]["resolving_revision"], 2);

    // The rerun is now licensed and completes.
    let rerun = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    assert_eq!(rerun["data"]["outcome"], "completed");
}

#[test]
fn contract_resolve_rejection_bumps_revision_without_changing_content() {
    // Covers R10: an explicit rejection bumps the revision recording the rationale
    // WITHOUT changing contract content, and the rationale is redacted before store.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let stopped = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = stopped["data"]["stop_id"].as_str().unwrap().to_string();

    // Capture the frozen revision-1 bytes before resolving.
    let before = json_output(
        repo.forge()
            .args(["--json", "contract", "show", "ccx-a"])
            .assert()
            .success(),
    );
    let original_yaml = before["data"]["revision"]["source_yaml"]
        .as_str()
        .unwrap()
        .to_string();

    let resolved = contract_resolve(
        &repo,
        &[
            &stop_id,
            "--reject",
            "--rationale",
            "not a real gap; API_TOKEN=supersecretvalue in the config already answers it",
        ],
    );
    assert_eq!(resolved["data"]["resolution_kind"], "rejection");
    assert_eq!(resolved["data"]["revision"]["revision"], 2);
    // Content byte-for-byte unchanged (R10 rejection preserves content).
    assert_eq!(resolved["data"]["revision"]["source_yaml"], original_yaml);
    // The rationale is redacted before it enters the signed record (KTD3).
    let stop_rationale = resolved["data"]["resolved_stop"]["resolution_rationale"]
        .as_str()
        .unwrap();
    assert!(
        !stop_rationale.contains("supersecretvalue"),
        "rationale must be redacted: {stop_rationale}"
    );
    let rev_rationale = resolved["data"]["revision"]["resolution_rationale"]
        .as_str()
        .unwrap();
    assert!(
        !rev_rationale.contains("supersecretvalue"),
        "revision rationale must be redacted: {rev_rationale}"
    );

    // The stop no longer blocks reruns.
    let rerun = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    assert_eq!(rerun["data"]["outcome"], "completed");
}

#[test]
fn contract_resolve_reconstructs_malformed_stop_fields() {
    // Malformed reconstruction (R8/R25): a malformed stop's four fields are supplied
    // at resolve time; the resulting record is complete and its signature verifies.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], "echo 'help I am stuck' > UNKNOWN.md", 2);
    assert_eq!(env["data"]["malformed"], true);
    let stop_id = env["data"]["stop_id"].as_str().unwrap().to_string();

    let resolved = contract_resolve(
        &repo,
        &[
            &stop_id,
            "--reject",
            "--rationale",
            "reconstructed after triage",
            "--what-needed",
            "the exact trait bound to implement",
            "--why-unanswered",
            "the brief omits the bound",
            "--kind",
            "blocking",
            "--evidence",
            "crates/forge-core/src/lib.rs:10",
        ],
    );
    let stop = &resolved["data"]["resolved_stop"];
    assert_eq!(stop["state"], "resolved");
    // Reconstruction cleared the malformed flag: all four fields are now present.
    assert_eq!(stop["malformed"], false);
    assert!(stop["what_needed"]
        .as_str()
        .unwrap()
        .contains("trait bound"));
    assert!(stop["why_unanswered"].as_str().unwrap().contains("omits"));
    assert_eq!(stop["kind"], "blocking");
    assert!(stop["evidence"].as_str().unwrap().contains("lib.rs:10"));

    // The re-signed record is valid: a ledger signature exists for the stop whose
    // signed digest matches the record's CURRENT content hash (reconstruction
    // re-signed the mutated row). Full two-sided `forge doctor` coverage of the new
    // kinds lands in U9 (KTD2); here we assert the re-sign directly.
    let content_hash = stop["content_hash"].as_str().unwrap();
    let stop_id_val = stop["stop_id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let signed_digest: String = conn
        .query_row(
            "SELECT signed_digest FROM ledger_signatures
             WHERE subject_kind = 'contract_stop' AND subject_id = ?1
             ORDER BY rowid DESC LIMIT 1",
            [stop_id_val],
            |r| r.get(0),
        )
        .expect("a signature row exists for the reconstructed stop");
    assert_eq!(
        signed_digest, content_hash,
        "the re-signed stop's signature must cover its current content hash"
    );
}

#[test]
fn contract_resolve_reconstruction_refused_on_well_formed_stop() {
    // U8 review addendum: reconstruction backfills a BEST-EFFORT malformed ingest
    // ONLY. Supplying the four field flags against a WELL-FORMED stop is refused so an
    // agent-authored stop's fields can never be silently rewritten and re-signed.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    // STOP_AGENT files a complete four-field UNKNOWN.md → the stop is NOT malformed.
    let env = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    assert_eq!(env["data"]["malformed"], false);
    let stop_id = env["data"]["stop_id"].as_str().unwrap().to_string();

    let refusal = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "resolve",
                &stop_id,
                "--reject",
                "--rationale",
                "triaged",
                "--what-needed",
                "trying to overwrite a well-formed field",
            ])
            .assert()
            .failure(),
    );
    assert_eq!(refusal["status"], "error");
    assert!(
        refusal["errors"][0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("malformed"),
        "refusal must explain reconstruction is malformed-only: {:?}",
        refusal["errors"]
    );
    // The stop is untouched: still open (the refusal happened before any write).
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let state: String = conn
        .query_row(
            "SELECT state FROM contract_stops WHERE id = ?1",
            [&stop_id],
            |r| r.get(0),
        )
        .expect("stop row");
    assert_eq!(
        state, "open",
        "a refused reconstruction must not mutate the stop"
    );
}

#[test]
fn contract_resolve_bad_kind_is_rejected() {
    // A reconstruction --kind outside the vocabulary is refused before any write.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let env = contract_run(&repo, &["ccx-a"], "echo stuck > UNKNOWN.md", 2);
    let stop_id = env["data"]["stop_id"].as_str().unwrap().to_string();
    repo.forge()
        .args([
            "--json",
            "contract",
            "resolve",
            &stop_id,
            "--reject",
            "--rationale",
            "x",
            "--kind",
            "nonsense",
        ])
        .assert()
        .failure();
}

#[test]
fn contract_resolve_replay_is_idempotent() {
    // R18/KTD6: replaying a resolve request-id returns the recorded result and does
    // NOT create a second bump revision.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let stopped = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = stopped["data"]["stop_id"].as_str().unwrap().to_string();

    let args = [
        "--json",
        "--request-id",
        "resolve-1",
        "contract",
        "resolve",
        &stop_id,
        "--reject",
        "--rationale",
        "covered already",
    ];
    let first = json_output(repo.forge().args(args).assert().success());
    assert_eq!(first["data"]["revision"]["revision"], 2);
    let count_after_first = revision_count(&repo);

    let replay = json_output(repo.forge().args(args).assert().success());
    assert_eq!(replay["status"], "success");
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(
        revision_count(&repo),
        count_after_first,
        "a replayed resolve must not freeze a second bump revision"
    );
}

#[test]
fn contract_show_run_and_contract_round_trip_expected_fields() {
    // R23: `show <run-id>` carries per-task outcomes + a tally counting stops as
    // successes pending triage; `show <contract-id>` carries the frozen revision and
    // blocked/runnable status.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    freeze_contract(&repo, "ccx-b", &["ccx-a"]);

    // A stopped chain: the run tally counts the stop as a success pending triage.
    let stopped = contract_run(&repo, &["ccx-a", "ccx-b"], STOP_AGENT, 2);
    let run_id = stopped["data"]["run_id"].as_str().unwrap().to_string();
    let show_run = json_output(
        repo.forge()
            .args(["--json", "contract", "show", &run_id])
            .assert()
            .success(),
    );
    assert_eq!(show_run["data"]["kind"], "run");
    assert_eq!(show_run["data"]["run"]["outcome"], "stopped");
    assert_eq!(show_run["data"]["tally"]["stopped"], 1);
    assert_eq!(show_run["data"]["tally"]["failed"], 0);
    assert_eq!(show_run["data"]["tally"]["successes_pending_triage"], 1);
    let tasks = show_run["data"]["tasks"].as_array().unwrap();
    assert_eq!(tasks[0]["outcome"], "stopped");
    assert_eq!(tasks[1]["outcome"], "skipped");

    // show <contract-id>: ccx-a is blocked by its own open stop; ccx-b is blocked
    // transitively through its dependency on ccx-a.
    let show_a = json_output(
        repo.forge()
            .args(["--json", "contract", "show", "ccx-a"])
            .assert()
            .success(),
    );
    assert_eq!(show_a["data"]["kind"], "contract");
    assert_eq!(show_a["data"]["revision"]["revision"], 1);
    assert_eq!(show_a["data"]["blocked"], true);
    assert_eq!(show_a["data"]["runnable"], false);
    assert!(!show_a["data"]["blocking_stop_ids"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn contract_show_runnable_contract_reports_no_block() {
    // R23: a frozen contract with no open stop in its closure is runnable.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let show = json_output(
        repo.forge()
            .args(["--json", "contract", "show", "ccx-a"])
            .assert()
            .success(),
    );
    assert_eq!(show["data"]["runnable"], true);
    assert_eq!(show["data"]["blocked"], false);
}

#[test]
fn contract_verdicts_lists_recorded_run_verdicts() {
    // R23: a completed run records per-task blast-pass verdicts; `verdicts <run-id>`
    // round-trips their kind / pass-fail / command shape.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let completed = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    let run_id = completed["data"]["run_id"].as_str().unwrap().to_string();

    let env = json_output(
        repo.forge()
            .args(["--json", "contract", "verdicts", &run_id])
            .assert()
            .success(),
    );
    assert_eq!(env["data"]["run_id"], run_id);
    let verdicts = env["data"]["verdicts"].as_array().expect("verdicts array");
    assert!(
        !verdicts.is_empty(),
        "a completed run records blast verdicts"
    );
    assert_eq!(verdicts[0]["verdict_kind"], "blast");
    assert_eq!(verdicts[0]["passed"], true);
}

// ---------------------------------------------------------------------------
// U9: doctor two-sided coverage + GC reachability at the CLI level
// ---------------------------------------------------------------------------

/// Backdate EVERY loose native object past the GC protection window, so a gc run
/// reclaims unreferenced objects deterministically regardless of the default
/// window (mirrors forge_doctor_gc's `mark_object_old`, applied store-wide).
fn backdate_all_native_objects(repo: &TestRepo) {
    let objects = repo.path().join(".forge/objects/sha256");
    let Ok(prefixes) = std::fs::read_dir(&objects) else {
        return;
    };
    for prefix in prefixes.flatten() {
        let Ok(entries) = std::fs::read_dir(prefix.path()) else {
            continue;
        };
        for object in entries.flatten() {
            let status = std::process::Command::new("touch")
                .args(["-t", "202001010000"])
                .arg(object.path())
                .status()
                .expect("touch");
            assert!(status.success(), "backdate mtime failed");
        }
    }
}

/// Any object file whose bytes contain `needle` (proves a secret tree was or was
/// not reclaimed). Returns true when at least one loose object still contains it.
fn any_object_contains(repo: &TestRepo, needle: &str) -> bool {
    let objects = repo.path().join(".forge/objects/sha256");
    let Ok(prefixes) = std::fs::read_dir(&objects) else {
        return false;
    };
    for prefix in prefixes.flatten() {
        let Ok(entries) = std::fs::read_dir(prefix.path()) else {
            continue;
        };
        for object in entries.flatten() {
            if let Ok(bytes) = std::fs::read(object.path()) {
                if String::from_utf8_lossy(&bytes).contains(needle) {
                    return true;
                }
            }
        }
    }
    false
}

/// `gc --dry-run` envelope `data`.
fn gc_dry_run(repo: &TestRepo) -> Value {
    json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    )["data"]
        .clone()
}

/// `gc --yes --plan-digest <d>` envelope `data` (real deletion).
fn gc_delete(repo: &TestRepo, plan_digest: &str) -> Value {
    json_output(
        repo.forge()
            .args(["--json", "gc", "--yes", "--plan-digest", plan_digest])
            .assert()
            .success(),
    )["data"]
        .clone()
}

fn array_contains_str(value: &Value, needle: &str) -> bool {
    value
        .as_array()
        .map(|items| items.iter().any(|v| v.as_str() == Some(needle)))
        .unwrap_or(false)
}

#[test]
fn doctor_green_over_native_contract_chain() {
    // U9 (R17/KTD2) at the CLI level: a real chain through the binary — freeze policy
    // + contract, a completed run, then a stopped run whose stop is resolved (the
    // mutable stop re-sign path) — leaves `forge doctor` green. Every contract, run,
    // and stop row is signed and its op-log chain link re-verifies.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    let stopped = contract_run(&repo, &["ccx-a"], STOP_AGENT, 2);
    let stop_id = stopped["data"]["stop_id"].as_str().expect("stop id");
    repo.forge()
        .args([
            "--json",
            "contract",
            "resolve",
            stop_id,
            "--reject",
            "--rationale",
            "brief already covers it",
        ])
        .assert()
        .success();

    let report = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(
        report["data"]["ok"], true,
        "doctor must be green over a native contract chain: {:?}",
        report["data"]["issues"]
    );
    assert!(report["data"]["signature_issues"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(report["data"]["tampered_rows"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn gc_retains_referenced_run_patch_object() {
    // KTD3/U9: a completed run's produced-patch tree object is a durable GC root
    // (contract_runs.patch_content_ref), so it is NEVER reclaimed even after being
    // backdated past the protection window. A new ObjectKind that is not a GC root
    // is a data-loss bug; this proves the wiring holds through a real gc.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let completed = contract_run(&repo, &["ccx-a"], EDIT_AGENT, 0);
    let run_id = completed["data"]["run_id"].as_str().unwrap().to_string();

    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    let patch_ref: String = conn
        .query_row(
            "SELECT patch_content_ref FROM contract_runs WHERE id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .expect("patch ref present on a completed run");
    drop(conn);
    let tree_id = patch_ref
        .strip_prefix("forge-tree:")
        .expect("forge-tree ref")
        .to_string();

    backdate_all_native_objects(&repo);
    let dry = gc_dry_run(&repo);
    // The core property: the referenced patch tree is a GC ROOT, so it is never in
    // the unreachable set even when backdated past the protection window.
    assert!(
        !array_contains_str(&dry["unreachable_native_objects"], &tree_id),
        "the referenced patch tree must be reachable (not in the unreachable set)"
    );
    let digest = dry["plan_digest"].as_str().unwrap();
    gc_delete(&repo, digest);
    // After a real gc it is STILL reachable (packing may relocate it loose→pack, but
    // it is never reclaimed) and doctor stays green (no dangling content ref).
    let post = gc_dry_run(&repo);
    assert!(
        !array_contains_str(&post["unreachable_native_objects"], &tree_id),
        "the referenced patch tree must survive gc and stay reachable"
    );
    let report = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(report["data"]["ok"], true);
}

#[test]
fn gc_reclaims_secret_refused_unreferenced_post_tree() {
    // U6 decision + U9 proof: a secret-content-refused run leaves its post-tree
    // UNREFERENCED (the run carries no patch ref), so GC classifies it as unreachable
    // and reclaims its loose plaintext object. The reciprocal of the retained-patch
    // test — a refused tree must not linger as a reachable, secret-bearing root.
    let repo = run_repo();
    freeze_contract(&repo, "ccx-a", &[]);
    let secret = "AKIASEKRETGCMARKER98765";
    let agent = format!("printf 'api_key = \"{secret}\"\\n' > out.txt");
    let env = contract_run(&repo, &["ccx-a"], &agent, 3);
    assert_eq!(env["data"]["outcome"], "blast_violation");
    assert_eq!(env["data"]["secret_content_detected"], true);
    // The snapshot step wrote the offending tree+blob loose before the refusal, so the
    // plaintext secret is present in a loose object at this point (the run just never
    // references it).
    assert!(
        any_object_contains(&repo, "AKIASEKRETGCMARKER"),
        "the refused post-tree's loose object should exist pre-gc"
    );

    backdate_all_native_objects(&repo);
    let dry = gc_dry_run(&repo);
    // Unreferenced ⇒ classified unreachable (collectable). A GC-rooted object would
    // never appear here; this is the U6 not-a-root property.
    assert!(
        !dry["unreachable_native_objects"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the refused post-tree must be unreferenced and collectable"
    );
    let digest = dry["plan_digest"].as_str().unwrap();
    gc_delete(&repo, digest);
    // gc reclaimed the loose plaintext object (packs are zlib-compressed, so a plaintext
    // substring scan of the loose store is the meaningful scrub check).
    assert!(
        !any_object_contains(&repo, "AKIASEKRETGCMARKER"),
        "gc must reclaim the loose plaintext secret object"
    );
    let report = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(report["data"]["ok"], true);
}
