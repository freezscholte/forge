//! NER U3: `forge contract lint | freeze` end-to-end through the CLI.
//!
//! Covers the U3 acceptance slice: AE4 (unknown top-level key errors, no frozen
//! revision) and AE6 (metacharacter / non-cargo acceptance refused at lint with
//! CONTRACT_GRAMMAR_VIOLATION), plus lint-clean freeze reading back as revision
//! 1, wrong-visibility primitive rejection, relative-path canonicalization
//! (R19), and global-policy freeze under the reserved id.

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
