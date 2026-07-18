//! Numbered `.sql` migration runner (NER-133 U3).
//!
//! The historic `apply_migrations` hard-coded a single embedded migration plus
//! two unconditional `ALTER`s, so a fresh init and an upgraded DB reached the same
//! columns by *different* genesis paths with no mechanism for "add a schema change
//! = drop in a numbered file". This module replaces that with an ordered list of
//! embedded `.sql` migrations, each applied (DDL + version stamp) in **one
//! `IMMEDIATE` transaction** via [`crate::with_immediate_retry`], recorded in
//! `schema_migrations` with a SHA-256 checksum, and gated on the recorded version.
//!
//! Invariants carried forward:
//! - **Crash-atomicity:** each migration's DDL and its `schema_migrations` row
//!   commit together in a single `IMMEDIATE` txn; a crash mid-upgrade leaves the
//!   DB at a clean version boundary.
//! - **Concurrent-init idempotency (NER-132 U5):** the version stamp is written
//!   with `INSERT OR IGNORE`, so a racing first-init / first-open cannot collide
//!   on the version PRIMARY KEY.
//! - **Read-only refuse:** a DB whose `MAX(version)` exceeds this binary's head
//!   was written by a newer Forge — the runner refuses with
//!   [`ForgeError::UnknownSchemaVersion`] rather than writing into it.
//! - **Checksum grandfathering:** the `checksum` column is nullable; rows applied
//!   before this runner carry `NULL` and skip verification, so normalizing the
//!   `001` baseline cannot brick an in-the-wild DB.

use crate::ForgeError;
use anyhow::Result;
use forge_core::now_ms;
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Ordered, embedded migrations: `(version, name, sql)`. Adding a schema change
/// is appending one `include_str!` entry here plus its numbered `.sql` file.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "001_init", include_str!("../migrations/001_init.sql")),
    (
        2,
        "002_columns",
        include_str!("../migrations/002_columns.sql"),
    ),
    (
        3,
        "003_check_spec",
        include_str!("../migrations/003_check_spec.sql"),
    ),
    (
        4,
        "004_integrity_and_actor",
        include_str!("../migrations/004_integrity_and_actor.sql"),
    ),
    (
        5,
        "005_native_history",
        include_str!("../migrations/005_native_history.sql"),
    ),
    (
        6,
        "006_native_history_commit_id",
        include_str!("../migrations/006_native_history_commit_id.sql"),
    ),
    (
        7,
        "007_expected_content_ref",
        include_str!("../migrations/007_expected_content_ref.sql"),
    ),
    (
        8,
        "008_conflict_data",
        include_str!("../migrations/008_conflict_data.sql"),
    ),
    (
        9,
        "009_attempt_workspaces",
        include_str!("../migrations/009_attempt_workspaces.sql"),
    ),
    (
        10,
        "010_storage_policy",
        include_str!("../migrations/010_storage_policy.sql"),
    ),
    (
        11,
        "011_local_signatures",
        include_str!("../migrations/011_local_signatures.sql"),
    ),
    (
        12,
        "012_trust_policy",
        include_str!("../migrations/012_trust_policy.sql"),
    ),
    (
        13,
        "013_signing_key_origins",
        include_str!("../migrations/013_signing_key_origins.sql"),
    ),
    (
        14,
        "014_sync_merge_signature_subject",
        include_str!("../migrations/014_sync_merge_signature_subject.sql"),
    ),
    (
        15,
        "015_trust_ladder_attestation_levels",
        include_str!("../migrations/015_trust_ladder_attestation_levels.sql"),
    ),
    (
        16,
        "016_hosted_runner_attestations",
        include_str!("../migrations/016_hosted_runner_attestations.sql"),
    ),
    (
        17,
        "017_third_party_attestations",
        include_str!("../migrations/017_third_party_attestations.sql"),
    ),
    (
        18,
        "018_visibility_policy",
        include_str!("../migrations/018_visibility_policy.sql"),
    ),
    (
        19,
        "019_org_identity_governance",
        include_str!("../migrations/019_org_identity_governance.sql"),
    ),
    (
        20,
        "020_encrypted_private_content",
        include_str!("../migrations/020_encrypted_private_content.sql"),
    ),
    (
        21,
        "021_embargo_workflow",
        include_str!("../migrations/021_embargo_workflow.sql"),
    ),
    (
        22,
        "022_contracts",
        include_str!("../migrations/022_contracts.sql"),
    ),
];

/// The highest migration version this binary knows how to apply.
pub(crate) fn schema_head() -> i64 {
    MIGRATIONS
        .iter()
        .map(|(version, _, _)| *version)
        .max()
        .unwrap_or(0)
}

/// Read the DB's recorded schema version — `MAX(version)` from
/// `schema_migrations` — as a cheap, lock-free probe. Returns `0` for a brand-new
/// or empty DB: either the `schema_migrations` table does not exist yet (never
/// migrated) or it holds no rows (`MAX` is `NULL`). This deliberately reads only
/// the version ledger, never any domain row, so the transient `migrate()`
/// fast-path can decide up-to-date / pending / ahead without touching the lock.
pub(crate) fn current_schema_version(conn: &Connection) -> Result<i64> {
    // A DB that has never been migrated has no `schema_migrations` table; probe
    // `sqlite_master` first so the absent-table case is `0`, not an error.
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !table_exists {
        return Ok(0);
    }
    // `MAX(version)` is `NULL` on an empty table; coalesce to `0`.
    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    Ok(version)
}

/// Hex-encoded SHA-256 of a migration's SQL text.
fn checksum_of(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Ensure `schema_migrations` exists with the current columns, including the
/// nullable `checksum` column. A pre-existing table created before this runner
/// lacks `checksum`; probe `PRAGMA table_info` (mirroring the historic
/// `ensure_*_column` pattern) and `ALTER ADD COLUMN` it in if absent. Nullable so
/// rows stamped before this runner grandfather without verification.
pub(crate) fn bootstrap_schema_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL,
            checksum TEXT
        );",
    )?;

    let mut statement = conn.prepare("PRAGMA table_info(schema_migrations)")?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    let mut has_checksum = false;
    for column in columns {
        if column? == "checksum" {
            has_checksum = true;
            break;
        }
    }
    if !has_checksum {
        conn.execute("ALTER TABLE schema_migrations ADD COLUMN checksum TEXT", [])?;
    }
    Ok(())
}

/// Apply every embedded migration newer than the DB's recorded version, in order,
/// each in a single `IMMEDIATE` transaction. Assumes the caller has serialized
/// write access (under the repo lock) — it never touches the lock itself.
pub(crate) fn apply_pending_migrations(conn: &mut Connection) -> Result<()> {
    bootstrap_schema_migrations(conn)?;

    let mut applied: HashMap<i64, Option<String>> = HashMap::new();
    {
        let mut statement = conn.prepare("SELECT version, checksum FROM schema_migrations")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (version, checksum) = row?;
            applied.insert(version, checksum);
        }
    }
    let max_applied = applied.keys().copied().max().unwrap_or(0);

    // A DB ahead of this binary was written by a newer Forge: refuse to write.
    if max_applied > schema_head() {
        return Err(ForgeError::UnknownSchemaVersion {
            db_version: max_applied,
            supported_head: schema_head(),
        }
        .into());
    }

    for (version, name, sql) in MIGRATIONS {
        let version = *version;
        let checksum = checksum_of(sql);

        if version <= max_applied {
            // Already applied: verify a recorded (non-NULL) checksum still matches.
            // NULL checksums (rows stamped before this runner) grandfather.
            if let Some(Some(stored)) = applied.get(&version) {
                if stored != &checksum {
                    return Err(ForgeError::MigrationFailed {
                        version,
                        message: "checksum mismatch".into(),
                    }
                    .into());
                }
            }
            continue;
        }

        // Pending: apply DDL + stamp the version in one IMMEDIATE txn. INSERT OR
        // IGNORE preserves the NER-132 concurrent-init idempotency shim.
        //
        // Apply the migration STATEMENT-BY-STATEMENT rather than as one
        // `execute_batch(sql)`. A single batch aborts on the first failing
        // statement, so a historic DB at v1 whose `content_backend` column is
        // already inline (the `cd1bb3b`-era binary) would never run 002's *second*
        // `ALTER` (`attached_attempt_id`) and would brick on every command.
        // Splitting lets us tolerate an already-present column per statement and
        // still apply the rest. NOTE: this naive split on `;` is only safe because
        // our migration DDL contains no semicolons inside string literals — keep it
        // that way when adding migrations.
        let name = *name;
        crate::with_immediate_retry(conn, |tx| {
            for statement in sql.split(';') {
                let statement = statement.trim();
                if statement.is_empty() {
                    continue;
                }
                if let Err(error) = tx.execute_batch(statement) {
                    let message = error.to_string();
                    // 002 is purely additive ADD COLUMNs; an already-present column
                    // means the target state is already reached (the cd1bb3b inline
                    // schema). Treat it as satisfied so the v1 DB converges instead
                    // of bricking.
                    if message.contains("duplicate column name") {
                        continue;
                    }
                    // Any other DDL failure is a genuine migration defect — surface
                    // it typed (code MIGRATION_FAILED) so it is diagnosable rather
                    // than collapsing to COMMAND_FAILED.
                    return Err(ForgeError::MigrationFailed { version, message }.into());
                }
            }
            tx.execute(
                "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at_ms, checksum)
                 VALUES (?1, ?2, ?3, ?4)",
                params![version, name, now_ms(), checksum],
            )?;
            Ok(())
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// The OLD pre-revert `001_init.sql`: `content_backend` and
    /// `attached_attempt_id` are INLINE, and `schema_migrations` has NO `checksum`
    /// column. This reproduces a DB created by the merged binary (genesis case C)
    /// so the convergence test can prove the runner skips an at-HEAD inline schema
    /// and only bootstraps the missing checksum column. Kept verbatim here (not via
    /// `include_str!`) precisely because the shipped `001` no longer carries these.
    const MERGED_BINARY_V2_DDL: &str = "
        CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at_ms INTEGER NOT NULL
        );
        CREATE TABLE repositories (
            id TEXT PRIMARY KEY,
            root_path TEXT NOT NULL UNIQUE,
            git_head TEXT,
            content_backend TEXT NOT NULL DEFAULT 'git',
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE operations (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            request_id TEXT,
            command TEXT NOT NULL,
            status TEXT NOT NULL,
            kind TEXT NOT NULL,
            parent_operation_id TEXT REFERENCES operations(id),
            resulting_view_id TEXT,
            error_json TEXT,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE views (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            operation_id TEXT NOT NULL REFERENCES operations(id),
            kind TEXT NOT NULL,
            state_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE intents (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            text TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE attempts (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            intent_id TEXT NOT NULL REFERENCES intents(id),
            base_head TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE current_state (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            current_operation_id TEXT NOT NULL REFERENCES operations(id),
            current_view_id TEXT NOT NULL REFERENCES views(id),
            attached_attempt_id TEXT REFERENCES attempts(id),
            updated_at_ms INTEGER NOT NULL
        );
        -- Stand-ins for the remaining 001 tables that migration 004 ALTERs / scans
        -- (this synthetic fixture trims tables the convergence assertions ignore).
        CREATE TABLE evidence (id TEXT PRIMARY KEY);
        CREATE TABLE decisions (id TEXT PRIMARY KEY);
        CREATE TABLE publications (id TEXT PRIMARY KEY);
    ";

    /// The `cd1bb3b`-era v1 shape (genesis case D): `repositories.content_backend`
    /// is INLINE (it shipped that way in the historic binary) but
    /// `current_state` LACKS `attached_attempt_id`, and `schema_migrations` has no
    /// `checksum` column. This is the case 002 must reconcile: its first `ALTER`
    /// (`content_backend`) is a duplicate-column no-op, and only its SECOND `ALTER`
    /// (`attached_attempt_id`) does real work. A whole-file `execute_batch` would
    /// abort on the first statement and never reach the second — bricking the DB.
    const CD1BB3B_V1_INLINE_CONTENT_BACKEND_DDL: &str = "
        CREATE TABLE repositories (
            id TEXT PRIMARY KEY,
            root_path TEXT NOT NULL UNIQUE,
            git_head TEXT,
            content_backend TEXT NOT NULL DEFAULT 'git',
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE operations (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            request_id TEXT,
            command TEXT NOT NULL,
            status TEXT NOT NULL,
            kind TEXT NOT NULL,
            parent_operation_id TEXT REFERENCES operations(id),
            resulting_view_id TEXT,
            error_json TEXT,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE views (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            operation_id TEXT NOT NULL REFERENCES operations(id),
            kind TEXT NOT NULL,
            state_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE intents (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            text TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE attempts (
            id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            intent_id TEXT NOT NULL REFERENCES intents(id),
            base_head TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL
        );
        CREATE TABLE current_state (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            repo_id TEXT NOT NULL REFERENCES repositories(id),
            current_operation_id TEXT NOT NULL REFERENCES operations(id),
            current_view_id TEXT NOT NULL REFERENCES views(id),
            updated_at_ms INTEGER NOT NULL
        );
        -- Stand-ins for the remaining 001 tables that migration 004 ALTERs / scans
        -- (this synthetic fixture trims tables the convergence assertions ignore).
        CREATE TABLE evidence (id TEXT PRIMARY KEY);
        CREATE TABLE decisions (id TEXT PRIMARY KEY);
        CREATE TABLE publications (id TEXT PRIMARY KEY);
    ";

    /// A `(name, type, notnull, dflt_value)` column descriptor from
    /// `PRAGMA table_info`, used as a SET (cid/ordinal deliberately excluded so
    /// inline-vs-ALTER ordinal drift is benign — all store SQL is by-name).
    fn column_set(conn: &Connection, table: &str) -> std::collections::BTreeSet<String> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table_info");
        let rows = statement
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let col_type: String = row.get(2)?;
                let notnull: i64 = row.get(3)?;
                let dflt: Option<String> = row.get(4)?;
                Ok(format!("{name}|{col_type}|{notnull}|{dflt:?}"))
            })
            .expect("query table_info");
        rows.map(|r| r.expect("table_info row")).collect()
    }

    fn applied_versions(conn: &Connection) -> Vec<(i64, Option<String>)> {
        let mut statement = conn
            .prepare("SELECT version, checksum FROM schema_migrations ORDER BY version")
            .expect("prepare versions");
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .expect("query versions");
        rows.map(|r| r.expect("version row")).collect()
    }

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.pragma_update(None, "foreign_keys", "ON")
            .expect("enable fks");
        conn
    }

    #[test]
    fn schema_head_is_max_version() {
        assert_eq!(schema_head(), 22);
    }

    #[test]
    fn checksum_is_stable_lowercase_hex() {
        let checksum = checksum_of("ALTER TABLE x ADD COLUMN y TEXT;");
        assert_eq!(checksum.len(), 64);
        assert!(checksum.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(checksum, checksum_of("ALTER TABLE x ADD COLUMN y TEXT;"));
    }

    /// Fresh apply reaches schema head with non-NULL checksums for every row.
    #[test]
    fn fresh_apply_reaches_head_with_checksums() {
        let mut conn = mem_conn();
        apply_pending_migrations(&mut conn).expect("apply migrations");

        let versions = applied_versions(&conn);
        assert_eq!(versions.len(), schema_head() as usize);
        assert_eq!(versions[0].0, 1);
        assert_eq!(versions[1].0, 2);
        assert_eq!(versions[2].0, 3);
        assert_eq!(versions[3].0, 4);
        assert_eq!(versions[4].0, 5);
        assert_eq!(versions[5].0, 6);
        assert_eq!(versions[6].0, 7);
        assert_eq!(versions[7].0, 8);
        assert_eq!(versions[8].0, 9);
        assert_eq!(versions[9].0, 10);
        assert_eq!(versions[10].0, 11);
        assert_eq!(versions[11].0, 12);
        assert_eq!(versions[12].0, 13);
        assert_eq!(versions[13].0, 14);
        assert_eq!(versions[14].0, 15);
        assert_eq!(versions[15].0, 16);
        assert_eq!(versions[16].0, 17);
        assert_eq!(versions[17].0, 18);
        assert_eq!(versions[18].0, 19);
        assert_eq!(versions[19].0, 20);
        assert_eq!(versions[20].0, 21);
        assert_eq!(versions[21].0, 22);
        assert!(versions[0].1.is_some(), "001 checksum must be non-NULL");
        assert!(versions[1].1.is_some(), "002 checksum must be non-NULL");
        assert!(versions[2].1.is_some(), "003 checksum must be non-NULL");
        assert!(versions[3].1.is_some(), "004 checksum must be non-NULL");
        assert!(versions[4].1.is_some(), "005 checksum must be non-NULL");
        assert!(versions[5].1.is_some(), "006 checksum must be non-NULL");
        assert!(versions[6].1.is_some(), "007 checksum must be non-NULL");
        assert!(versions[7].1.is_some(), "008 checksum must be non-NULL");
        assert!(versions[8].1.is_some(), "009 checksum must be non-NULL");
        assert!(versions[9].1.is_some(), "010 checksum must be non-NULL");
        assert!(versions[10].1.is_some(), "011 checksum must be non-NULL");
        assert!(versions[11].1.is_some(), "012 checksum must be non-NULL");
        assert!(versions[12].1.is_some(), "013 checksum must be non-NULL");
        assert!(versions[13].1.is_some(), "014 checksum must be non-NULL");
        assert!(versions[14].1.is_some(), "015 checksum must be non-NULL");
        assert!(versions[15].1.is_some(), "016 checksum must be non-NULL");
        assert!(versions[16].1.is_some(), "017 checksum must be non-NULL");
        assert!(versions[17].1.is_some(), "018 checksum must be non-NULL");
        assert!(versions[18].1.is_some(), "019 checksum must be non-NULL");
        assert!(versions[19].1.is_some(), "020 checksum must be non-NULL");
        assert!(versions[20].1.is_some(), "021 checksum must be non-NULL");
        assert!(versions[21].1.is_some(), "022 checksum must be non-NULL");

        // 005 seeds one native_object_format row; 006 bumps commit_schema_version -> 2
        // (justified-commit payload epoch) and adds object_format_version = 2 (kind-header
        // framing epoch). The per-object CommitObject.schema_version field stays 1.
        let format_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM native_object_format", [], |row| {
                row.get(0)
            })
            .expect("native_object_format exists");
        assert_eq!(format_rows, 1, "exactly one seeded format row");
        let (tag, algo, commit_v, object_v): (String, String, i64, i64) = conn
            .query_row(
                "SELECT format_tag, hash_algo, commit_schema_version, object_format_version FROM native_object_format WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("seeded format row");
        assert_eq!(
            (tag.as_str(), algo.as_str(), commit_v, object_v),
            ("f1", "sha256", 2, 2)
        );
        let conflict_columns = column_set(&conn, "conflict_sets");
        assert!(
            conflict_columns
                .iter()
                .any(|c| c.starts_with("content_hash|")),
            "008 adds conflict_sets.content_hash"
        );
        let path_columns = column_set(&conn, "path_conflicts");
        assert!(
            path_columns
                .iter()
                .any(|c| c.starts_with("path_fingerprint|")),
            "008 creates path_conflicts with path_fingerprint"
        );
        let policy: (i64, i64, i64) = conn
            .query_row(
                "SELECT protection_window_days, storage_budget_bytes, automatic_eviction
                 FROM storage_policy WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("010 seeds storage policy");
        assert_eq!(policy, (7, 1_073_741_824, 0));
        let trust_policy: (String, String) = conn
            .query_row(
                "SELECT min_accept_trust, min_export_trust
                 FROM trust_policy WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("012 seeds trust policy");
        assert_eq!(
            (trust_policy.0.as_str(), trust_policy.1.as_str()),
            ("self_reported", "self_reported")
        );
        let visibility_default: String = conn
            .query_row(
                "SELECT default_work_package_visibility
                 FROM visibility_policy WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("018 seeds visibility policy");
        assert_eq!(visibility_default, "public");
        let org_profile: (i64, i64, String) = conn
            .query_row(
                "SELECT enabled, policy_revision, recovery_status
                 FROM org_authority_profile WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("019 seeds disabled org authority profile");
        assert_eq!(org_profile, (0, 0, "normal".to_string()));
    }

    /// Build genesis case B — a GENUINE old v1 DB — by running the reverted-001
    /// baseline DDL (neither column present) and stamping version=1 with a NULL
    /// checksum. We do NOT drop columns from a v2 DB (the FK on
    /// `attached_attempt_id` makes that impossible, and dropping `content_backend`
    /// alone leaves the artificial one-column state that breaks 002). Upgrading
    /// then runs 002, ALTERing BOTH columns in cleanly.
    fn build_genuine_v1(conn: &Connection) {
        conn.execute_batch(MIGRATIONS[0].2)
            .expect("apply reverted-001 baseline");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL
            );",
        )
        .expect("create v1 schema_migrations (no checksum)");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (1, '001_init', 0)",
            [],
        )
        .expect("stamp version 1 with NULL checksum");
    }

    /// By-name convergence across the three genesis cases: fresh (A), genuine old
    /// v1 upgraded via 002 (B), and a merged-binary inline-v2 DB (C). All three
    /// must agree on the `repositories` and `current_state` column SETS
    /// (name/type/notnull/dflt — NOT cid order).
    #[test]
    fn three_genesis_cases_converge_by_name() {
        // (A) fresh.
        let mut a = mem_conn();
        apply_pending_migrations(&mut a).expect("fresh apply");

        // (B) genuine old v1, upgraded via 002.
        let mut b = mem_conn();
        build_genuine_v1(&b);
        apply_pending_migrations(&mut b).expect("upgrade v1");

        // (C) merged-binary inline v2: both columns inline, schema_migrations
        // without a checksum column, versions 1 & 2 stamped NULL. The runner must
        // skip both migrations and only bootstrap the checksum column — no error.
        let mut c = mem_conn();
        c.execute_batch(MERGED_BINARY_V2_DDL)
            .expect("build merged-binary inline v2");
        c.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (1, '001_init', 0)",
            [],
        )
        .expect("stamp v1");
        c.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (2, '002_columns', 0)",
            [],
        )
        .expect("stamp v2");
        apply_pending_migrations(&mut c).expect("inline v2 runs clean");

        // `intents` is included so migration 003's `check_spec_json` column is part of
        // the by-name convergence guard (code-review F4): case C (merged-binary v2)
        // is stamped only at version 2, so the runner must apply 003 to it.
        // `native_object_format` (005, NER-138 Phase 7 slice 2) is a new table created in
        // all three genesis paths — cases B and C are stamped below version 5, so the
        // runner must apply 005 to them — so its column set must converge too.
        for table in [
            "repositories",
            "current_state",
            "intents",
            "native_object_format",
        ] {
            let set_a = column_set(&a, table);
            let set_b = column_set(&b, table);
            let set_c = column_set(&c, table);
            assert_eq!(set_a, set_b, "fresh vs genuine-v1 diverge on {table}");
            assert_eq!(set_a, set_c, "fresh vs merged-v2 diverge on {table}");
        }

        // 005's INSERT OR IGNORE seeds the format row and 006's UPDATE bumps it on every
        // upgrade path, not just the fresh apply (which fresh_apply_reaches_head_with_checksums
        // covers) — column-set convergence proves the table exists, this proves the seed +
        // 006-bumped values match. object_format_version (006) is part of the convergence too.
        let seed = |conn: &Connection| -> (String, String, i64, i64) {
            conn.query_row(
                "SELECT format_tag, hash_algo, commit_schema_version, object_format_version FROM native_object_format WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("seeded native_object_format row")
        };
        assert_eq!(seed(&a), ("f1".to_string(), "sha256".to_string(), 2, 2));
        assert_eq!(seed(&b), seed(&a), "genuine-v1 seed diverges");
        assert_eq!(seed(&c), seed(&a), "merged-v2 seed diverges");
    }

    /// Re-running on a head DB is a no-op (no new rows, no error).
    #[test]
    fn rerun_on_head_is_noop() {
        let mut conn = mem_conn();
        apply_pending_migrations(&mut conn).expect("first apply");
        let before = applied_versions(&conn);
        apply_pending_migrations(&mut conn).expect("second apply");
        let after = applied_versions(&conn);
        assert_eq!(before, after);
    }

    /// A `schema_migrations` table lacking `checksum` gets the column added, and
    /// NULL-checksum rows are grandfathered (no verification failure).
    #[test]
    fn bootstrap_adds_checksum_and_grandfathers_nulls() {
        let mut conn = mem_conn();
        build_genuine_v1(&conn);
        // schema_migrations has no checksum column yet.
        assert!(
            !column_set(&conn, "schema_migrations")
                .iter()
                .any(|c| c.starts_with("checksum|")),
            "precondition: no checksum column"
        );
        apply_pending_migrations(&mut conn).expect("upgrade grandfathers NULL v1");
        assert!(
            column_set(&conn, "schema_migrations")
                .iter()
                .any(|c| c.starts_with("checksum|")),
            "checksum column bootstrapped"
        );
        // The grandfathered v1 row keeps its NULL checksum; the freshly-applied 002
        // carries a non-NULL checksum.
        let versions = applied_versions(&conn);
        assert_eq!(versions[0], (1, None));
        assert!(versions[1].1.is_some());
    }

    /// A DB stamped HEAD+1 ⇒ runner refuses with `UnknownSchemaVersion`.
    #[test]
    fn unknown_future_version_refuses() {
        let mut conn = mem_conn();
        apply_pending_migrations(&mut conn).expect("reach head");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms, checksum) VALUES (?1, 'future', 0, 'x')",
            params![schema_head() + 1],
        )
        .expect("stamp future version");

        let error = apply_pending_migrations(&mut conn).expect_err("must refuse");
        match error.downcast_ref::<ForgeError>() {
            Some(ForgeError::UnknownSchemaVersion {
                db_version,
                supported_head,
            }) => {
                assert_eq!(*db_version, schema_head() + 1);
                assert_eq!(*supported_head, schema_head());
            }
            other => panic!("expected UnknownSchemaVersion, got {other:?}"),
        }
    }

    /// A tampered already-applied migration (non-NULL checksum mismatch) is refused.
    #[test]
    fn tampered_checksum_is_refused() {
        let mut conn = mem_conn();
        // Build a v1 DB whose version-1 row carries a WRONG non-NULL checksum.
        conn.execute_batch(MIGRATIONS[0].2).expect("apply baseline");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL,
                checksum TEXT
            );",
        )
        .expect("create schema_migrations");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms, checksum)
             VALUES (1, '001_init', 0, 'deadbeef-not-the-real-checksum')",
            [],
        )
        .expect("stamp tampered v1");

        let error = apply_pending_migrations(&mut conn).expect_err("must refuse tampered");
        match error.downcast_ref::<ForgeError>() {
            Some(ForgeError::MigrationFailed { version, .. }) => assert_eq!(*version, 1),
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
    }

    /// FIX A (genesis case D): a `cd1bb3b`-era v1 DB has `content_backend` INLINE in
    /// `repositories` but `current_state` LACKS `attached_attempt_id`, stamped at
    /// version=1 with a NULL checksum. Running the migrator must CONVERGE — apply
    /// 002 statement-by-statement, no-op the duplicate `content_backend` ALTER, add
    /// the missing `attached_attempt_id`, stamp version=2 — and must NOT error
    /// (whole-file `execute_batch` would brick this DB on the duplicate column).
    #[test]
    fn cd1bb3b_v1_with_inline_content_backend_converges() {
        let mut conn = mem_conn();
        conn.execute_batch(CD1BB3B_V1_INLINE_CONTENT_BACKEND_DDL)
            .expect("build cd1bb3b inline-content_backend v1");
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL
            );",
        )
        .expect("create v1 schema_migrations (no checksum)");
        conn.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms) VALUES (1, '001_init', 0)",
            [],
        )
        .expect("stamp version 1 with NULL checksum");

        // Precondition: content_backend present (inline), attached_attempt_id absent.
        assert!(
            column_set(&conn, "repositories")
                .iter()
                .any(|c| c.starts_with("content_backend|")),
            "precondition: content_backend is inline"
        );
        assert!(
            !column_set(&conn, "current_state")
                .iter()
                .any(|c| c.starts_with("attached_attempt_id|")),
            "precondition: attached_attempt_id is absent"
        );

        apply_pending_migrations(&mut conn).expect("cd1bb3b v1 must converge, not brick");

        // Both columns now present; version advanced to HEAD.
        assert!(
            column_set(&conn, "repositories")
                .iter()
                .any(|c| c.starts_with("content_backend|")),
            "content_backend preserved"
        );
        assert!(
            column_set(&conn, "current_state")
                .iter()
                .any(|c| c.starts_with("attached_attempt_id|")),
            "attached_attempt_id added by the reconciling 002"
        );
        let versions = applied_versions(&conn);
        assert_eq!(
            versions.last().expect("at least one version").0,
            schema_head()
        );
        assert_eq!(
            current_schema_version(&conn).expect("version probe"),
            schema_head()
        );
    }

    /// FIX A: a genuinely-failing migration statement (a malformed `ALTER`, not a
    /// duplicate column) surfaces as `ForgeError::MigrationFailed`, not a raw
    /// COMMAND_FAILED. Driven by injecting a bad migration through the same
    /// per-statement applier the runner uses.
    #[test]
    fn malformed_migration_statement_surfaces_migration_failed() {
        let mut conn = mem_conn();
        // Reach a clean head first so we have the schema_migrations ledger.
        apply_pending_migrations(&mut conn).expect("reach head");

        // Drive the per-statement applier directly with a malformed statement and
        // assert it maps to the typed MigrationFailed (code MIGRATION_FAILED).
        let version = 99;
        let result: Result<()> = crate::with_immediate_retry(&mut conn, |tx| {
            for statement in "ALTER TABLE repositories ADD COLUMN".split(';') {
                let statement = statement.trim();
                if statement.is_empty() {
                    continue;
                }
                if let Err(error) = tx.execute_batch(statement) {
                    let message = error.to_string();
                    if message.contains("duplicate column name") {
                        continue;
                    }
                    return Err(ForgeError::MigrationFailed { version, message }.into());
                }
            }
            Ok(())
        });
        let error = result.expect_err("malformed ALTER must fail");
        match error.downcast_ref::<ForgeError>() {
            Some(ForgeError::MigrationFailed { version: v, .. }) => assert_eq!(*v, 99),
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
    }
}
