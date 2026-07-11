//! `forge_store::migrate(cwd)` — the transient, self-acquiring schema-upgrade
//! entrypoint (NER-133 U4).
//!
//! These exercise `migrate` directly against hand-built `.forge/forge.db` files in
//! real (git-initialized) temp repos: a behind DB upgrades to head, an at-head DB
//! is a no-op, a forward-versioned DB is refused with `UnknownSchemaVersion`, and
//! an uninitialized repo is a no-op (no panic, no lock).
//!
//! `migrate` reads only `schema_migrations` and applies DDL — it never touches a
//! domain row — so a behind-DB fixture needs only the baseline tables plus a
//! version stamp, not a valid domain state. Critically, a v1 fixture is built from
//! the reverted-001 baseline (which has NEITHER `content_backend` NOR
//! `attached_attempt_id`), not by deleting a row from a v2 DB (which would still
//! carry both columns and make re-applying 002 hit "duplicate column name").

use rusqlite::Connection;
use std::path::Path;

/// The reverted-001 baseline DDL (neither `content_backend` nor
/// `attached_attempt_id`), read from the shipped migration file so the fixture
/// can never drift from the real baseline.
const BASELINE_001: &str = include_str!("../migrations/001_init.sql");
/// The 002 ALTERs (both columns) — used to build an at-head fixture.
const COLUMNS_002: &str = include_str!("../migrations/002_columns.sql");
/// The 003 ALTER (`intents.check_spec_json`) — used to build an at-head fixture.
const CHECK_SPEC_003: &str = include_str!("../migrations/003_check_spec.sql");
/// The 004 integrity/actor migration — used to build an at-head fixture.
const INTEGRITY_004: &str = include_str!("../migrations/004_integrity_and_actor.sql");
/// The 005 native-history migration (NER-138 Phase 7 slice 2) — used to build an
/// at-head fixture.
const NATIVE_HISTORY_005: &str = include_str!("../migrations/005_native_history.sql");
/// The 006 commit-id migration (NER-138 Phase 7 slice 3) — used to build an
/// at-head fixture now that HEAD is 7.
const NATIVE_HISTORY_COMMIT_ID_006: &str =
    include_str!("../migrations/006_native_history_commit_id.sql");
/// The 007 expected-content-ref migration (NER-143 R1) — used to build an at-head fixture.
const EXPECTED_CONTENT_REF_007: &str = include_str!("../migrations/007_expected_content_ref.sql");
/// The 008 conflict-data migration (NER-139 Phase 8 S2a).
const CONFLICT_DATA_008: &str = include_str!("../migrations/008_conflict_data.sql");
/// The 009 attempt-workspaces migration (NER-139 Phase 8 S4).
const ATTEMPT_WORKSPACES_009: &str = include_str!("../migrations/009_attempt_workspaces.sql");
/// The 010 storage-policy migration (Phase 8 S5).
const STORAGE_POLICY_010: &str = include_str!("../migrations/010_storage_policy.sql");
/// The 011 local-signatures migration (Phase 9 S1).
const LOCAL_SIGNATURES_011: &str = include_str!("../migrations/011_local_signatures.sql");
/// The 012 trust-policy migration (Phase 9 S2).
const TRUST_POLICY_012: &str = include_str!("../migrations/012_trust_policy.sql");
/// The 013 signing-key-origin migration (Phase 9 remote key trust).
const SIGNING_KEY_ORIGINS_013: &str = include_str!("../migrations/013_signing_key_origins.sql");
/// The 014 sync-merge signature subject migration.
const SYNC_MERGE_SIGNATURE_SUBJECT_014: &str =
    include_str!("../migrations/014_sync_merge_signature_subject.sql");
/// The 015 trust-ladder attestation-level migration.
const TRUST_LADDER_ATTESTATION_LEVELS_015: &str =
    include_str!("../migrations/015_trust_ladder_attestation_levels.sql");
/// The 016 hosted-runner attestation migration.
const HOSTED_RUNNER_ATTESTATIONS_016: &str =
    include_str!("../migrations/016_hosted_runner_attestations.sql");
/// The 017 third-party attestation migration.
const THIRD_PARTY_ATTESTATIONS_017: &str =
    include_str!("../migrations/017_third_party_attestations.sql");
/// The 018 visibility policy migration.
const VISIBILITY_POLICY_018: &str = include_str!("../migrations/018_visibility_policy.sql");
/// The 019 org identity governance migration.
const ORG_IDENTITY_GOVERNANCE_019: &str =
    include_str!("../migrations/019_org_identity_governance.sql");
/// The 020 encrypted private content migration.
const ENCRYPTED_PRIVATE_CONTENT_020: &str =
    include_str!("../migrations/020_encrypted_private_content.sql");
/// The 021 embargo workflow migration.
const EMBARGO_WORKFLOW_021: &str = include_str!("../migrations/021_embargo_workflow.sql");

const CONTRACTS_022: &str = include_str!("../migrations/022_contracts.sql");

/// Initialize a real git repo in a fresh temp dir (so `git rev-parse
/// --show-toplevel`, which `migrate` uses to resolve the root, succeeds).
fn git_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create temp dir");
    run_git(dir.path(), &["init"]);
    run_git(dir.path(), &["config", "user.email", "forge@example.test"]);
    run_git(dir.path(), &["config", "user.name", "Forge Test"]);
    dir
}

fn run_git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Create `<root>/.forge/forge.db`, returning the database path.
fn make_forge_db(root: &Path) -> std::path::PathBuf {
    let forge_dir = root.join(".forge");
    std::fs::create_dir_all(&forge_dir).expect("create .forge");
    forge_dir.join("forge.db")
}

/// Open a connection with `foreign_keys=ON` (matching `open_connection`'s pragma,
/// so the 002 `REFERENCES` ALTER behaves the same way in-fixture).
fn open(db: &Path) -> Connection {
    let conn = Connection::open(db).expect("open db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    conn
}

/// Stamp a `schema_migrations(version, name, applied_at_ms)` ledger WITHOUT the
/// `checksum` column (a genuine pre-runner v1 ledger), inserting one row per
/// version with a NULL checksum.
fn stamp_versions(conn: &Connection, versions: &[(i64, &str)]) {
    conn.execute_batch(
        "CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL
        );",
    )
    .expect("create schema_migrations");
    for (version, name) in versions {
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (?1, ?2, 0)",
            rusqlite::params![version, name],
        )
        .expect("stamp version");
    }
}

/// Whether a table has a named column (by `PRAGMA table_info`).
fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    let mut rows = statement.query([]).expect("query table_info");
    while let Some(row) = rows.next().expect("row") {
        let name: String = row.get(1).expect("column name");
        if name == column {
            return true;
        }
    }
    false
}

fn apply_through_012(conn: &Connection) {
    conn.execute_batch(BASELINE_001).expect("apply baseline");
    conn.execute_batch(COLUMNS_002).expect("apply 002 ALTERs");
    conn.execute_batch(CHECK_SPEC_003).expect("apply 003 ALTER");
    conn.execute_batch(INTEGRITY_004).expect("apply 004 ALTERs");
    conn.execute_batch(NATIVE_HISTORY_005)
        .expect("apply 005 native-history");
    conn.execute_batch(NATIVE_HISTORY_COMMIT_ID_006)
        .expect("apply 006 commit-id");
    conn.execute_batch(EXPECTED_CONTENT_REF_007)
        .expect("apply 007 expected-content-ref");
    conn.execute_batch(CONFLICT_DATA_008)
        .expect("apply 008 conflict-data");
    conn.execute_batch(ATTEMPT_WORKSPACES_009)
        .expect("apply 009 attempt-workspaces");
    conn.execute_batch(STORAGE_POLICY_010)
        .expect("apply 010 storage-policy");
    conn.execute_batch(LOCAL_SIGNATURES_011)
        .expect("apply 011 local-signatures");
    conn.execute_batch(TRUST_POLICY_012)
        .expect("apply 012 trust-policy");
}

fn apply_through_013(conn: &Connection) {
    apply_through_012(conn);
    conn.execute_batch(SIGNING_KEY_ORIGINS_013)
        .expect("apply 013 signing-key-origins");
}

fn apply_through_014(conn: &Connection) {
    apply_through_013(conn);
    conn.execute_batch(SYNC_MERGE_SIGNATURE_SUBJECT_014)
        .expect("apply 014 sync-merge signature subject");
}

fn apply_through_019(conn: &Connection) {
    apply_through_014(conn);
    conn.execute_batch(TRUST_LADDER_ATTESTATION_LEVELS_015)
        .expect("apply 015 trust-ladder attestation levels");
    conn.execute_batch(HOSTED_RUNNER_ATTESTATIONS_016)
        .expect("apply 016 hosted-runner attestations");
    conn.execute_batch(THIRD_PARTY_ATTESTATIONS_017)
        .expect("apply 017 third-party attestations");
    conn.execute_batch(VISIBILITY_POLICY_018)
        .expect("apply 018 visibility-policy");
    conn.execute_batch(ORG_IDENTITY_GOVERNANCE_019)
        .expect("apply 019 org identity governance");
}

fn apply_through_020(conn: &Connection) {
    apply_through_019(conn);
    conn.execute_batch(ENCRYPTED_PRIVATE_CONTENT_020)
        .expect("apply 020 encrypted private content");
}

fn apply_through_021(conn: &Connection) {
    apply_through_020(conn);
    conn.execute_batch(EMBARGO_WORKFLOW_021)
        .expect("apply 021 embargo workflow");
}

#[test]
fn contracts_signature_rebuild_preserves_existing_signature_rows() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_021(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");
    let seeded = [
        (
            "sig_evidence",
            "evidence",
            "evidence_existing",
            "digest_evidence",
            40_i64,
        ),
        (
            "sig_decision",
            "decision",
            "decision_existing",
            "digest_decision",
            41,
        ),
        (
            "sig_commit",
            "commit",
            "commit_existing",
            "digest_commit",
            42,
        ),
        (
            "sig_sync_merge",
            "sync_merge_commit",
            "sync_merge_existing",
            "digest_sync_merge",
            43,
        ),
    ];
    for (id, subject_kind, subject_id, signed_digest, created_at_ms) in seeded {
        conn.execute(
            "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (?1, 'repo_test', ?2, ?3, ?4, 'ed25519', 'public_key_existing',
                   'fingerprint_existing', 'signature_existing', 'locally_signed', ?5)",
            rusqlite::params![id, subject_kind, subject_id, signed_digest, created_at_ms],
        )
        .expect("insert pre-existing signature");
    }

    conn.execute_batch(CONTRACTS_022)
        .expect("apply 022 contracts");

    let preserved: Vec<(String, String, String, String, i64)> = conn
        .prepare(
            "SELECT id, subject_kind, subject_id, signed_digest, created_at_ms
             FROM ledger_signatures ORDER BY created_at_ms",
        )
        .expect("prepare preserved signatures")
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query preserved signatures")
        .collect::<Result<_, _>>()
        .expect("collect preserved signatures");
    let expected: Vec<(String, String, String, String, i64)> = seeded
        .iter()
        .map(|(id, kind, subject, digest, ts)| {
            (
                (*id).to_string(),
                (*kind).to_string(),
                (*subject).to_string(),
                (*digest).to_string(),
                *ts,
            )
        })
        .collect();
    assert_eq!(
        preserved, expected,
        "pre-022 signature rows must survive the ledger_signatures rebuild byte-for-byte"
    );

    for kind in [
        "contract",
        "contract_run",
        "contract_stop",
        "contract_run_verdict",
    ] {
        conn.execute(
            "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (?1, 'repo_test', ?2, ?3, 'digest_new', 'ed25519', 'pk',
                   'fp', 'sig', 'locally_signed', 50)",
            rusqlite::params![format!("sig_{kind}"), kind, format!("{kind}_subject")],
        )
        .unwrap_or_else(|_| panic!("widened CHECK must admit subject_kind {kind}"));
    }
}

fn max_version(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )
    .expect("max version")
}

#[test]
fn signing_key_origin_backfill_labels_existing_signature_keys_as_peer() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_012(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");
    conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_peer', 'repo_test', 'evidence', 'ev_peer', 'digest',
            'ed25519', 'peer_public_key', 'peer_fingerprint', 'peer_signature',
            'locally_signed', 7
         )",
        [],
    )
    .expect("insert pre-existing signature");

    conn.execute_batch(SIGNING_KEY_ORIGINS_013)
        .expect("apply 013 signing-key-origins");

    let trust_origin: String = conn
        .query_row(
            "SELECT trust_origin FROM signing_keys
             WHERE repo_id = 'repo_test' AND key_fingerprint = 'peer_fingerprint'",
            [],
            |row| row.get(0),
        )
        .expect("query signing key origin");
    assert_eq!(
        trust_origin, "peer",
        "migration must fail closed for pre-existing signature rows"
    );
}

#[test]
fn sync_merge_signature_subject_migration_preserves_existing_signature_rows() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_013(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");
    for (id, subject_kind, subject_id, signed_digest, created_at_ms) in [
        (
            "sig_evidence",
            "evidence",
            "evidence_existing",
            "digest_evidence",
            40,
        ),
        (
            "sig_decision",
            "decision",
            "decision_existing",
            "digest_decision",
            41,
        ),
        (
            "sig_existing",
            "commit",
            "commit_existing",
            "digest_existing",
            42,
        ),
    ] {
        conn.execute(
            "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (?1, 'repo_test', ?2, ?3, ?4, 'ed25519', 'public_key_existing',
                   'fingerprint_existing', 'signature_existing', 'locally_signed', ?5)",
            rusqlite::params![id, subject_kind, subject_id, signed_digest, created_at_ms],
        )
        .expect("insert pre-existing signature");
    }

    conn.execute_batch(SYNC_MERGE_SIGNATURE_SUBJECT_014)
        .expect("apply 014 sync-merge signature subject");

    let preserved: Vec<(String, String, String, String, i64)> = conn
        .prepare(
            "SELECT id, subject_kind, subject_id, signed_digest, created_at_ms
             FROM ledger_signatures ORDER BY created_at_ms",
        )
        .expect("prepare preserved signatures")
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("query preserved signatures")
        .collect::<rusqlite::Result<_>>()
        .expect("collect preserved signatures");
    assert_eq!(
        preserved,
        vec![
            (
                "sig_evidence".to_string(),
                "evidence".to_string(),
                "evidence_existing".to_string(),
                "digest_evidence".to_string(),
                40,
            ),
            (
                "sig_decision".to_string(),
                "decision".to_string(),
                "decision_existing".to_string(),
                "digest_decision".to_string(),
                41,
            ),
            (
                "sig_existing".to_string(),
                "commit".to_string(),
                "commit_existing".to_string(),
                "digest_existing".to_string(),
                42,
            ),
        ]
    );

    conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_sync_merge', 'repo_test', 'sync_merge_commit', 'commit_merge', 'digest_merge',
            'ed25519', 'public_key_existing', 'fingerprint_existing', 'signature_merge',
            'locally_signed', 43
         )",
        [],
    )
    .expect("new sync_merge_commit subject kind is accepted");
    let rejected = conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_bogus', 'repo_test', 'bogus', 'commit_bogus', 'digest_bogus',
            'ed25519', 'public_key_existing', 'fingerprint_existing', 'signature_bogus',
            'locally_signed', 44
         )",
        [],
    );
    assert!(
        rejected.is_err(),
        "014 must preserve the subject_kind CHECK constraint"
    );
    let duplicate = conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_duplicate', 'repo_test', 'sync_merge_commit', 'commit_merge', 'digest_merge',
            'ed25519', 'public_key_existing', 'fingerprint_existing', 'signature_duplicate',
            'locally_signed', 45
         )",
        [],
    );
    assert!(
        duplicate.is_err(),
        "014 must preserve the ledger signature uniqueness constraint"
    );
    let orphan_repo = conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_orphan', 'missing_repo', 'sync_merge_commit', 'commit_orphan', 'digest_orphan',
            'ed25519', 'public_key_existing', 'fingerprint_existing', 'signature_orphan',
            'locally_signed', 46
         )",
        [],
    );
    assert!(
        orphan_repo.is_err(),
        "014 must preserve the repository foreign key"
    );
    let bad_trust = conn.execute(
        "INSERT INTO ledger_signatures (
            id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
            public_key, key_fingerprint, signature, trust_level, created_at_ms
         ) VALUES (
            'sig_bad_trust', 'repo_test', 'sync_merge_commit', 'commit_bad_trust', 'digest_bad_trust',
            'ed25519', 'public_key_existing', 'fingerprint_existing', 'signature_bad_trust',
            'peer_signed', 47
         )",
        [],
    );
    assert!(
        bad_trust.is_err(),
        "014 must preserve the trust_level CHECK constraint"
    );
    let index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_ledger_signatures_subject'",
            [],
            |row| row.get(0),
        )
        .expect("query ledger signature index");
    assert_eq!(
        index_count, 1,
        "014 must recreate the ledger signature subject index"
    );
}

#[test]
fn trust_ladder_attestation_level_migration_allows_higher_policy_rungs() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_014(&conn);

    conn.execute_batch(TRUST_LADDER_ATTESTATION_LEVELS_015)
        .expect("apply 015 trust-ladder attestation levels");
    conn.execute(
        "UPDATE trust_policy
         SET min_accept_trust = 'hosted_runner_signed',
             min_export_trust = 'third_party_attested'
         WHERE singleton = 1",
        [],
    )
    .expect("higher trust levels should satisfy 015 CHECK constraints");

    let policy: (String, String) = conn
        .query_row(
            "SELECT min_accept_trust, min_export_trust
             FROM trust_policy WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query migrated trust policy");
    assert_eq!(policy.0, "hosted_runner_signed");
    assert_eq!(policy.1, "third_party_attested");
}

#[test]
fn org_identity_migration_enforces_principal_and_key_references() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_019(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");

    let orphan_alias = conn.execute(
        "INSERT INTO org_principal_aliases (
            id, repo_id, principal_id, alias_kind, alias_value, visibility, state,
            created_at_ms, updated_at_ms
         ) VALUES (
            'alias_orphan', 'repo_test', 'actor_missing', 'actor', 'alice',
            'private', 'active', 1, 1
         )",
        [],
    );
    assert!(
        orphan_alias.is_err(),
        "aliases must reference an existing principal in the same repo"
    );

    conn.execute(
        "INSERT INTO org_principals (id, repo_id, kind, state, created_at_ms, updated_at_ms)
         VALUES ('actor_owner', 'repo_test', 'human', 'active', 1, 1)",
        [],
    )
    .expect("insert principal");

    let missing_key = conn.execute(
        "INSERT INTO org_key_bindings (
            id, repo_id, principal_id, key_fingerprint, public_key, binding_authority,
            state, valid_from_revision, created_at_ms, updated_at_ms
         ) VALUES (
            'key_missing', 'repo_test', 'actor_owner', 'fingerprint_missing', 'public_key',
            'actor_owner', 'active', 1, 1, 1
         )",
        [],
    );
    assert!(
        missing_key.is_err(),
        "key bindings must reference an existing signing key"
    );

    conn.execute(
        "INSERT INTO signing_keys (
            repo_id, key_fingerprint, public_key, trust_origin, created_at_ms, updated_at_ms
         ) VALUES ('repo_test', 'fingerprint_owner', 'public_key', 'local', 1, 1)",
        [],
    )
    .expect("insert signing key");
    conn.execute(
        "INSERT INTO org_key_bindings (
            id, repo_id, principal_id, key_fingerprint, public_key, binding_authority,
            state, valid_from_revision, created_at_ms, updated_at_ms
         ) VALUES (
            'key_owner', 'repo_test', 'actor_owner', 'fingerprint_owner', 'public_key',
            'actor_owner', 'active', 1, 1, 1
         )",
        [],
    )
    .expect("valid key binding");

    let missing_authority = conn.execute(
        "INSERT INTO org_role_bindings (
            id, repo_id, principal_id, role, authority, state, valid_from_revision,
            created_at_ms, updated_at_ms
         ) VALUES (
            'role_bad', 'repo_test', 'actor_owner', 'maintainer', 'actor_missing',
            'active', 1, 1, 1
         )",
        [],
    );
    assert!(
        missing_authority.is_err(),
        "role bindings must reference an existing authority principal"
    );
}

#[test]
fn encrypted_private_content_migration_enforces_overlay_references() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_020(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");

    let orphan_encryption_key = conn.execute(
        "INSERT INTO org_encryption_key_bindings (
            id, repo_id, principal_id, key_fingerprint, public_key, binding_authority,
            state, valid_from_revision, created_at_ms, updated_at_ms
         ) VALUES (
            'enc_missing', 'repo_test', 'actor_missing', 'age-x25519:fingerprint',
            'age1recipient', 'actor_missing', 'active', 1, 1, 1
         )",
        [],
    );
    assert!(
        orphan_encryption_key.is_err(),
        "encryption key bindings must reference org principals"
    );

    conn.execute(
        "INSERT INTO org_principals (id, repo_id, kind, state, created_at_ms, updated_at_ms)
         VALUES ('actor_owner', 'repo_test', 'human', 'active', 1, 1)",
        [],
    )
    .expect("insert principal");
    conn.execute(
        "INSERT INTO org_encryption_key_bindings (
            id, repo_id, principal_id, key_fingerprint, public_key, binding_authority,
            state, valid_from_revision, created_at_ms, updated_at_ms
         ) VALUES (
            'enc_owner', 'repo_test', 'actor_owner', 'age-x25519:fingerprint',
            'age1recipient', 'actor_owner', 'active', 1, 1, 1
         )",
        [],
    )
    .expect("valid encryption key binding does not require signing key row");

    conn.execute(
        "INSERT INTO intents (id, repo_id, text, check_spec_json, created_at_ms)
         VALUES ('intent_test', 'repo_test', 'private work', NULL, 1)",
        [],
    )
    .expect("insert intent");
    conn.execute(
        "INSERT INTO attempts (id, repo_id, intent_id, base_head, status, created_at_ms)
         VALUES ('attempt_test', 'repo_test', 'intent_test', 'HEAD0', 'active', 1)",
        [],
    )
    .expect("insert attempt");
    conn.execute(
        "INSERT INTO private_path_labels (
            id, repo_id, work_package_kind, work_package_id, path_hash,
            encrypted_display_path, visibility, created_at_ms, updated_at_ms
         ) VALUES (
            'path_private', 'repo_test', 'attempt', 'attempt_test',
            'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            'encrypted-path-metadata', 'private', 1, 1
         )",
        [],
    )
    .expect("insert private path label");

    let duplicate_label = conn.execute(
        "INSERT INTO private_path_labels (
            id, repo_id, work_package_kind, work_package_id, path_hash,
            encrypted_display_path, visibility, created_at_ms, updated_at_ms
         ) VALUES (
            'path_duplicate', 'repo_test', 'attempt', 'attempt_test',
            'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            'encrypted-path-metadata-2', 'private', 2, 2
         )",
        [],
    );
    assert!(
        duplicate_label.is_err(),
        "private path labels are unique per work package and path hash"
    );

    let missing_label_payload = conn.execute(
        "INSERT INTO encrypted_private_payloads (
            id, repo_id, work_package_kind, work_package_id, path_label_id, path_hash,
            envelope_format, recipient_fingerprint, ciphertext_digest, private_object_path,
            encrypted_metadata_json, created_at_ms
         ) VALUES (
            'payload_missing_label', 'repo_test', 'attempt', 'attempt_test', 'path_missing',
            'sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
            'age-x25519-v1', 'age-x25519:fingerprint',
            'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc',
            '.forge/private/objects/sha256/cc', '{}', 1
         )",
        [],
    );
    assert!(
        missing_label_payload.is_err(),
        "encrypted payloads must reference a private path label"
    );

    conn.execute(
        "INSERT INTO intents (id, repo_id, text, check_spec_json, created_at_ms)
         VALUES ('intent_other', 'repo_test', 'other private work', NULL, 1)",
        [],
    )
    .expect("insert other intent");
    conn.execute(
        "INSERT INTO attempts (id, repo_id, intent_id, base_head, status, created_at_ms)
         VALUES ('attempt_other', 'repo_test', 'intent_other', 'HEAD0', 'active', 1)",
        [],
    )
    .expect("insert other attempt");
    let mismatched_label_payload = conn.execute(
        "INSERT INTO encrypted_private_payloads (
            id, repo_id, work_package_kind, work_package_id, path_label_id, path_hash,
            envelope_format, recipient_fingerprint, ciphertext_digest, private_object_path,
            encrypted_metadata_json, created_at_ms
         ) VALUES (
            'payload_mismatched_label', 'repo_test', 'attempt', 'attempt_other', 'path_private',
            'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            'age-x25519-v1', 'age-x25519:fingerprint',
            'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd',
            '.forge/private/objects/sha256/dd', '{}', 1
         )",
        [],
    );
    assert!(
        mismatched_label_payload.is_err(),
        "encrypted payload label must match work package and path hash"
    );
}

#[test]
fn behind_db_upgrades_to_head() {
    let repo = git_repo();
    let db = make_forge_db(repo.path());
    {
        let conn = open(&db);
        conn.execute_batch(BASELINE_001)
            .expect("apply reverted-001 baseline");
        stamp_versions(&conn, &[(1, "001_init")]);
        // Sanity: a genuine v1 fixture has NEITHER column yet.
        assert!(!has_column(&conn, "repositories", "content_backend"));
        assert!(!has_column(&conn, "current_state", "attached_attempt_id"));
    }

    forge_store::migrate(repo.path()).expect("migrate upgrades a behind DB");

    let conn = open(&db);
    assert!(
        has_column(&conn, "repositories", "content_backend"),
        "002 added content_backend"
    );
    assert!(
        has_column(&conn, "current_state", "attached_attempt_id"),
        "002 added attached_attempt_id"
    );
    assert!(
        has_column(&conn, "intents", "check_spec_json"),
        "003 added check_spec_json"
    );
    assert!(
        has_column(&conn, "evidence", "content_hash"),
        "004 added evidence.content_hash"
    );
    assert!(
        has_column(&conn, "native_object_format", "format_tag"),
        "005 created native_object_format"
    );
    assert!(
        has_column(&conn, "decisions", "commit_id"),
        "006 added decisions.commit_id"
    );
    assert!(
        has_column(&conn, "native_object_format", "object_format_version"),
        "006 added object_format_version"
    );
    assert!(
        has_column(&conn, "current_state", "expected_content_ref"),
        "007 added expected_content_ref"
    );
    assert!(
        has_column(&conn, "conflict_sets", "content_hash"),
        "008 added conflict_sets.content_hash"
    );
    assert!(
        has_column(&conn, "path_conflicts", "path_fingerprint"),
        "008 created path_conflicts"
    );
    assert!(
        has_column(&conn, "attempt_workspaces", "workspace_rel_path"),
        "009 created attempt_workspaces"
    );
    assert!(
        has_column(
            &conn,
            "visibility_policy",
            "default_work_package_visibility"
        ),
        "018 created visibility_policy"
    );
    assert!(
        has_column(&conn, "org_authority_profile", "policy_revision"),
        "019 created org_authority_profile"
    );
    assert!(
        has_column(&conn, "org_encryption_key_bindings", "key_fingerprint"),
        "020 created org_encryption_key_bindings"
    );
    assert!(
        has_column(&conn, "private_path_labels", "path_hash"),
        "020 created private_path_labels"
    );
    assert!(
        has_column(&conn, "encrypted_private_payloads", "ciphertext_digest"),
        "020 created encrypted_private_payloads"
    );
    assert!(
        has_column(&conn, "embargo_workflows", "state"),
        "021 created embargo_workflows"
    );
    assert!(
        has_column(&conn, "embargo_workflow_events", "policy_revision"),
        "021 created embargo_workflow_events"
    );
    assert!(
        has_column(
            &conn,
            "embargo_release_authorizations",
            "content_classes_json"
        ),
        "021 created embargo_release_authorizations"
    );
    assert!(
        has_column(&conn, "contracts", "contract_id"),
        "022 created contracts"
    );
    assert!(
        has_column(&conn, "contract_revisions", "source_yaml"),
        "022 created contract_revisions"
    );
    assert!(
        has_column(&conn, "contract_runs", "outcome"),
        "022 created contract_runs"
    );
    assert!(
        has_column(&conn, "contract_stops", "malformed"),
        "022 created contract_stops"
    );
    assert!(
        has_column(&conn, "contract_run_verdicts", "verdict_kind"),
        "022 created contract_run_verdicts"
    );
    assert_eq!(max_version(&conn), 22, "reached HEAD=22");
}

#[test]
fn migration_021_backfills_existing_embargoed_visibility_rows() {
    let conn = Connection::open_in_memory().expect("open memory db");
    conn.pragma_update(None, "foreign_keys", "ON")
        .expect("enable fks");
    apply_through_020(&conn);
    conn.execute(
        "INSERT INTO repositories (id, root_path, git_head, content_backend, created_at_ms)
         VALUES ('repo_test', '/tmp/forge-test', NULL, 'native', 0)",
        [],
    )
    .expect("insert repository");
    conn.execute(
        "INSERT INTO work_package_visibility (
            repo_id, work_package_kind, work_package_id, visibility, created_at_ms, updated_at_ms
         ) VALUES ('repo_test', 'proposal', 'proposal_legacy', 'embargoed', 100, 200)",
        [],
    )
    .expect("insert legacy embargo visibility");

    conn.execute_batch(EMBARGO_WORKFLOW_021)
        .expect("apply 021 embargo workflow");

    let state: String = conn
        .query_row(
            "SELECT state FROM embargo_workflows
             WHERE repo_id = 'repo_test' AND work_package_kind = 'proposal' AND work_package_id = 'proposal_legacy'",
            [],
            |row| row.get(0),
        )
        .expect("backfilled workflow");
    assert_eq!(state, "active");
    let event_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM embargo_workflow_events
             WHERE repo_id = 'repo_test' AND work_package_kind = 'proposal'
               AND work_package_id = 'proposal_legacy' AND action = 'mark'
               AND actor = 'migration' AND new_state = 'active'",
            [],
            |row| row.get(0),
        )
        .expect("backfilled workflow event");
    assert_eq!(event_count, 1);
}

#[test]
fn at_head_db_is_a_noop() {
    let repo = git_repo();
    let db = make_forge_db(repo.path());
    {
        let conn = open(&db);
        apply_through_021(&conn);
        stamp_versions(
            &conn,
            &[
                (1, "001_init"),
                (2, "002_columns"),
                (3, "003_check_spec"),
                (4, "004_integrity_and_actor"),
                (5, "005_native_history"),
                (6, "006_native_history_commit_id"),
                (7, "007_expected_content_ref"),
                (8, "008_conflict_data"),
                (9, "009_attempt_workspaces"),
                (10, "010_storage_policy"),
                (11, "011_local_signatures"),
                (12, "012_trust_policy"),
                (13, "013_signing_key_origins"),
                (14, "014_sync_merge_signature_subject"),
                (15, "015_trust_ladder_attestation_levels"),
                (16, "016_hosted_runner_attestations"),
                (17, "017_third_party_attestations"),
                (18, "018_visibility_policy"),
                (19, "019_org_identity_governance"),
                (20, "020_encrypted_private_content"),
                (21, "021_embargo_workflow"),
                (22, "022_contracts"),
            ],
        );
        assert_eq!(max_version(&conn), 22);
    }

    forge_store::migrate(repo.path()).expect("at-head migrate is Ok");

    let conn = open(&db);
    assert_eq!(max_version(&conn), 22, "still at HEAD, unchanged");
}

#[test]
fn head_plus_one_is_refused() {
    let repo = git_repo();
    let db = make_forge_db(repo.path());
    {
        let conn = open(&db);
        apply_through_021(&conn);
        // HEAD is now 22, so the genuinely-ahead stamp is 23.
        stamp_versions(
            &conn,
            &[
                (1, "001_init"),
                (2, "002_columns"),
                (3, "003_check_spec"),
                (4, "004_integrity_and_actor"),
                (5, "005_native_history"),
                (6, "006_native_history_commit_id"),
                (7, "007_expected_content_ref"),
                (8, "008_conflict_data"),
                (9, "009_attempt_workspaces"),
                (10, "010_storage_policy"),
                (11, "011_local_signatures"),
                (12, "012_trust_policy"),
                (13, "013_signing_key_origins"),
                (14, "014_sync_merge_signature_subject"),
                (15, "015_trust_ladder_attestation_levels"),
                (16, "016_hosted_runner_attestations"),
                (17, "017_third_party_attestations"),
                (18, "018_visibility_policy"),
                (19, "019_org_identity_governance"),
                (20, "020_encrypted_private_content"),
                (21, "021_embargo_workflow"),
                (22, "022_contracts"),
                (23, "future"),
            ],
        );
        assert_eq!(max_version(&conn), 23);
    }

    let error = forge_store::migrate(repo.path()).expect_err("HEAD+1 must be refused");
    match error.downcast_ref::<forge_store::ForgeError>() {
        Some(forge_store::ForgeError::UnknownSchemaVersion {
            db_version,
            supported_head,
        }) => {
            assert_eq!(*db_version, 23);
            assert_eq!(*supported_head, 22);
        }
        other => panic!("expected UnknownSchemaVersion, got {other:?}"),
    }
}

#[test]
fn uninitialized_repo_is_a_noop() {
    // A git repo with no `.forge/forge.db` at all: migrate must return Ok(()) so
    // the command's own logic surfaces NOT_INITIALIZED — no panic, no lock.
    let repo = git_repo();
    assert!(!repo.path().join(".forge/forge.db").exists());
    forge_store::migrate(repo.path()).expect("uninitialized repo is a no-op");
}

#[test]
fn not_a_git_repo_is_a_noop() {
    // Outside any git work tree, `git_root` fails and migrate no-ops (Ok), letting
    // the command surface the not-a-git-repo error itself.
    let dir = tempfile::tempdir().expect("temp dir");
    forge_store::migrate(dir.path()).expect("non-git dir is a no-op");
}
