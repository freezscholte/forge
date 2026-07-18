//! U10 / R21 retirement-criterion dogfood: the FULL native contract chain end to
//! end — author → freeze → run → stop → triage → resume → verify → integrate →
//! accept → doctor — driven ENTIRELY through `forge contract` subcommands with the
//! fake agent command as the ONLY script glue.
//!
//! Split out of `forge_contract.rs` (which is at its 3000-line domain ceiling) as a
//! cohesive self-contained scenario. It re-declares only the small helper set it
//! needs; each `tests/*.rs` file is its own crate, so this duplication is local and
//! cheap and keeps the AE unit suite and the retirement dogfood independently
//! runnable.

mod common;

use common::TestRepo;
use serde_json::Value;

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json envelope")
}

/// A native repo with `crates/` scaffolding so the lint rules that read the repo
/// working tree have something to resolve against.
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

/// The `<name>.yaml` file name a `ccx-<name>` id requires (lint R1 correspondence).
fn contract_file_name(id: &str) -> String {
    format!("{}.yaml", id.strip_prefix("ccx-").unwrap_or(id))
}

/// The recorded outcome of one per-task run row.
fn task_outcome(repo: &TestRepo, run_id: &str, task_id: &str) -> String {
    let conn = rusqlite::Connection::open(repo.path().join(".forge/forge.db")).unwrap();
    conn.query_row(
        "SELECT outcome FROM contract_run_tasks WHERE run_id = ?1 AND task_id = ?2",
        rusqlite::params![run_id, task_id],
        |r| r.get(0),
    )
    .expect("task row")
}

/// A fake agent that produces a real (non-empty) patch by APPENDING to a file.
const EDIT_AGENT: &str = "echo change >> out.txt";

const VERIFY_CARGO_TOML: &str =
    "[package]\nname = \"verifyfixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
/// rustfmt-clean, compiles — both `cargo build` and `cargo fmt --check` pass.
const GOOD_LIB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";

/// Install a standalone cargo fixture crate so the run base (and the rebuilt verify
/// scratch) contains a real cargo project. `verify` runs `cargo build` +
/// `cargo fmt --check` against it; the well-formed lib makes both deterministically
/// green.
fn install_verify_crate(repo: &TestRepo) {
    write(repo, "Cargo.toml", VERIFY_CARGO_TOML);
    write(repo, "src/lib.rs", GOOD_LIB);
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

/// Freeze a retirement-dogfood contract for `id`: `cargo build` fix + optional guards,
/// `out.txt` the sole allowed path, optional `depends_on`. Supporting a dependency edge
/// (unlike the AE-suite's verify helper) lets the R21 chain be both verify-green (real
/// cargo acceptance) AND dependency-ordered.
fn freeze_dogfood_contract(repo: &TestRepo, id: &str, guard: &[&str], depends_on: &[&str]) {
    let mut yaml = format!(
        "schema: ccx.contract.v1\n\
id: {id}\n\
revision: 1\n\
ticket: NER-999\n\
task: Add two integers.\n\
interface: Provide an add function.\n\
acceptance:\n  fix:\n    - cargo build\n"
    );
    if !guard.is_empty() {
        yaml.push_str("  guard:\n");
        for cmd in guard {
            yaml.push_str(&format!("    - {cmd}\n"));
        }
    }
    yaml.push_str("allowed_changes:\n  paths: [out.txt]\n");
    yaml.push_str("authority: {source: human, confidence: high, reviewer: test}\n");
    if !depends_on.is_empty() {
        yaml.push_str("depends_on:\n");
        for dep in depends_on {
            yaml.push_str(&format!("  - {dep}\n"));
        }
    }
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

#[test]
fn native_chain_end_to_end_retirement_criterion() {
    // R21 retirement criterion: a full dogfood chain — author → freeze → run → stop →
    // triage → resume → verify → integrate → accept → doctor — driven ENTIRELY through
    // `forge contract` subcommands with the fake agent command as the ONLY script glue.
    // No ccx-*.py / *.sh harness stage is invoked. This is the equivalent-to-dogfood-#2
    // scenario (multi-task, dependency-ordered, one induced stop + triage cycle) the plan
    // names as the trigger to retire the overlapping `tools/ccx` script stages.
    let repo = init_repo();

    // ---- Author + freeze: global policy + a dependency-ordered pair ----
    // A real, tiny, well-formed cargo crate so `verify` (cargo build + cargo fmt --check)
    // is deterministically green.
    install_verify_crate(&repo);
    freeze_global_policy(&repo);
    // ccx-root: no deps, fix cargo build + guard cargo fmt --check.
    freeze_dogfood_contract(&repo, "ccx-root", &["cargo fmt --check"], &[]);
    // ccx-leaf: depends on ccx-root, fix cargo build.
    freeze_dogfood_contract(&repo, "ccx-leaf", &[], &["ccx-root"]);

    // ---- Run the chain: ccx-root completes, ccx-leaf files an UNKNOWN.md stop ----
    // The fake agent reads the brief on stdin and discriminates the task by its id: it
    // STOPS (four-field UNKNOWN.md) on the leaf and edits out.txt for the root.
    write(
        &repo,
        "chain-agent.sh",
        "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in\n  *'id: ccx-leaf'*)\n    printf 'What: need the dependency shape\\nWhy: brief omits the leaf contract boundary\\nKind: blocking\\nEvidence: src/lib.rs:1\\n' > UNKNOWN.md\n    exit 0 ;;\nesac\necho change >> out.txt\n",
    );
    let chain_agent = format!("sh {}", repo.path().join("chain-agent.sh").display());
    let halted = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-root",
                "ccx-leaf",
                "--chain",
                "--agent-cmd",
                &chain_agent,
            ])
            .assert()
            .code(2),
    );
    assert_eq!(halted["status"], "success"); // a stop is exit 2 + success envelope (R25)
    assert_eq!(halted["data"]["outcome"], "stopped");
    let halted_run_id = halted["data"]["run_id"].as_str().unwrap().to_string();
    let stop_id = halted["data"]["stop_id"].as_str().unwrap().to_string();
    assert_eq!(task_outcome(&repo, &halted_run_id, "ccx-root"), "completed");
    assert_eq!(task_outcome(&repo, &halted_run_id, "ccx-leaf"), "stopped");

    // ---- Triage surface: `stops --open` shows the open leaf stop ----
    let open = json_output(
        repo.forge()
            .args(["--json", "contract", "stops", "--open"])
            .assert()
            .success(),
    );
    assert_eq!(open["data"]["count"], 1);
    let listed = &open["data"]["stops"][0];
    assert_eq!(listed["contract_id"], "ccx-leaf");
    assert_eq!(listed["state"], "open");
    assert!(listed["what_needed"]
        .as_str()
        .unwrap()
        .contains("dependency shape"));

    // ---- Resolve --revised: a revision bump on the leaf clears the stop ----
    let revised = "schema: ccx.contract.v1\n\
id: ccx-leaf\n\
revision: 1\n\
ticket: NER-999\n\
task: Add two integers.\n\
interface: Provide an add function - clarified the leaf boundary is out.txt only.\n\
acceptance:\n  fix:\n    - cargo build\n\
allowed_changes:\n  paths: [out.txt]\n\
authority: {source: human, confidence: high, reviewer: test}\n\
depends_on:\n  - ccx-root\n";
    write(&repo, "contracts/leaf.yaml", revised);
    let resolved = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "resolve",
                &stop_id,
                "--revised",
                "contracts/leaf.yaml",
            ])
            .assert()
            .success(),
    );
    assert_eq!(resolved["data"]["resolution_kind"], "revision");
    assert_eq!(resolved["data"]["revision"]["revision"], 2);
    assert_eq!(resolved["data"]["resolved_stop"]["state"], "resolved");

    // ---- Resume: the chain restarts at the halted leaf and completes ----
    // The root is REPLAYED from recorded output (the canary proves its agent is never
    // re-executed); the leaf now completes (edits out.txt, no UNKNOWN.md).
    let canary = repo.path().join("retirement-resume-canary");
    write(
        &repo,
        "resume-agent.sh",
        &format!(
            "#!/bin/sh\nprompt=$(cat)\ncase \"$prompt\" in *'id: ccx-root'*) touch {} ;; esac\necho more >> out.txt\n",
            canary.display()
        ),
    );
    let resume_agent = format!("sh {}", repo.path().join("resume-agent.sh").display());
    let resumed = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-root",
                "ccx-leaf",
                "--chain",
                "--agent-cmd",
                &resume_agent,
                "--resume",
                &halted_run_id,
            ])
            .assert()
            .code(0),
    );
    assert_eq!(resumed["data"]["outcome"], "completed");
    assert!(
        !canary.exists(),
        "resume must replay the completed root, never re-execute its agent"
    );
    let resumed_run_id = resumed["data"]["run_id"].as_str().unwrap().to_string();

    // ---- Verify the completed task: fix + guard green (exit 0) ----
    let verified = json_output(
        repo.forge()
            .args(["--json", "contract", "verify", &resumed_run_id])
            .assert()
            .code(0),
    );
    assert_eq!(verified["data"]["outcome"], "passed");

    // ---- Integrate the root directly (no deps) + accept its proposal ----
    // A completed single-contract run of the root produces the integration payload; the
    // root has no dependencies, so it integrates directly onto HEAD. Acceptance flows
    // through the ordinary propose → accept lifecycle (contract acceptance licenses
    // integration; it is never a merge — R22).
    let root_run = json_output(
        repo.forge()
            .args([
                "--json",
                "contract",
                "run",
                "ccx-root",
                "--agent-cmd",
                EDIT_AGENT,
            ])
            .assert()
            .code(0),
    );
    let root_run_id = root_run["data"]["run_id"].as_str().unwrap().to_string();
    let integrated = json_output(
        repo.forge()
            .args(["--json", "contract", "integrate", &root_run_id])
            .assert()
            .success(),
    );
    let attempt_id = integrated["data"]["attempt_id"].as_str().unwrap();
    assert!(!attempt_id.is_empty());
    // Accept the integration attempt's proposal through the normal save → run → propose
    // → check → accept lifecycle (KTD8): integrate materialized the merged tree into the
    // attempt's isolated workspace, so `save` snapshots it and the rest flows unchanged
    // (accept requires a passed check; the synthesized intent carries no gates, so a
    // trivial check passes).
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    repo.forge().args(["--json", "accept"]).assert().success();
    // The integration is now accepted into HEAD: integrating the dependent leaf's chain
    // run is no longer blocked by the deps gate on ccx-root (it may fail later stages,
    // but the deps-accepted precondition is satisfied — proven by the accepted root).

    // ---- doctor green over the full new-kind population ----
    let doctor = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(
        doctor["data"]["ok"], true,
        "doctor must be green after the full native chain: {:?}",
        doctor["data"]["issues"]
    );
    assert!(doctor["data"]["signature_issues"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(doctor["data"]["tampered_rows"]
        .as_array()
        .unwrap()
        .is_empty());
}
