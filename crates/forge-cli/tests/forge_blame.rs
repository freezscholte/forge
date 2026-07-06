//! NER-362 slice 3: `forge blame` end-to-end through the CLI — line attribution over
//! native history, joined with per-commit provenance, resolved at the authoritative
//! LEDGER tip (never the ref-store HEAD, which lags by design).

mod common;
#[path = "support/sync.rs"]
mod sync_support;

use common::{forge_in, TestRepo};
use serde_json::Value;
use std::path::Path;
use sync_support::{
    cloned_peer_from_bundle, json, native_accept_file_change, native_accept_file_change_in,
    native_accepted_lifecycle,
};

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

fn head(path: &Path) -> Option<String> {
    std::fs::read_to_string(path.join(".forge/refs/HEAD"))
        .ok()
        .map(|raw| raw.trim().to_string())
}

/// Drive a native repo through one full `start → save → run → propose → check → accept`
/// cycle that writes `content` to `path`, returning the accepted commit id.
fn accept_change(repo: &TestRepo, intent: &str, path: &str, content: &str) -> String {
    repo.forge()
        .args(["--json", "start", intent])
        .assert()
        .success();
    std::fs::write(repo.path().join(path), content).expect("write file");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    let accepted = json_output(repo.forge().args(["--json", "accept"]).assert().success());
    accepted["data"]["commit_id"]
        .as_str()
        .expect("native accept surfaces commit_id")
        .to_string()
}

fn init_native(repo: &TestRepo) {
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
}

#[test]
fn blame_attributes_lines_to_their_introducing_commits() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let first = accept_change(&repo, "first change", "feature.txt", "one\ntwo\n");
    let second = accept_change(&repo, "second change", "feature.txt", "one\nTWO\nthree\n");

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );

    assert_eq!(out["schema_version"], "forge.cli.v0");
    assert_eq!(out["status"], "success");
    assert_eq!(out["data"]["path"], "feature.txt");
    assert_eq!(out["data"]["tip_commit_id"], second.as_str());
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["line_number"], 1);
    assert_eq!(lines[0]["content"], "one");
    assert_eq!(lines[0]["commit_id"], first.as_str());
    assert_eq!(lines[1]["line_number"], 2);
    assert_eq!(lines[1]["content"], "TWO");
    assert_eq!(lines[1]["commit_id"], second.as_str());
    assert_eq!(lines[2]["line_number"], 3);
    assert_eq!(lines[2]["content"], "three");
    assert_eq!(lines[2]["commit_id"], second.as_str());
    // Provenance fields come verbatim from the CommitObject: accepted commits carry
    // an intent, a proposal revision, a decision, and an actor.
    for line in lines {
        assert!(line["intent_id"].is_string(), "intent_id present: {line}");
        assert!(
            line["proposal_revision_id"].is_string(),
            "proposal_revision_id present: {line}"
        );
        assert!(
            line["decision_id"].is_string(),
            "decision_id present: {line}"
        );
        assert!(line["actor"].is_string(), "actor present: {line}");
    }
    // The two accepts carry different intents, so the join must be per-commit.
    assert_ne!(lines[0]["intent_id"], lines[1]["intent_id"]);
}

#[test]
fn blame_resolves_the_ledger_tip_even_when_head_lags() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let genesis = {
        repo.forge()
            .args(["--json", "start", "stale head window"])
            .assert()
            .success();
        head(repo.path()).expect("genesis HEAD exists after start")
    };
    std::fs::write(repo.path().join("feature.txt"), "alpha\nbeta\n").expect("write file");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    let accepted = json_output(repo.forge().args(["--json", "accept"]).assert().success());
    let commit_id = accepted["data"]["commit_id"].as_str().unwrap().to_string();

    // Simulate the post-accept / pre-reconcile window: the decision row committed but
    // the ref-store HEAD advance was lost (HEAD-lags-never-leads). Blame is read-only
    // and must not depend on reconcile_native_head having run.
    std::fs::write(repo.path().join(".forge/refs/HEAD"), &genesis).expect("rewind HEAD");
    assert_eq!(head(repo.path()).as_deref(), Some(genesis.as_str()));

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );
    assert_eq!(
        out["data"]["tip_commit_id"],
        commit_id.as_str(),
        "blame attributes at the ledger tip, not the stale ref-store HEAD"
    );
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2);
    for line in lines {
        assert_eq!(line["commit_id"], commit_id.as_str());
    }

    // Blame's tip equals `log`'s first commit (the shared ledger resolution)...
    let log = json_output(repo.forge().args(["--json", "log"]).assert().success());
    assert_eq!(log["data"]["commits"][0]["commit_id"], commit_id.as_str());
    // ...and blame stayed read-only: it never reconciled or advanced HEAD.
    assert_eq!(
        head(repo.path()).as_deref(),
        Some(genesis.as_str()),
        "read-only blame must not write the ref-store HEAD"
    );
}

#[test]
fn blame_requires_an_initialized_repository() {
    let repo = TestRepo::new_git();
    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .failure(),
    );
    assert_eq!(out["status"], "error");
    assert_eq!(out["errors"][0]["code"], "NOT_INITIALIZED");
}

#[test]
fn blame_errors_on_a_path_missing_at_the_tip() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    accept_change(&repo, "unrelated change", "feature.txt", "one\n");

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "missing.txt"])
            .assert()
            .failure(),
    );
    assert_eq!(out["status"], "error");
    let message = out["errors"][0]["message"].as_str().expect("message");
    assert!(message.contains("missing.txt"), "unexpected: {message}");
}

#[test]
fn blame_errors_on_a_binary_blob() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    repo.forge()
        .args(["--json", "start", "binary change"])
        .assert()
        .success();
    std::fs::write(repo.path().join("blob.bin"), b"\xff\xfe\x00\x01").expect("write binary");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    repo.forge().args(["--json", "accept"]).assert().success();

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "blob.bin"])
            .assert()
            .failure(),
    );
    assert_eq!(out["status"], "error");
    let message = out["errors"][0]["message"].as_str().expect("message");
    assert!(message.contains("binary"), "unexpected: {message}");
}

#[test]
fn blame_human_output_prints_one_row_per_line() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let commit_id = accept_change(&repo, "human rows", "feature.txt", "alpha\nbeta\n");

    let assert = repo
        .forge()
        .args(["blame", "feature.txt"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    let all: Vec<&str> = stdout.lines().collect();
    let digest = commit_id.rsplit(':').next().unwrap();
    let short = &digest[..12];
    // NER-362 slice 4: a commit legend block comes FIRST — one line per distinct
    // blamed commit (`<short-commit> <intent-title-or-"-">`), newest first.
    // One blamed commit here, so exactly one legend line, then the rows.
    assert_eq!(all.len(), 3, "legend line + one row per line: {stdout}");
    assert_eq!(
        all[0],
        format!("{short} human rows"),
        "legend carries the intent title: {stdout}"
    );
    // The per-line row format itself is unchanged: four space-separated fields.
    let rows = &all[1..];
    for (idx, (row, content)) in rows.iter().zip(["alpha", "beta"]).enumerate() {
        let mut fields = row.splitn(4, ' ');
        assert_eq!(fields.next(), Some(short), "short commit in: {row}");
        let intent = fields.next().expect("intent column");
        assert_ne!(intent, "", "intent column in: {row}");
        assert_eq!(fields.next(), Some((idx + 1).to_string().as_str()));
        assert_eq!(fields.next(), Some(content));
    }
}

#[test]
fn blame_human_legend_lists_each_blamed_commit_newest_first() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let first = accept_change(&repo, "first change", "feature.txt", "one\ntwo\n");
    let second = accept_change(&repo, "second change", "feature.txt", "one\nTWO\nthree\n");

    let assert = repo
        .forge()
        .args(["blame", "feature.txt"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    let all: Vec<&str> = stdout.lines().collect();
    // Two distinct blamed commits → two legend lines, then three rows.
    assert_eq!(all.len(), 5, "two legend lines + three rows: {stdout}");
    let short = |commit: &str| commit.rsplit(':').next().unwrap()[..12].to_string();
    assert_eq!(
        all[0],
        format!("{} second change", short(&second)),
        "newest blamed commit first: {stdout}"
    );
    assert_eq!(
        all[1],
        format!("{} first change", short(&first)),
        "older blamed commit second: {stdout}"
    );
}

#[test]
fn blame_json_lines_carry_ledger_enrichment() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    accept_change(&repo, "first change", "feature.txt", "one\ntwo\n");
    accept_change(&repo, "second change", "feature.txt", "one\nTWO\nthree\n");

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );

    assert_eq!(out["schema_version"], "forge.cli.v0");
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 3);
    // Line 1 was introduced by the first accept, lines 2-3 by the second: the
    // enrichment joins per commit, so titles differ per line.
    assert_eq!(lines[0]["intent_title"], "first change");
    assert_eq!(lines[1]["intent_title"], "second change");
    assert_eq!(lines[2]["intent_title"], "second change");
    for line in lines {
        // Every line came from an accepted proposal whose check passed.
        assert_eq!(line["decision_status"], "accepted", "in {line}");
        assert_eq!(line["check_status"], "passed", "in {line}");
        // The 362-3 fields are untouched by enrichment.
        assert!(line["intent_id"].is_string(), "intent_id present: {line}");
        assert!(line["commit_id"].is_string(), "commit_id present: {line}");
    }
}

// --- NER-362 slice 5 additions. The tests above are the seeded slice-3/4 suite
// and stay untouched; everything below extends coverage additively. ---

#[test]
fn blame_attributes_a_single_accept_to_its_intent() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let started = json_output(
        repo.forge()
            .args(["--json", "start", "single accept"])
            .assert()
            .success(),
    );
    let intent_id = started["data"]["intent_id"]
        .as_str()
        .expect("start surfaces intent_id")
        .to_string();
    std::fs::write(repo.path().join("feature.txt"), "one\ntwo\nthree\n").expect("write file");
    repo.forge().args(["--json", "save"]).assert().success();
    repo.forge()
        .args(["--json", "run", "--", "sh", "-c", "true"])
        .assert()
        .success();
    repo.forge().args(["--json", "propose"]).assert().success();
    repo.forge().args(["--json", "check"]).assert().success();
    let accepted = json_output(repo.forge().args(["--json", "accept"]).assert().success());
    let commit_id = accepted["data"]["commit_id"]
        .as_str()
        .expect("native accept surfaces commit_id");

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 3);
    for line in lines {
        assert_eq!(line["commit_id"], commit_id, "in {line}");
        assert_eq!(line["intent_id"], intent_id.as_str(), "in {line}");
    }
}

fn assert_snake_case_keys(value: &Value, context: &str) {
    for key in value.as_object().expect("json object").keys() {
        assert!(
            key.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "non-snake_case key {key:?} in {context}"
        );
    }
}

#[test]
fn blame_json_payload_is_snake_case_inside_the_standard_envelope() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    accept_change(&repo, "payload shape", "feature.txt", "one\ntwo\n");

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );

    // Standard forge.cli.v0 envelope.
    assert_eq!(out["schema_version"], "forge.cli.v0");
    assert_eq!(out["status"], "success");
    assert!(out["errors"].as_array().expect("errors array").is_empty());

    let data = &out["data"];
    assert_snake_case_keys(data, "blame data payload");
    assert_eq!(data["path"], "feature.txt");
    assert!(
        data["tip_commit_id"].is_string(),
        "payload carries the resolved ledger tip as tip_commit_id"
    );
    let lines = data["lines"].as_array().expect("lines array");
    assert!(!lines.is_empty());
    for line in lines {
        assert_snake_case_keys(line, "blame line");
        let object = line.as_object().expect("line object");
        // The 362-3 payload plus the 362-4 additive enrichment fields.
        for key in [
            "line_number",
            "content",
            "commit_id",
            "intent_id",
            "proposal_revision_id",
            "decision_id",
            "actor",
            "authored_time",
            "intent_title",
            "decision_status",
            "check_status",
        ] {
            assert!(object.contains_key(key), "line carries {key}: {line}");
        }
    }
}

#[test]
fn blame_enrichment_degrades_to_null_when_the_ledger_predates_the_ids() {
    let repo = TestRepo::new_git();
    init_native(&repo);
    let commit_id = accept_change(&repo, "predating ledger", "feature.txt", "alpha\nbeta\n");

    // Simulate history that predates some ledger rows: the commit objects keep
    // their intent / revision ids, but the rows those ids point at are gone.
    // The decisions rows stay intact — the ledger tip is resolved from them
    // (362-3-owned behavior), and this test is about enrichment degradation only.
    // Foreign-key enforcement is per-connection in SQLite; drop it here so the
    // parent rows can vanish while their children keep the dangling ids.
    let connection =
        rusqlite::Connection::open(repo.path().join(".forge/forge.db")).expect("open forge.db");
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DELETE FROM intents;
             DELETE FROM check_results;",
        )
        .expect("delete enrichment rows");
    drop(connection);

    let out = json_output(
        repo.forge()
            .args(["--json", "blame", "feature.txt"])
            .assert()
            .success(),
    );
    assert_eq!(out["status"], "success");
    assert_eq!(out["data"]["tip_commit_id"], commit_id.as_str());
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2);
    for line in lines {
        // Unresolvable ids degrade to null — never an error.
        assert!(line["intent_title"].is_null(), "in {line}");
        assert!(line["check_status"].is_null(), "in {line}");
        // The decisions row survived, so its field still resolves.
        assert_eq!(line["decision_status"], "accepted", "in {line}");
        // The 362-3 commit-object fields are untouched by the missing rows.
        assert!(line["intent_id"].is_string(), "in {line}");
        assert!(line["proposal_revision_id"].is_string(), "in {line}");
    }

    // Human mode also degrades: the legend title falls back to "-".
    let assert = repo
        .forge()
        .args(["blame", "feature.txt"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    let digest = commit_id.rsplit(':').next().unwrap();
    let short = &digest[..12];
    assert_eq!(
        stdout.lines().next(),
        Some(format!("{short} -").as_str()),
        "legend degrades to '-' when the intent row is gone: {stdout}"
    );
}

/// Native sync-merge commits carry `intent_id: None` (crates/forge-store/src/sync.rs —
/// `record_sync_merge_commit`), so the ledger-enrichment loop in `blame_response`
/// (crates/forge-cli/src/commands/core.rs — `let Some(intent_id) = … else { continue }`)
/// skips them entirely. When such a merge commit is a line's last-change attribution,
/// the enrichment fields must render as null and the command must still succeed. This
/// exercises the skip branch end-to-end: a clean divergent `sync pull` makes the merge
/// commit the blame tip, and — because the walk follows the FIRST parent (the receiver's
/// own head, which never had the source-side file) — the source-side file is "added" by
/// the merge commit, so the merge commit owns every one of its lines.
#[test]
fn blame_renders_intent_less_sync_merge_commits_with_null_enrichment() {
    let source = TestRepo::new_git();
    native_accepted_lifecycle(&source);
    let bundle_dir = tempfile::tempdir().expect("sync merge blame bundle dir");
    let bundle_path = bundle_dir.path().join("sync-merge-blame-base.json");
    source
        .forge()
        .args([
            "--json",
            "sync",
            "export",
            "--output",
            bundle_path.to_str().expect("utf8 base bundle path"),
        ])
        .assert()
        .success();

    // Source accepts a multi-line file the receiver never had; the peer accepts a
    // disjoint file. Disjoint edits merge cleanly (no conflict set), and the merged
    // tree carries the source-side file.
    native_accept_file_change(
        &source,
        "source only file",
        "source-only.txt",
        "sole one\nsole two\n",
    );
    let peer = cloned_peer_from_bundle(&bundle_path);
    native_accept_file_change_in(peer.path(), "peer only file", "peer-only.txt", "peer\n");

    let pulled = json(
        forge_in(peer.path())
            .args([
                "--json",
                "sync",
                "pull",
                source.path().to_str().expect("utf8 source path"),
            ])
            .assert()
            .success(),
    );
    assert_eq!(pulled["data"]["merged"], true, "{pulled}");
    assert_eq!(pulled["data"]["materialized"], true, "{pulled}");
    let merge_commit_id = pulled["data"]["merge_commit_id"]
        .as_str()
        .expect("clean sync pull surfaces a merge commit id")
        .to_string();

    // JSON blame over the source-side file: the merge commit is the tip and owns every
    // line, and — being intent-less — its enrichment fields degrade to null, not error.
    let out = json(
        forge_in(peer.path())
            .args(["--json", "blame", "source-only.txt"])
            .assert()
            .success(),
    );
    assert_eq!(out["status"], "success", "{out}");
    assert_eq!(
        out["data"]["tip_commit_id"],
        merge_commit_id.as_str(),
        "blame resolves the sync merge commit as the ledger tip: {out}"
    );
    let lines = out["data"]["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2, "{out}");
    for line in lines {
        // The intent-less sync merge commit is this line's last-change attribution.
        assert_eq!(
            line["commit_id"], merge_commit_id,
            "line attributed to the sync merge commit: {line}"
        );
        // The commit carries intent_id: None → the skip branch fires → every ledger
        // enrichment field renders null (never an error, never a stale join).
        assert!(line["intent_id"].is_null(), "intent_id null: {line}");
        assert!(line["intent_title"].is_null(), "intent_title null: {line}");
        assert!(
            line["decision_status"].is_null(),
            "decision_status null: {line}"
        );
        assert!(line["check_status"].is_null(), "check_status null: {line}");
    }

    // Human mode also degrades cleanly: the legend title for the intent-less merge
    // commit falls back to "-".
    let assert = forge_in(peer.path())
        .args(["blame", "source-only.txt"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8");
    let digest = merge_commit_id.rsplit(':').next().unwrap();
    let short = &digest[..12];
    assert_eq!(
        stdout.lines().next(),
        Some(format!("{short} -").as_str()),
        "legend title is '-' for the intent-less sync merge commit: {stdout}"
    );
}

#[test]
fn schema_lists_the_blame_command() {
    let repo = TestRepo::new_git();
    let out = json_output(repo.forge().args(["--json", "schema"]).assert().success());
    let commands = out["data"]["commands"].as_array().expect("commands array");
    assert!(
        commands.iter().any(|command| command["command"] == "blame"),
        "schema command registry must include blame"
    );
}
