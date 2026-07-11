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
