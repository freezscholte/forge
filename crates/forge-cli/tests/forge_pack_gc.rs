mod common;

use common::TestRepo;
use forge_content_native::{NativeObjectStore, ObjectId, ObjectKind};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::process::Command;

const OLD_MS: u64 = 1_577_836_800_000;

fn json_output(assert: assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assert.get_output().stdout).expect("valid json")
}

#[test]
fn gc_packs_old_reachable_loose_objects_and_deletes_unreachable_orphans() {
    // Repack candidates are REACHABLE loose objects only. Reachable content (built
    // through a real start/save lifecycle) is repacked with its loose duplicate
    // reclaimed; an unreferenced orphan blob is planned under
    // `deletable_loose_native_objects` and deleted outright — never copied into a
    // fresh (protection-window-shielded) pack.
    let repo = native_repo();
    repo.forge()
        .args(["--json", "start", "pack reachable content"])
        .assert()
        .success();
    std::fs::write(repo.path().join("packed.txt"), "pack me\n").expect("write worktree file");
    repo.forge().args(["--json", "save"]).assert().success();

    let store = NativeObjectStore::new(repo.path());
    let orphan = store
        .write_object(ObjectKind::Blob, b"unreachable orphan")
        .expect("write orphan loose object");
    backdate_all_loose_objects(repo.path());

    let dry = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );
    let candidates: Vec<ObjectId> = dry["data"]["pack_candidate_native_objects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| ObjectId::parse(value.as_str().unwrap()).expect("candidate id"))
        .collect();
    assert!(
        !candidates.is_empty(),
        "reachable old loose objects must be repack candidates: dry={dry}"
    );
    assert!(
        !contains_id(&dry["data"]["pack_candidate_native_objects"], &orphan),
        "an unreachable loose object must never be a repack candidate"
    );
    assert!(contains_id(
        &dry["data"]["deletable_loose_native_objects"],
        &orphan
    ));
    assert!(dry["data"]["loose_duplicate_native_objects"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(dry["data"]["deletable_native_packs"]
        .as_array()
        .unwrap()
        .is_empty());

    let digest = dry["data"]["plan_digest"].as_str().unwrap();
    let gc = json_output(
        repo.forge()
            .args(["--json", "gc", "--yes", "--plan-digest", digest])
            .assert()
            .success(),
    );
    assert_eq!(
        gc["data"]["created_packs"].as_array().unwrap().len(),
        1,
        "gc={gc}"
    );
    for id in &candidates {
        assert!(contains_id(&gc["data"]["deleted"], id));
        assert!(!object_path(repo.path(), id).exists());
        assert!(
            store.read_object(id).is_ok(),
            "repacked reachable object must stay readable"
        );
    }
    assert!(contains_id(&gc["data"]["deleted"], &orphan));
    assert!(!object_path(repo.path(), &orphan).exists());
    assert!(
        store.read_object(&orphan).is_err(),
        "the unreachable orphan must be reclaimed, not repacked"
    );

    let doctor = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(doctor["data"]["ok"], true, "doctor={doctor}");
    assert!(doctor["data"]["native_pack_issues"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn gc_crash_after_pack_write_overretains_loose_duplicates() {
    // Only reachable loose objects are repacked, so the pack-then-crash scenario
    // needs reachable content from a real start/save lifecycle.
    let repo = native_repo();
    repo.forge()
        .args(["--json", "start", "crash safe content"])
        .assert()
        .success();
    std::fs::write(repo.path().join("crash.txt"), "crash safe\n").expect("write worktree file");
    repo.forge().args(["--json", "save"]).assert().success();
    backdate_all_loose_objects(repo.path());

    let dry = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );
    let id = ObjectId::parse(
        dry["data"]["pack_candidate_native_objects"]
            .as_array()
            .unwrap()
            .first()
            .expect("a reachable repack candidate")
            .as_str()
            .unwrap(),
    )
    .expect("candidate id");
    let digest = dry["data"]["plan_digest"].as_str().unwrap();
    run_until_crash(
        repo.path(),
        "gc_after_pack_before_loose_delete",
        &["--json", "gc", "--yes", "--plan-digest", digest],
    );

    assert!(object_path(repo.path(), &id).exists());
    assert_eq!(
        pack_index_count(repo.path()),
        1,
        "pack/index should be durable before loose deletion starts"
    );
    NativeObjectStore::new(repo.path())
        .read_object(&id)
        .expect("read after crash");
    let doctor = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(doctor["data"]["ok"], true, "doctor={doctor}");
}

#[test]
fn gc_deletes_pack_only_when_every_entry_is_old_and_unreachable() {
    let repo = native_repo();
    let store = NativeObjectStore::new(repo.path());
    let id = store
        .write_object(ObjectKind::Blob, b"old packed orphan")
        .expect("write loose object");
    write_test_pack_from_loose_objects(repo.path(), "oldpack", std::slice::from_ref(&id), true);
    std::fs::remove_file(object_path(repo.path(), &id)).expect("delete loose copy");

    let dry = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );
    assert!(contains_string(
        &dry["data"]["deletable_native_packs"],
        "oldpack"
    ));
    let digest = dry["data"]["plan_digest"].as_str().unwrap();

    let gc = json_output(
        repo.forge()
            .args(["--json", "gc", "--yes", "--plan-digest", digest])
            .assert()
            .success(),
    );
    assert!(contains_string(&gc["data"]["deleted_packs"], "oldpack"));
    assert!(!repo.path().join(".forge/packs/oldpack.fidx").exists());
    assert!(!repo.path().join(".forge/packs/oldpack.fpack").exists());
}

#[test]
fn packed_objects_with_ambiguous_age_metadata_remain_protected() {
    let repo = native_repo();
    let store = NativeObjectStore::new(repo.path());
    let id = store
        .write_object(ObjectKind::Blob, b"ambiguous packed orphan")
        .expect("write loose object");
    write_test_pack_from_loose_objects(repo.path(), "ambiguous", std::slice::from_ref(&id), false);
    std::fs::remove_file(object_path(repo.path(), &id)).expect("delete loose copy");

    let dry = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );
    assert!(contains_id(&dry["data"]["unreachable_native_objects"], &id));
    assert!(contains_id(&dry["data"]["protected_native_objects"], &id));
    assert!(!contains_string(
        &dry["data"]["deletable_native_packs"],
        "ambiguous"
    ));
}

#[test]
fn doctor_reports_corrupt_pack_and_gc_yes_refuses_deletion() {
    let repo = native_repo();
    let store = NativeObjectStore::new(repo.path());
    let id = store
        .write_object(ObjectKind::Blob, b"corrupt packed orphan")
        .expect("write loose object");
    write_test_pack_from_loose_objects(repo.path(), "corrupt", std::slice::from_ref(&id), true);
    std::fs::remove_file(object_path(repo.path(), &id)).expect("delete loose copy");
    std::fs::write(repo.path().join(".forge/packs/corrupt.fpack"), b"not zstd")
        .expect("corrupt pack data");

    let doctor = json_output(repo.forge().args(["--json", "doctor"]).assert().success());
    assert_eq!(doctor["data"]["ok"], false, "doctor={doctor}");
    assert!(!doctor["data"]["native_pack_issues"]
        .as_array()
        .unwrap()
        .is_empty());

    let dry = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );
    let digest = dry["data"]["plan_digest"].as_str().unwrap();
    let gc = json_output(
        repo.forge()
            .args(["--json", "gc", "--yes", "--plan-digest", digest])
            .assert()
            .failure(),
    );
    assert!(gc["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("doctor"));
}

#[test]
fn gc_plan_digest_changes_when_collectable_loose_set_changes() {
    let repo = native_repo();
    let store = NativeObjectStore::new(repo.path());
    let first = store
        .write_object(ObjectKind::Blob, b"first")
        .expect("write first");
    mark_object_old(repo.path(), &first);
    let before = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );

    let second = store
        .write_object(ObjectKind::Blob, b"second")
        .expect("write second");
    mark_object_old(repo.path(), &second);
    let after = json_output(
        repo.forge()
            .args(["--json", "gc", "--dry-run"])
            .assert()
            .success(),
    );

    assert_ne!(
        before["data"]["plan_digest"], after["data"]["plan_digest"],
        "adding a collectable unreachable loose object must change the gc plan digest"
    );
}

fn native_repo() -> TestRepo {
    let repo = TestRepo::new_git();
    repo.forge()
        .args(["--json", "init", "--content-backend", "native"])
        .assert()
        .success();
    repo
}

fn run_until_crash(repo: &std::path::Path, crash_point: &str, args: &[&str]) {
    let output = Command::new(assert_cmd::cargo::cargo_bin("forge"))
        .args(args)
        .env("FORGE_CRASH_POINT", crash_point)
        .current_dir(repo)
        .output()
        .expect("spawn forge");
    assert!(
        !output.status.success(),
        "expected injected crash `{crash_point}` to abort `forge {args:?}`, but it succeeded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn backdate_all_loose_objects(repo_path: &std::path::Path) {
    let objects = repo_path.join(".forge/objects/sha256");
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
                .expect("run touch");
            assert!(status.success(), "backdate mtime failed");
        }
    }
}

fn mark_object_old(repo_path: &std::path::Path, id: &ObjectId) {
    let status = std::process::Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(object_path(repo_path, id))
        .status()
        .expect("run touch");
    assert!(status.success(), "touch old mtime failed");
}

fn object_path(repo_path: &std::path::Path, id: &ObjectId) -> std::path::PathBuf {
    repo_path
        .join(".forge/objects/sha256")
        .join(&id.digest()[..2])
        .join(id.digest())
}

fn pack_index_count(repo_path: &std::path::Path) -> usize {
    std::fs::read_dir(repo_path.join(".forge/packs"))
        .expect("read packs")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("fidx"))
        .count()
}

fn contains_id(value: &Value, id: &ObjectId) -> bool {
    contains_string(value, &id.to_string())
}

fn contains_string(value: &Value, needle: &str) -> bool {
    value
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value.as_str() == Some(needle))
}

fn write_test_pack_from_loose_objects(
    repo_path: &std::path::Path,
    pack_id: &str,
    ids: &[ObjectId],
    include_age_metadata: bool,
) {
    let packs_dir = repo_path.join(".forge/packs");
    std::fs::create_dir_all(&packs_dir).expect("create packs dir");
    let mut offset = 0_u64;
    let mut data = Vec::new();
    let mut entries = Vec::new();
    for id in ids {
        let frame = std::fs::read(object_path(repo_path, id)).expect("read loose frame");
        let compressed = zstd::stream::encode_all(Cursor::new(&frame), 0).expect("compress frame");
        let compressed_len = compressed.len() as u64;
        data.extend_from_slice(&compressed);
        let mut entry = serde_json::json!({
            "object_id": id.to_string(),
            "offset": offset,
            "framed_len": frame.len(),
            "compressed_len": compressed_len,
            "checksum": hex_lower(&Sha256::digest(&compressed)),
        });
        if include_age_metadata {
            entry["packed_at_ms"] = serde_json::json!(OLD_MS);
            entry["loose_mtime_ms"] = serde_json::json!(OLD_MS);
        }
        entries.push(entry);
        offset += compressed_len;
    }
    std::fs::write(packs_dir.join(format!("{pack_id}.fpack")), data).expect("write pack");
    std::fs::write(
        packs_dir.join(format!("{pack_id}.fidx")),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "pack_id": pack_id,
            "entries": entries,
        }))
        .expect("serialize index"),
    )
    .expect("write index");
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}
