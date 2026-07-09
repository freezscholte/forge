mod common;

use common::{forge_in, TestRepo};
use rusqlite::Connection;
use serde_json::Value;

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

fn clear_attached_attempt(repo: &TestRepo) {
    let connection = Connection::open(repo.path().join(".forge/forge.db")).expect("open db");
    connection
        .execute("UPDATE current_state SET attached_attempt_id = NULL", [])
        .expect("clear attachment");
}

fn git(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn start_attaches_created_attempt() {
    // The "migrate an existing DB" half of this test used to DELETE the version-2
    // row while leaving its inline column in place — an artificial one-column state
    // that the version-gated migration runner (NER-133 U3) would try to re-ALTER
    // and fail with "duplicate column name". Genuine v1->v2 upgrade convergence is
    // now covered by `forge-store`'s `migrations::tests` (genesis case B), so this
    // test focuses on the CLI attach behavior against a normal at-HEAD DB.
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();

    let started = json_output(
        repo.forge()
            .args(["--json", "start", "first attempt"])
            .assert()
            .success(),
    );
    let attempts = json_output(
        repo.forge()
            .args(["--json", "attempt", "list"])
            .assert()
            .success(),
    );

    assert_eq!(started["data"]["attached"], true);
    assert_eq!(attempts["data"]["attempts"][0]["attached"], true);
    assert_eq!(
        attempts["data"]["attempts"][0]["attempt_id"],
        started["data"]["attempt_id"]
    );
}

#[test]
fn attempt_start_lists_and_shows_competing_attempts() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );

    assert_eq!(second["data"]["intent_id"], intent_id);
    let listed = json_output(
        repo.forge()
            .args(["--json", "attempt", "list"])
            .assert()
            .success(),
    );
    assert_eq!(listed["data"]["attempts"].as_array().unwrap().len(), 2);
    // NER-382: list/show carry the workspace_role qualifier next to workspace_path.
    assert_eq!(
        listed["data"]["attempts"][0]["workspace_role"],
        "materialization_target"
    );

    let shown = json_output(
        repo.forge()
            .args([
                "--json",
                "attempt",
                "show",
                second["data"]["attempt_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(shown["data"]["attempt"]["intent_id"], intent_id);
    assert_eq!(
        shown["data"]["attempt"]["workspace_role"],
        "materialization_target"
    );
    assert!(shown["data"]["proposals"].as_array().unwrap().is_empty());
}

#[test]
fn ambiguous_attempt_requires_explicit_selector() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    repo.forge()
        .args(["--json", "attempt", "start", "--intent", intent_id])
        .assert()
        .success();
    clear_attached_attempt(&repo);

    std::fs::write(repo.path().join("README.md"), "ambiguous\n").expect("write readme");
    let output = json_output(repo.forge().args(["--json", "save"]).assert().failure());
    assert_eq!(output["errors"][0]["code"], "AMBIGUOUS_ATTEMPT");

    let saved = json_output(
        repo.forge()
            .args([
                "--json",
                "save",
                "--attempt",
                first["data"]["attempt_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(saved["data"]["attempt_id"], first["data"]["attempt_id"]);

    let shown = json_output(repo.forge().args(["--json", "show"]).assert().failure());
    assert_eq!(shown["errors"][0]["code"], "AMBIGUOUS_ATTEMPT");
}

#[test]
fn unknown_attempt_selector_is_typed() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    repo.forge()
        .args(["--json", "start", "known"])
        .assert()
        .success();

    let output = json_output(
        repo.forge()
            .args(["--json", "save", "--attempt", "attempt_missing"])
            .assert()
            .failure(),
    );
    assert_eq!(output["errors"][0]["code"], "UNKNOWN_ATTEMPT");
}

#[test]
fn attach_materializes_snapshot_and_refuses_dirty_work() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_attempt = first["data"]["attempt_id"].as_str().unwrap();
    std::fs::write(repo.path().join("README.md"), "attempt one\n").expect("write first");
    repo.forge().args(["--json", "save"]).assert().success();

    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n"
    );

    std::fs::write(repo.path().join("README.md"), "unsaved\n").expect("write dirty");
    let dirty = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", first_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(dirty["errors"][0]["code"], "DIRTY_WORKTREE");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "unsaved\n"
    );
}

#[test]
fn attach_materializes_native_base_via_forge_tree_ref() {
    // NER-138 Phase 7 slice 2: a native repo's base_head is an f1:commit: id (not a git
    // SHA). Attaching a fresh competing attempt materializes its base through
    // base_content_ref -> forge-tree: -> the native backend (prefix dispatch), with no git
    // — proving native base anchoring round-trips through `attempt attach`.
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    assert!(
        first["data"]["base_head"]
            .as_str()
            .unwrap()
            .starts_with("f1:commit:sha256:"),
        "native base_head must be a commit id: {}",
        first["data"]["base_head"]
    );
    // Modify + save under the first attempt; the genesis base remains README = "hello".
    std::fs::write(repo.path().join("README.md"), "attempt one\n").expect("write first");
    repo.forge().args(["--json", "save"]).assert().success();

    // A competing attempt under the same intent; attaching it (no snapshot yet)
    // materializes the native BASE tree via the forge-tree: ref.
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n",
        "attach must materialize the native base via the forge-tree: ref"
    );
}

#[test]
fn native_attempts_surface_and_materialize_workspace_paths() {
    let repo = TestRepo::new_git();
    std::fs::write(repo.path().join(".env"), "TOKEN=committed\n").expect("write env");
    git(repo.path(), &["add", ".env"]);
    git(repo.path(), &["commit", "-m", "track env"]);

    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    let started = json_output(
        repo.forge()
            .args(["--json", "start", "workspace"])
            .assert()
            .success(),
    );
    let workspace_path = started["data"]["workspace_path"].as_str().unwrap();
    assert!(workspace_path.starts_with(".forge/worktrees/"));
    assert_eq!(
        started["data"]["workspace_role"], "materialization_target",
        "start must qualify workspace_path as a materialization target (NER-382)"
    );
    let workspace = repo.path().join(workspace_path);
    assert!(workspace
        .join(forge_content::WORKSPACE_MARKER_FILE)
        .exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(
        !workspace.join(".env").exists(),
        "secret-risk paths must not materialize into attempt workspaces"
    );

    let listed = json_output(
        repo.forge()
            .args(["--json", "attempt", "list"])
            .assert()
            .success(),
    );
    assert_eq!(
        listed["data"]["attempts"][0]["workspace_path"],
        started["data"]["workspace_path"]
    );
    let shown = json_output(
        repo.forge()
            .args([
                "--json",
                "attempt",
                "show",
                started["data"]["attempt_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(
        shown["data"]["attempt"]["workspace_path"],
        started["data"]["workspace_path"]
    );
}

/// NER-382: `attempt start` must qualify `workspace_path` with
/// `workspace_role: "materialization_target"` in both the fresh payload and the
/// idempotent `--request-id` replay (which is rebuilt from the stored
/// `replay_data`, a separate emission site from the fresh `StartAttempt`).
#[test]
fn attempt_start_payload_and_replay_carry_workspace_role() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let started = json_output(
        repo.forge()
            .args(["--json", "start", "roles"])
            .assert()
            .success(),
    );
    let intent_id = started["data"]["intent_id"].as_str().unwrap();

    let second = json_output(
        repo.forge()
            .args([
                "--json",
                "--request-id",
                "attempt-start-once",
                "attempt",
                "start",
                "--intent",
                intent_id,
            ])
            .assert()
            .success(),
    );
    assert_eq!(second["data"]["workspace_role"], "materialization_target");

    let replay = json_output(
        repo.forge()
            .args([
                "--json",
                "--request-id",
                "attempt-start-once",
                "attempt",
                "start",
                "--intent",
                intent_id,
            ])
            .assert()
            .success(),
    );
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(replay["data"]["attempt_id"], second["data"]["attempt_id"]);
    assert_eq!(
        replay["data"]["workspace_path"],
        second["data"]["workspace_path"]
    );
    assert_eq!(replay["data"]["workspace_role"], "materialization_target");
}

/// NER-382 replay gap: an `attempt_started` op recorded BEFORE the workspace_role
/// upgrade carries `workspace_path` in its stored `replay_data` but no
/// `workspace_role`. The replay rebuild must inject the constant so the replayed
/// payload still matches the schema promise. Simulated by stripping the key from
/// the stored view state, exactly the shape a pre-upgrade row has.
#[test]
fn attempt_start_replay_injects_workspace_role_for_pre_upgrade_rows() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let started = json_output(
        repo.forge()
            .args(["--json", "start", "pre-upgrade replay"])
            .assert()
            .success(),
    );
    let intent_id = started["data"]["intent_id"].as_str().unwrap();
    let second = json_output(
        repo.forge()
            .args([
                "--json",
                "--request-id",
                "pre-upgrade-once",
                "attempt",
                "start",
                "--intent",
                intent_id,
            ])
            .assert()
            .success(),
    );

    let connection = Connection::open(repo.path().join(".forge/forge.db")).expect("open db");
    let stripped = connection
        .execute(
            "UPDATE views
             SET state_json = json_remove(state_json, '$.replay_data.workspace_role')
             WHERE json_extract(state_json, '$.replay_data.workspace_role') IS NOT NULL",
            [],
        )
        .expect("strip workspace_role from stored replay_data");
    assert!(stripped > 0, "fixture must have stripped at least one row");

    let replay = json_output(
        repo.forge()
            .args([
                "--json",
                "--request-id",
                "pre-upgrade-once",
                "attempt",
                "start",
                "--intent",
                intent_id,
            ])
            .assert()
            .success(),
    );
    assert_eq!(replay["data"]["idempotent_replay"], true);
    assert_eq!(replay["data"]["attempt_id"], second["data"]["attempt_id"]);
    assert!(replay["data"]["workspace_path"].is_string());
    assert_eq!(replay["data"]["workspace_role"], "materialization_target");
}

#[test]
fn native_attempt_workspaces_are_isolated_and_bind_saves() {
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "parallel"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_workspace = repo
        .path()
        .join(first["data"]["workspace_path"].as_str().unwrap());

    std::fs::write(first_workspace.join("README.md"), "attempt one\n").expect("write first");
    let first_save = json_output(
        forge_in(&first_workspace)
            .args(["--json", "save"])
            .assert()
            .success(),
    );
    assert_eq!(
        first_save["data"]["attempt_id"],
        first["data"]["attempt_id"]
    );

    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_workspace = repo
        .path()
        .join(second["data"]["workspace_path"].as_str().unwrap());

    assert_eq!(
        std::fs::read_to_string(first_workspace.join("README.md")).unwrap(),
        "attempt one\n"
    );
    assert_eq!(
        std::fs::read_to_string(second_workspace.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n",
        "workspace saves must not mutate the repo-root checkout"
    );

    std::fs::write(second_workspace.join("README.md"), "attempt two\n").expect("write second");
    let second_save = json_output(
        forge_in(&second_workspace)
            .args(["--json", "save"])
            .assert()
            .success(),
    );
    assert_eq!(
        second_save["data"]["attempt_id"],
        second["data"]["attempt_id"]
    );
    assert_eq!(
        std::fs::read_to_string(first_workspace.join("README.md")).unwrap(),
        "attempt one\n"
    );
}

#[test]
fn workspace_save_does_not_poison_repo_root_dirty_baseline() {
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "parallel merge"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_attempt = first["data"]["attempt_id"].as_str().unwrap();
    let first_workspace = repo
        .path()
        .join(first["data"]["workspace_path"].as_str().unwrap());
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    let second_workspace = repo
        .path()
        .join(second["data"]["workspace_path"].as_str().unwrap());

    std::fs::write(first_workspace.join("README.md"), "attempt one\n").expect("write first");
    forge_in(&first_workspace)
        .args(["--json", "save"])
        .assert()
        .success();
    forge_in(&first_workspace)
        .args(["--json", "run", "--", "true"])
        .assert()
        .success();
    let first_proposal = json_output(
        forge_in(&first_workspace)
            .args(["--json", "propose"])
            .assert()
            .success(),
    );

    std::fs::write(second_workspace.join("OTHER.md"), "attempt two\n").expect("write second");
    forge_in(&second_workspace)
        .args(["--json", "save"])
        .assert()
        .success();
    forge_in(&second_workspace)
        .args(["--json", "run", "--", "true"])
        .assert()
        .success();
    let second_proposal = json_output(
        forge_in(&second_workspace)
            .args(["--json", "propose"])
            .assert()
            .success(),
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n",
        "workspace saves must leave the repo root at its own baseline"
    );

    repo.forge()
        .args([
            "--json",
            "check",
            "--attempt",
            first_attempt,
            "--proposal",
            first_proposal["data"]["proposal_id"].as_str().unwrap(),
        ])
        .assert()
        .success();
    repo.forge()
        .args([
            "--json",
            "accept",
            "--attempt",
            first_attempt,
            "--proposal",
            first_proposal["data"]["proposal_id"].as_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n",
        "accept records the decision but does not materialize into the repo root"
    );

    let merged = json_output(
        repo.forge()
            .args([
                "--json",
                "merge",
                "--proposal",
                second_proposal["data"]["proposal_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(merged["data"]["merged"], true);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "attempt one\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("OTHER.md")).unwrap(),
        "attempt two\n"
    );
    assert_eq!(
        second_attempt,
        second_proposal["data"]["attempt_id"].as_str().unwrap()
    );
}

#[test]
fn attach_base_revision_preserves_tracked_secret_risk_paths() {
    let repo = TestRepo::new_git();
    std::fs::write(repo.path().join(".env"), "TOKEN=committed\n").expect("write env");
    git(repo.path(), &["add", ".env"]);
    git(repo.path(), &["commit", "-m", "track env"]);

    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "secrets"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    std::fs::write(repo.path().join("README.md"), "saved\n").expect("write readme");
    repo.forge().args(["--json", "save"]).assert().success();
    std::fs::write(repo.path().join(".env"), "TOKEN=local\n").expect("write local env");

    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    repo.forge()
        .args([
            "--json",
            "attempt",
            "attach",
            second["data"]["attempt_id"].as_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(
        std::fs::read_to_string(repo.path().join(".env")).unwrap(),
        "TOKEN=local\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n"
    );
}

#[test]
fn ambiguous_proposal_requires_explicit_selector() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    repo.forge()
        .args(["--json", "start", "choose proposal"])
        .assert()
        .success();
    std::fs::write(repo.path().join("README.md"), "proposal one\n").expect("write one");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge().args(["--json", "propose"]).assert().success();
    std::fs::write(repo.path().join("README.md"), "proposal two\n").expect("write two");
    repo.forge().args(["--json", "save"]).assert().success();
    // A passing command on proposal two's snapshot so the evidence gate lets the
    // explicit `accept --proposal <two>` proceed (NER-135).
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    let second = json_output(repo.forge().args(["--json", "propose"]).assert().success());

    let ambiguous = json_output(repo.forge().args(["--json", "accept"]).assert().failure());
    assert_eq!(ambiguous["errors"][0]["code"], "AMBIGUOUS_PROPOSAL");

    let accepted = json_output(
        repo.forge()
            .args([
                "--json",
                "accept",
                "--proposal",
                second["data"]["proposal_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert_eq!(
        accepted["data"]["proposal_id"],
        second["data"]["proposal_id"]
    );
    assert_eq!(
        accepted["data"]["proposal_revision_id"],
        second["data"]["proposal_revision_id"]
    );
}

#[test]
fn competing_attempt_loop_exports_selected_proposal() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_attempt = first["data"]["attempt_id"].as_str().unwrap();

    std::fs::write(repo.path().join("README.md"), "attempt one\n").expect("write first");
    repo.forge()
        .args(["--json", "save", "--attempt", first_attempt])
        .assert()
        .success();
    repo.forge()
        .args([
            "--json",
            "run",
            "--attempt",
            first_attempt,
            "--",
            "sh",
            "-c",
            "true",
        ])
        .assert()
        .success();
    let first_proposal = json_output(
        repo.forge()
            .args(["--json", "propose", "--attempt", first_attempt])
            .assert()
            .success(),
    );

    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
    std::fs::write(repo.path().join("README.md"), "attempt two\n").expect("write second");
    repo.forge()
        .args(["--json", "save", "--attempt", second_attempt])
        .assert()
        .success();
    repo.forge()
        .args([
            "--json",
            "run",
            "--attempt",
            second_attempt,
            "--",
            "sh",
            "-c",
            "true",
        ])
        .assert()
        .success();
    let second_proposal = json_output(
        repo.forge()
            .args(["--json", "propose", "--attempt", second_attempt])
            .assert()
            .success(),
    );

    let proposals = json_output(
        repo.forge()
            .args(["--json", "proposal", "list", "--attempt", second_attempt])
            .assert()
            .success(),
    );
    assert_eq!(proposals["data"]["proposals"].as_array().unwrap().len(), 1);
    assert_eq!(
        proposals["data"]["proposals"][0]["proposal_id"],
        second_proposal["data"]["proposal_id"]
    );

    repo.forge()
        .args([
            "--json",
            "check",
            "--attempt",
            second_attempt,
            "--proposal",
            second_proposal["data"]["proposal_id"].as_str().unwrap(),
        ])
        .assert()
        .success();
    repo.forge()
        .args([
            "--json",
            "accept",
            "--attempt",
            second_attempt,
            "--proposal",
            second_proposal["data"]["proposal_id"].as_str().unwrap(),
        ])
        .assert()
        .success();
    repo.forge()
        .args([
            "--json",
            "export",
            "branch",
            "--attempt",
            second_attempt,
            "--proposal",
            second_proposal["data"]["proposal_id"].as_str().unwrap(),
            "forge/selected-attempt",
        ])
        .assert()
        .success();

    assert_eq!(
        git(repo.path(), &["show", "forge/selected-attempt:README.md"]),
        "attempt two\n"
    );
    assert_ne!(
        first_proposal["data"]["proposal_id"],
        second_proposal["data"]["proposal_id"]
    );
}

#[test]
fn save_records_target_attempt_not_materialized_attempt() {
    // NER-134 exit criterion: `save --attempt X` must NOT record a different attempt's
    // content when X is not the attempt the worktree is materialized for. Reproduces the
    // footgun directly — `attempt start` neither materializes nor attaches, so after
    // creating A2 the worktree still holds A1's content and `attached_attempt_id` still
    // points at A1.
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_attempt = first["data"]["attempt_id"].as_str().unwrap();

    std::fs::write(repo.path().join("README.md"), "attempt one\n").expect("write first");
    repo.forge()
        .args(["--json", "save", "--attempt", first_attempt])
        .assert()
        .success();

    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();

    // Worktree still holds A1's content and attached == A1. Saving to A2 must be refused
    // with the typed mismatch error, carrying both opaque ids and a non-retryable verdict.
    let mismatch = json_output(
        repo.forge()
            .args(["--json", "save", "--attempt", second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(mismatch["errors"][0]["code"], "ATTEMPT_WORKTREE_MISMATCH");
    assert_eq!(
        mismatch["errors"][0]["details"]["requested_attempt"],
        second_attempt
    );
    assert_eq!(
        mismatch["errors"][0]["details"]["attached_attempt"],
        first_attempt
    );
    assert_eq!(mismatch["retry"]["retryable"], false);

    // Nothing was recorded under A2.
    let shown_a2 = json_output(
        repo.forge()
            .args(["--json", "attempt", "show", second_attempt])
            .assert()
            .success(),
    );
    assert!(
        shown_a2["data"]["latest_snapshot"].is_null(),
        "no snapshot may exist for A2 after the refused save"
    );

    // The fix is to attach A2 first (re-materializes its base, re-binds the worktree);
    // then `save --attempt A2` records A2's own content.
    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
    std::fs::write(repo.path().join("README.md"), "attempt two\n").expect("write second");
    let saved = json_output(
        repo.forge()
            .args(["--json", "save", "--attempt", second_attempt])
            .assert()
            .success(),
    );
    assert_eq!(saved["data"]["attempt_id"], second_attempt);
    let shown_after = json_output(
        repo.forge()
            .args(["--json", "attempt", "show", second_attempt])
            .assert()
            .success(),
    );
    assert!(
        !shown_after["data"]["latest_snapshot"].is_null(),
        "A2 now has its own snapshot"
    );
}

#[test]
fn restore_rejects_cross_attempt_snapshot() {
    // NER-134 Piece 1b: `restore <snapshot>` must refuse a snapshot owned by an attempt
    // other than the one the worktree is bound to — otherwise restore is a second
    // cross-attempt contamination vector (it would clobber the worktree with another
    // attempt's content while `attached_attempt_id` stays put).
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "compete"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let first_attempt = first["data"]["attempt_id"].as_str().unwrap();

    std::fs::write(repo.path().join("README.md"), "attempt one\n").expect("write first");
    let snap_one = json_output(
        repo.forge()
            .args(["--json", "save", "--attempt", first_attempt])
            .assert()
            .success(),
    );
    let snap_one_id = snap_one["data"]["snapshot_id"].as_str().unwrap();

    // Create + attach A2, then snapshot it so it has its own snapshot.
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
    std::fs::write(repo.path().join("README.md"), "attempt two\n").expect("write two");
    let snap_two = json_output(
        repo.forge()
            .args(["--json", "save", "--attempt", second_attempt])
            .assert()
            .success(),
    );
    let snap_two_id = snap_two["data"]["snapshot_id"].as_str().unwrap();

    // Worktree is bound to A2 and clean vs A2's latest. Restoring A1's snapshot must be
    // refused with the typed mismatch error and must leave the worktree untouched.
    let mismatch = json_output(
        repo.forge()
            .args(["--json", "restore", snap_one_id, "--yes"])
            .assert()
            .failure(),
    );
    assert_eq!(mismatch["errors"][0]["code"], "ATTEMPT_WORKTREE_MISMATCH");
    assert_eq!(
        mismatch["errors"][0]["details"]["requested_attempt"],
        first_attempt
    );
    assert_eq!(
        mismatch["errors"][0]["details"]["attached_attempt"],
        second_attempt
    );
    // The mismatch is deterministic on both the save and restore surfaces — pin the
    // contract so a refactor can't silently flip restore's classification (review T1).
    assert_eq!(mismatch["retry"]["retryable"], false);
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "attempt two\n",
        "a refused restore must not clobber the worktree"
    );

    // Restoring A2's OWN snapshot is still allowed (worktree matches A2's latest).
    repo.forge()
        .args(["--json", "restore", snap_two_id, "--yes"])
        .assert()
        .success();
}

/// NER-382 drift-guard fixture: a native repo with a competing second attempt whose
/// workspace dir was materialized (so `attempt_workspaces.materialized_content_ref`
/// records a `forge-tree:` ref) but which is not attached yet. Returns the repo, the
/// second attempt's id, and its workspace dir under `.forge/worktrees/`.
fn native_repo_with_competing_attempt() -> (TestRepo, String, std::path::PathBuf) {
    native_repo_with_competing_attempt_prepared(|_| {})
}

/// Same fixture with a `prepare` hook run after `init` but before `start`, so a test
/// can add repo-root files (a `.gitignore`, a symlink, …) that the genesis snapshot —
/// and therefore the materialized workspace tree — will contain.
fn native_repo_with_competing_attempt_prepared(
    prepare: impl FnOnce(&TestRepo),
) -> (TestRepo, String, std::path::PathBuf) {
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    prepare(&repo);
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "drift guard"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap().to_string();
    let workspace = repo
        .path()
        .join(second["data"]["workspace_path"].as_str().unwrap());
    (repo, second_attempt, workspace)
}

#[test]
fn attach_refuses_drifted_attempt_workspace() {
    // NER-382 silent-loss repro: edits made inside .forge/worktrees/<attempt2>/ before
    // `attempt attach` used to be clobbered without a trace by re-materialization.
    // Attach must now fail loudly with the typed WORKSPACE_DRIFT error listing the
    // drifted paths, and the refusal must leave both the workspace dir and the repo
    // root untouched (refusal happens BEFORE any materialization write).
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    std::fs::write(workspace.join("README.md"), "drifted edit\n").expect("drift edit");
    std::fs::write(workspace.join("EXTRA.md"), "added in workspace\n").expect("drift add");

    let drift = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(drift["errors"][0]["code"], "WORKSPACE_DRIFT");
    assert_eq!(drift["retry"]["retryable"], false);
    let paths: Vec<&str> = drift["errors"][0]["details"]["paths"]
        .as_array()
        .expect("drift details carry a paths list")
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"README.md"),
        "modified workspace file must be listed as drifted: {paths:?}"
    );
    assert!(
        paths.contains(&"EXTRA.md"),
        "file added to the workspace must be listed as drifted: {paths:?}"
    );

    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "drifted edit\n",
        "a refused attach must not touch the drifted workspace"
    );
    assert!(workspace.join("EXTRA.md").exists());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n",
        "a refused attach must not touch the repo-root worktree"
    );
}

#[test]
fn attach_discard_workspace_changes_discards_drift() {
    // NER-382: the documented escape hatch. On the same drifted state,
    // `attempt attach --discard-workspace-changes` succeeds and the drifted
    // workspace content is re-materialized away.
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    std::fs::write(workspace.join("README.md"), "drifted edit\n").expect("drift edit");
    std::fs::write(workspace.join("EXTRA.md"), "added in workspace\n").expect("drift add");

    let attached = json_output(
        repo.forge()
            .args([
                "--json",
                "attempt",
                "attach",
                &second_attempt,
                "--discard-workspace-changes",
            ])
            .assert()
            .success(),
    );
    assert_eq!(attached["data"]["attempt_id"], second_attempt.as_str());

    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "hello\n",
        "the drifted edit is discarded by re-materialization"
    );
    assert!(
        !workspace.join("EXTRA.md").exists(),
        "a file added to the workspace is discarded by re-materialization"
    );
}

#[test]
fn attach_without_workspace_drift_behaves_as_before() {
    // NER-382 invariant: attach with NO drift is unchanged — success, no new
    // warnings, and the target's base materialized into the repo root as always.
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();

    let attached = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .success(),
    );
    assert_eq!(attached["data"]["attempt_id"], second_attempt.as_str());
    assert!(
        attached["warnings"].as_array().unwrap().is_empty(),
        "a drift-free attach must not emit new warnings: {}",
        attached["warnings"]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "hello\n"
    );
}

#[test]
fn attach_skips_drift_check_when_workspace_never_materialized() {
    // NER-382: with a NULL/absent materialized_content_ref there is no baseline to
    // drift from, so attach proceeds without the check. The git backend is the
    // natural such state: its workspace dirs hold only the marker file and never
    // record a materialized ref, so even stray content inside the dir is not drift.
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();
    let first = json_output(
        repo.forge()
            .args(["--json", "start", "no baseline"])
            .assert()
            .success(),
    );
    let intent_id = first["data"]["intent_id"].as_str().unwrap();
    let second = json_output(
        repo.forge()
            .args(["--json", "attempt", "start", "--intent", intent_id])
            .assert()
            .success(),
    );
    let second_attempt = second["data"]["attempt_id"].as_str().unwrap();
    let workspace = repo
        .path()
        .join(second["data"]["workspace_path"].as_str().unwrap());
    assert!(
        workspace
            .join(forge_content::WORKSPACE_MARKER_FILE)
            .exists(),
        "the workspace dir exists (marker only) but was never materialized"
    );
    std::fs::write(workspace.join("STRAY.md"), "not a baseline\n").expect("write stray");

    repo.forge()
        .args(["--json", "attempt", "attach", second_attempt])
        .assert()
        .success();
}

#[test]
fn attach_discard_workspace_changes_never_bypasses_dirty_worktree() {
    // NER-382 review: --discard-workspace-changes discards workspace-DIR drift
    // ONLY. With a dirty repo-root worktree, the attach must still refuse with
    // DIRTY_WORKTREE — not WORKSPACE_DRIFT, and never success — even when the
    // workspace dir has also drifted and the override flag is passed.
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    std::fs::write(workspace.join("README.md"), "drifted edit\n").expect("drift edit");
    std::fs::write(repo.path().join("README.md"), "dirtied root\n").expect("dirty root");

    let dirty = json_output(
        repo.forge()
            .args([
                "--json",
                "attempt",
                "attach",
                &second_attempt,
                "--discard-workspace-changes",
            ])
            .assert()
            .failure(),
    );
    assert_eq!(dirty["errors"][0]["code"], "DIRTY_WORKTREE");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("README.md")).unwrap(),
        "dirtied root\n",
        "the refused attach must not touch the dirty repo-root worktree"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("README.md")).unwrap(),
        "drifted edit\n",
        "the refused attach must not discard the workspace drift either"
    );
}

#[test]
fn attach_reports_deleted_workspace_file_as_drift() {
    // NER-382: a deletion inside the workspace dir is drift too — re-materializing
    // would silently resurrect the file, masking the (possibly intentional) removal.
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    std::fs::remove_file(workspace.join("README.md")).expect("delete materialized file");

    let drift = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(drift["errors"][0]["code"], "WORKSPACE_DRIFT");
    let paths: Vec<&str> = drift["errors"][0]["details"]["paths"]
        .as_array()
        .expect("drift details carry a paths list")
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"README.md"),
        "deleted workspace file must be listed as drifted: {paths:?}"
    );
    // The structured recovery keys ride along (additive forge.cli.v0 details).
    assert_eq!(
        drift["errors"][0]["details"]["override_flag"],
        "--discard-workspace-changes"
    );
}

#[cfg(unix)]
#[test]
fn attach_reports_symlink_drift() {
    // NER-382 bytes/paths semantics: replacing a materialized regular file with a
    // symlink is drift (type change), and re-pointing a materialized symlink at a
    // different target is drift (the recorded blob pins the target bytes).
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt_prepared(|repo| {
        std::os::unix::fs::symlink("README.md", repo.path().join("link.txt"))
            .expect("create repo-root symlink");
    });
    // Type change: README.md becomes a symlink (to identical bytes, even).
    std::fs::remove_file(workspace.join("README.md")).expect("remove regular file");
    std::os::unix::fs::symlink("link.txt", workspace.join("README.md"))
        .expect("replace with symlink");
    // Target change: the materialized symlink now points somewhere else.
    std::fs::remove_file(workspace.join("link.txt")).expect("remove materialized symlink");
    std::os::unix::fs::symlink("ELSEWHERE.md", workspace.join("link.txt"))
        .expect("re-point symlink");

    let drift = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(drift["errors"][0]["code"], "WORKSPACE_DRIFT");
    let paths: Vec<&str> = drift["errors"][0]["details"]["paths"]
        .as_array()
        .expect("drift details carry a paths list")
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"README.md"),
        "regular file replaced by a symlink must drift: {paths:?}"
    );
    assert!(
        paths.contains(&"link.txt"),
        "symlink with a changed target must drift: {paths:?}"
    );
}

#[cfg(unix)]
#[test]
fn attach_ignores_chmod_only_workspace_change() {
    // NER-382 documented semantics: the drift equality is bytes/paths only — an
    // executable-bit-only change in the workspace dir is NOT drift (the recorded
    // tree pins content identity, not permissions).
    use std::os::unix::fs::PermissionsExt;

    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    std::fs::set_permissions(
        workspace.join("README.md"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod workspace file");

    repo.forge()
        .args(["--json", "attempt", "attach", &second_attempt])
        .assert()
        .success();
}

#[test]
fn attach_drift_check_honors_workspace_gitignore() {
    // NER-382 fix 1 (gitignore parity): the workspace walk honors the .gitignore
    // materialized INSIDE the workspace dir, exactly like the scanner that built
    // the recorded tree. Without it, ignored build artifacts would be permanent
    // false drift that --discard-workspace-changes can never clear (the deletion
    // pass honors the same ignore semantics and never removes them).
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt_prepared(|repo| {
        std::fs::write(repo.path().join(".gitignore"), "target/\n")
            .expect("write repo-root gitignore");
    });
    assert_eq!(
        std::fs::read_to_string(workspace.join(".gitignore")).unwrap(),
        "target/\n",
        "the .gitignore is part of the materialized workspace tree"
    );
    std::fs::create_dir_all(workspace.join("target")).expect("create ignored dir");
    std::fs::write(workspace.join("target/artifact.o"), "build output\n")
        .expect("write ignored artifact");
    // A NON-ignored stray file still drifts — and the ignored artifact is not listed.
    std::fs::write(workspace.join("STRAY.txt"), "stray\n").expect("write stray");
    let drift = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(drift["errors"][0]["code"], "WORKSPACE_DRIFT");
    let paths: Vec<&str> = drift["errors"][0]["details"]["paths"]
        .as_array()
        .expect("drift details carry a paths list")
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"STRAY.txt"),
        "non-ignored stray file must drift: {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.starts_with("target/")),
        "gitignored artifact must not be reported as drift: {paths:?}"
    );

    // With only the gitignored artifact present, attach succeeds (no false drift) —
    // and re-materialization leaves the ignored artifact in place.
    std::fs::remove_file(workspace.join("STRAY.txt")).expect("remove stray");
    repo.forge()
        .args(["--json", "attempt", "attach", &second_attempt])
        .assert()
        .success();
    assert!(
        workspace.join("target/artifact.o").exists(),
        "re-materialization must not delete gitignored workspace artifacts"
    );
}

#[test]
fn attach_drift_check_excludes_private_labeled_paths() {
    // NER-382 fix 2 (private path labels): an in-workspace save records a
    // private-EXCLUDED public tree while the private file stays on disk, so the
    // drift equality must drop the attempt's private-labeled paths from both
    // sides — otherwise the guard reports false drift NAMING private paths and
    // steers the agent toward --discard-workspace-changes, which deletes them.
    let (repo, second_attempt, workspace) = native_repo_with_competing_attempt();
    repo.forge()
        .args([
            "--json",
            "visibility",
            "path",
            "set",
            "--kind",
            "attempt",
            "--id",
            &second_attempt,
            "--path",
            "NOTES.md",
        ])
        .assert()
        .success();
    std::fs::write(workspace.join("NOTES.md"), "private notes\n").expect("write private file");

    // With a genuinely drifted file alongside, the refusal must not name the
    // private path.
    std::fs::write(workspace.join("STRAY.txt"), "stray\n").expect("write stray");
    let drift = json_output(
        repo.forge()
            .args(["--json", "attempt", "attach", &second_attempt])
            .assert()
            .failure(),
    );
    assert_eq!(drift["errors"][0]["code"], "WORKSPACE_DRIFT");
    let paths: Vec<&str> = drift["errors"][0]["details"]["paths"]
        .as_array()
        .expect("drift details carry a paths list")
        .iter()
        .map(|path| path.as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"STRAY.txt"),
        "real drift still listed: {paths:?}"
    );
    assert!(
        !paths.contains(&"NOTES.md"),
        "private-labeled path must not be reported as drift: {paths:?}"
    );

    // With only the private-labeled file present, attach succeeds — no false drift.
    std::fs::remove_file(workspace.join("STRAY.txt")).expect("remove stray");
    repo.forge()
        .args(["--json", "attempt", "attach", &second_attempt])
        .assert()
        .success();
}

/// NER-382: both payload emission sites qualify `workspace_path` with
/// `workspace_role: "materialization_target"` — `start` (auto-attach) and
/// `attempt start`. (The idempotent `--request-id` replay site is pinned by
/// `attempt_start_payload_and_replay_carry_workspace_role` above.)
#[test]
fn start_and_attempt_start_payloads_carry_workspace_role() {
    let repo = TestRepo::new_git();
    repo.forge().args(["--json", "init"]).assert().success();

    let started = json_output(
        repo.forge()
            .args(["--json", "start", "role qualifier"])
            .assert()
            .success(),
    );
    assert!(started["data"]["workspace_path"].is_string());
    assert_eq!(started["data"]["workspace_role"], "materialization_target");

    let second = json_output(
        repo.forge()
            .args([
                "--json",
                "attempt",
                "start",
                "--intent",
                started["data"]["intent_id"].as_str().unwrap(),
            ])
            .assert()
            .success(),
    );
    assert!(second["data"]["workspace_path"].is_string());
    assert_eq!(second["data"]["workspace_role"], "materialization_target");
}
