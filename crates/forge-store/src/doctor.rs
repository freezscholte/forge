use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub issues: Vec<String>,
    /// Non-fatal guidance for configuration that can confuse Forge workflows. Warnings do
    /// not affect `ok` and are also surfaced as top-level CLI `warnings[]`.
    pub warnings: Vec<String>,
    pub schema_version: Option<i64>,
    /// File-byte accounting for `.forge`, grouped by stable storage category. This is
    /// informational only; storage-budget overflow is reported separately and never evicts.
    pub storage: StorageAccounting,
    pub storage_policy: StoragePolicy,
    pub storage_budget: StorageBudgetStatus,
    pub dangling_temp_files: Vec<String>,
    /// `content_ref` rows whose referenced object is missing or fails
    /// verification — the failure mode the store-before-DB ordering (NER-132)
    /// makes impossible; this is the safety-net assertion. Empty in a healthy repo.
    pub dangling_content_refs: Vec<String>,
    /// Worktree paths holding a leftover crash-atomic-restore temp
    /// (`.forge-restore-*`), the signature of a restore killed mid-flight. Empty
    /// in a healthy repo.
    pub half_applied_worktrees: Vec<String>,
    /// Rows whose tamper-evident hash failed verification (NER-136): an evidence or
    /// decision row whose content no longer matches its stored hash, an operation
    /// whose chain link is broken (a deletion/reorder), or a post-watermark missing
    /// hash. Empty in a healthy repo. A head-truncated chain (a lost latest op) is
    /// NOT a tamper — it verifies as a legitimately-shorter chain.
    pub tampered_rows: Vec<TamperedRow>,
    /// Native commit-DAG integrity breaks (NER-138 Phase 7 slice 3): a parent cycle, a
    /// dangling parent/tree object, or a `decisions.commit_id` whose commit object is absent.
    /// Empty in a healthy repo. This is the "DAG has no cycles/dangling parents (doctor
    /// verifies)" whole-phase exit criterion.
    pub native_history_issues: Vec<NativeHistoryFinding>,
    /// Corrupt `views.state_json` rows that make GC's reachability root set
    /// untrustworthy. Empty in a healthy repo.
    pub ledger_view_issues: Vec<LedgerViewFinding>,
    /// Contract-family ops whose referenced domain row was deleted (F5): a
    /// row+signature co-deletion that the op-chain and signature passes miss. Empty
    /// in a healthy repo.
    pub contract_row_issues: Vec<ContractRowFinding>,
    /// Native pack/index entries that fail offset, checksum, decompression, hash, or kind
    /// verification. Empty in a healthy repo.
    pub native_pack_issues: Vec<String>,
    /// Local Phase 9 signing findings: post-signature-migration evidence rows, decision rows,
    /// and native accepted commit ids must carry a valid Ed25519 `locally_signed` attestation.
    /// Empty in a healthy repo. Legacy pre-migration rows are grandfathered by rowid marker.
    pub signature_issues: Vec<SignatureFinding>,
    /// Signing-key origin labels. Local keys are keys minted or used by this repository;
    /// peer keys are valid signing keys imported through sync. A peer key may verify a
    /// signature cryptographically, but it does not satisfy local-only trust policy.
    pub signature_key_summary: SignatureKeySummary,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SignatureKeySummary {
    pub local_key_fingerprints: Vec<String>,
    pub peer_key_fingerprints: Vec<String>,
    pub hosted_runner_key_fingerprints: Vec<String>,
    pub third_party_key_fingerprints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignatureFinding {
    pub kind: SignatureFindingKind,
    pub subject_kind: String,
    pub subject_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureFindingKind {
    MissingSignature,
    InvalidSignature,
    DigestMismatch,
    SubjectMissing,
    MalformedSignature,
}

impl SignatureFindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SignatureFindingKind::MissingSignature => "missing_signature",
            SignatureFindingKind::InvalidSignature => "invalid_signature",
            SignatureFindingKind::DigestMismatch => "digest_mismatch",
            SignatureFindingKind::SubjectMissing => "subject_missing",
            SignatureFindingKind::MalformedSignature => "malformed_signature",
        }
    }
}

/// One row that failed integrity verification in `doctor`'s chain pass. Carries only
/// an opaque id, the table, and a closed-enum break kind — never an excerpt or
/// command string (this is a machine-visible egress). `kind` serializes as snake_case
/// (`content_edit`/`broken_link`/`missing_hash`).
#[derive(Debug, Clone, Serialize)]
pub struct TamperedRow {
    pub id: String,
    pub table: String,
    pub kind: TamperKind,
}

/// One native-history integrity break found by `doctor`'s commit-DAG walk (NER-138 Phase 7
/// slice 3). Carries only the closed-enum `kind` and opaque `f1:` commit ids — never a path or
/// excerpt. `kind` serializes the SAME way as [`ForgeError::NativeHistoryCorrupt`]'s `details`
/// (the shared [`NativeHistoryCorruptKind`]), so the error payload and the doctor report can
/// never disagree on the break-kind string. Empty in a healthy repo.
#[derive(Debug, Clone, Serialize)]
pub struct NativeHistoryFinding {
    pub kind: NativeHistoryCorruptKind,
    pub commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LedgerViewFinding {
    pub kind: LedgerViewFindingKind,
    pub view_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerViewFindingKind {
    CorruptStateJson,
    UnparseableCommitId,
}

/// A contract-family op in the ledger whose referenced domain row no longer exists
/// (F5). Unlike evidence/decision doctoring — which recomputes each row's digest
/// from the LIVE row — the contract family recovers each op's folded digest from the
/// FROZEN op `state_json`, so a `broken_link` never fires when a row is deleted; and
/// the signature pass's `SubjectMissing` only fires from `ledger_signatures`, so
/// co-deleting a row AND its signature rows leaves both passes green. This
/// cross-check closes that hole: every contract op names its subject row id in its
/// `state_json`, so a missing row is a tamper finding. Carries only opaque ids and a
/// closed table label — never row content. Empty in a healthy repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContractRowFinding {
    pub kind: ContractRowFindingKind,
    /// The domain table the op referenced (`contract_revisions`/`contract_runs`/
    /// `contract_stops`).
    pub table: String,
    /// The opaque row id (or `contract_id@revision` for a revision) the op names.
    pub subject_id: String,
    pub operation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractRowFindingKind {
    /// A signed contract-family op references a domain row that has been deleted.
    ReferencedRowMissing,
}

pub fn doctor(cwd: &Path) -> Result<DoctorReport> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let storage = storage::storage_accounting_for_root(&context.root_path)?;
    let storage_policy = storage::storage_policy(&connection)?;
    let storage_budget = storage::storage_budget_status_for(&storage, &storage_policy);
    let mut issues = Vec::new();
    let mut foreign_key_statement = connection.prepare("PRAGMA foreign_key_check")?;
    let mut foreign_key_rows = foreign_key_statement.query([])?;
    while let Some(row) = foreign_key_rows.next()? {
        let table: String = row.get(0)?;
        let rowid: i64 = row.get(1)?;
        issues.push(format!("foreign key violation in {table} row {rowid}"));
    }
    let schema_version = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    if schema_version != Some(migrations::schema_head()) {
        issues.push("schema mismatch".to_string());
    }
    let state_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM current_state cs
         JOIN operations o ON o.id = cs.current_operation_id
         JOIN views v ON v.id = cs.current_view_id
         WHERE cs.singleton = 1
           AND o.repo_id = cs.repo_id
           AND v.repo_id = cs.repo_id
           AND v.operation_id = o.id
           AND o.resulting_view_id = v.id",
        [],
        |row| row.get(0),
    )?;
    if state_count != 1 {
        issues.push("invalid current operation/view".to_string());
    }
    let mut dangling_temp_files = Vec::new();
    let temp_dir = context.root_path.join(".forge/tmp");
    if temp_dir.exists() {
        for entry in fs::read_dir(&temp_dir)? {
            let entry = entry?;
            dangling_temp_files.push(entry.path().to_string_lossy().into_owned());
        }
    }
    if !dangling_temp_files.is_empty() {
        issues.push("dangling temporary files".to_string());
    }
    let native_store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let mut reachable_native_objects = std::collections::BTreeSet::new();
    // A committed `content_ref` whose object is missing or fails verification is
    // the exact failure the store-before-DB ordering (NER-132) makes impossible;
    // surface it as its own category so the exit criterion is machine-checkable.
    let mut dangling_content_refs = Vec::new();
    let mut statement = connection.prepare(
        "SELECT 'snapshot ' || id, content_ref FROM snapshots
         UNION ALL
         SELECT 'proposal revision ' || id, content_ref FROM proposal_revisions",
    )?;
    let refs = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for content_ref in refs {
        let (label, content_ref) = content_ref?;
        if let Some(tree) = content_ref.strip_prefix("git-tree:") {
            let output = Command::new("git")
                .args(["cat-file", "-e", &format!("{tree}^{{tree}}")])
                .current_dir(&context.root_path)
                .output()?;
            if !output.status.success() {
                dangling_content_refs.push(format!("missing content ref for {label}"));
            }
        } else if content_ref.starts_with("forge-tree:") {
            match native_store.verify_content_ref(&content_ref) {
                Ok(ids) => reachable_native_objects.extend(ids),
                Err(error) => {
                    dangling_content_refs
                        .push(format!("invalid native content ref for {label}: {error}"));
                }
            }
        }
    }
    issues.extend(dangling_content_refs.iter().cloned());

    // A crash-atomic restore (NER-132 U4) killed mid-flight leaves a
    // `.forge-restore-*` temp in a worktree directory; those live in the worktree,
    // not `.forge/tmp`, so scan the work tree (excluding `.git`/`.forge`) for them.
    let half_applied_worktrees = scan_restore_temps(&context.root_path)?;
    if !half_applied_worktrees.is_empty() {
        issues.push("half-applied worktree (leftover restore temp files)".to_string());
    }

    // Tamper-evidence chain pass (NER-136): re-verify every hashed row offline.
    let tampered_rows = verify_integrity_chain(&connection)?;
    if !tampered_rows.is_empty() {
        issues.push(format!("{} tampered row(s) detected", tampered_rows.len()));
    }
    let signature_issues = signing::verify_signatures(&connection)?;
    if !signature_issues.is_empty() {
        issues.push(format!(
            "{} local signature issue(s) detected",
            signature_issues.len()
        ));
    }
    let signature_key_summary = signing::signature_key_summary(&connection, &context.repo_id)?;

    // Native commit-DAG integrity pass (NER-138 Phase 7 slice 3): walk the DAG from the
    // authoritative tip and cross-check the ledger, REPORTING (not raising) cycles / dangling
    // parents / dangling trees / dangling commit_ids. The raising counterpart lives in
    // reconcile/checkout; doctor is the offline health report.
    let native_history_issues = verify_native_history(&context, &connection, &native_store)?;
    if !native_history_issues.is_empty() {
        issues.push(format!(
            "{} native-history integrity break(s) detected",
            native_history_issues.len()
        ));
    }
    let ledger_view_issues = ledger_commit_roots(&context, &connection)?.view_issues;
    if !ledger_view_issues.is_empty() {
        issues.push(format!(
            "{} corrupt ledger view row(s) detected",
            ledger_view_issues.len()
        ));
    }
    // F5: contract-family ops recover their folded digest from the FROZEN op
    // state_json, so a deleted domain row + its deleted signature rows evade both the
    // op-chain and signature passes. Cross-check that every contract op's referenced
    // row still exists.
    let contract_row_issues =
        contract_doctor::contract_referenced_row_issues(&connection, &context.repo_id)?;
    if !contract_row_issues.is_empty() {
        issues.push(format!(
            "{} contract op(s) reference a deleted row",
            contract_row_issues.len()
        ));
    }
    let native_pack_issues = native_store.validate_packs();
    if !native_pack_issues.is_empty() {
        issues.push(format!(
            "{} native pack/index issue(s) detected",
            native_pack_issues.len()
        ));
    }
    let warnings = doctor_warnings(&context.root_path)?;

    Ok(DoctorReport {
        ok: issues.is_empty(),
        issues,
        warnings,
        schema_version,
        storage,
        storage_policy,
        storage_budget,
        dangling_temp_files,
        dangling_content_refs,
        half_applied_worktrees,
        tampered_rows,
        native_history_issues,
        ledger_view_issues,
        contract_row_issues,
        native_pack_issues,
        signature_issues,
        signature_key_summary,
    })
}

fn doctor_warnings(root: &Path) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if js_test_runner_may_scan_forge_worktrees(root)? {
        warnings.push(
            "JavaScript/TypeScript test discovery may scan Forge-managed worktrees; add `.forge/**` to test-runner excludes (for example Vitest `exclude: [...configDefaults.exclude, '.forge/**']`).".to_string(),
        );
    }
    Ok(warnings)
}

fn js_test_runner_may_scan_forge_worktrees(root: &Path) -> Result<bool> {
    let has_package_test_script = package_json_has_test_script(root)?;
    let mut relevant_files = js_test_config_files(root);
    if has_package_test_script {
        relevant_files.push(root.join("package.json"));
    }
    if relevant_files.is_empty() {
        return Ok(false);
    }
    for path in relevant_files {
        if file_mentions_forge_exclude(&path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn package_json_has_test_script(root: &Path) -> Result<bool> {
    let path = root.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let text =
        fs::read_to_string(&path).with_context(|| format!("read {}", path.to_string_lossy()))?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(Value::as_str)
        .is_some())
}

fn js_test_config_files(root: &Path) -> Vec<PathBuf> {
    [
        "vite.config.js",
        "vite.config.cjs",
        "vite.config.mjs",
        "vite.config.ts",
        "vite.config.cts",
        "vite.config.mts",
        "vitest.config.js",
        "vitest.config.cjs",
        "vitest.config.mjs",
        "vitest.config.ts",
        "vitest.config.cts",
        "vitest.config.mts",
        "jest.config.js",
        "jest.config.cjs",
        "jest.config.mjs",
        "jest.config.ts",
        "playwright.config.js",
        "playwright.config.cjs",
        "playwright.config.mjs",
        "playwright.config.ts",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .filter(|path| path.exists())
    .collect()
}

fn file_mentions_forge_exclude(path: &Path) -> Result<bool> {
    const MAX_CONFIG_BYTES: u64 = 1_048_576;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Ok(false);
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("read {}", path.to_string_lossy()))?;
    Ok(text.contains(".forge/**")
        || text.contains(".forge/")
        || text.contains("'.forge'")
        || text.contains("\".forge\""))
}

/// `doctor`'s native commit-DAG integrity pass (NER-138 Phase 7 slice 3): walk the DAG from
/// the authoritative tip detecting cycles (visited set), dangling parents, and dangling trees,
/// then cross-check every ledger-referenced native commit resolves to an existing commit object.
/// Reports findings (does not raise) — fail-closed at the call sites that raise. Findings are
/// deduped by (kind, commit_id, related_id).
fn verify_native_history(
    context: &RepositoryContext,
    connection: &Connection,
    store: &forge_content_native::NativeObjectStore,
) -> Result<Vec<NativeHistoryFinding>> {
    verify_native_history_from_tip(
        &context.repo_id,
        connection,
        store,
        native_tip(context, connection)?,
    )
}

fn verify_native_history_from_tip(
    repo_id: &str,
    connection: &Connection,
    store: &forge_content_native::NativeObjectStore,
    tip: Option<forge_content_native::ObjectId>,
) -> Result<Vec<NativeHistoryFinding>> {
    let mut findings: Vec<NativeHistoryFinding> = Vec::new();
    let mut push = |finding: NativeHistoryFinding| {
        if !findings.iter().any(|existing| {
            existing.kind.as_str() == finding.kind.as_str()
                && existing.commit_id == finding.commit_id
                && existing.related_id == finding.related_id
        }) {
            findings.push(finding);
        }
    };

    if let Some(tip) = tip {
        let mut states = std::collections::BTreeMap::new();
        let mut stack = vec![(tip, false)];
        while let Some((commit_id, expanded)) = stack.pop() {
            if expanded {
                states.insert(commit_id, NativeVisitState::Visited);
                continue;
            }
            match states.get(&commit_id).copied() {
                Some(NativeVisitState::Visited) => continue,
                Some(NativeVisitState::Visiting) => {
                    push(NativeHistoryFinding {
                        kind: NativeHistoryCorruptKind::Cycle,
                        commit_id: commit_id.to_string(),
                        related_id: None,
                    });
                    continue;
                }
                None => {}
            }
            states.insert(commit_id.clone(), NativeVisitState::Visiting);
            let commit = match store.read_commit(&commit_id) {
                Ok(commit) => commit,
                Err(_) => {
                    push(NativeHistoryFinding {
                        kind: NativeHistoryCorruptKind::DanglingCommitId,
                        commit_id: commit_id.to_string(),
                        related_id: None,
                    });
                    continue;
                }
            };
            stack.push((commit_id.clone(), true));
            let tree_ref = format!("{}{}", forge_content::FORGE_TREE_PREFIX, commit.tree);
            if store.verify_content_ref(&tree_ref).is_err() {
                push(NativeHistoryFinding {
                    kind: NativeHistoryCorruptKind::DanglingTree,
                    commit_id: commit_id.to_string(),
                    related_id: Some(commit.tree.clone()),
                });
            }
            for parent in &commit.parents {
                match forge_content_native::ObjectId::parse(parent) {
                    Ok(parent_id) if store.read_commit(&parent_id).is_ok() => {
                        stack.push((parent_id, false));
                    }
                    _ => push(NativeHistoryFinding {
                        kind: NativeHistoryCorruptKind::DanglingParent,
                        commit_id: commit_id.to_string(),
                        related_id: Some(parent.clone()),
                    }),
                }
            }
        }
    }

    let mut check_commit =
        |commit_id: String| match forge_content_native::ObjectId::parse(&commit_id) {
            Ok(id) if store.read_commit(&id).is_ok() => {}
            _ => push(NativeHistoryFinding {
                kind: NativeHistoryCorruptKind::DanglingCommitId,
                commit_id,
                related_id: None,
            }),
        };

    // Cross-check every accepted decisions.commit_id resolves to a commit object — catches a
    // dangling commit_id even if it is off the tip's ancestry (the store-before-DB violation).
    let mut decision_commits = connection
        .prepare("SELECT commit_id FROM decisions WHERE repo_id = ?1 AND commit_id IS NOT NULL")?;
    let rows = decision_commits.query_map(params![repo_id], |row| row.get::<_, String>(0))?;
    for row in rows {
        check_commit(row?);
    }

    // Sync merge operations also introduce native commit ids. Check them independently so an
    // imported or corrupted off-tip sync_*_merged view cannot claim a phantom signed commit.
    let query = format!(
        "SELECT json_extract(v.state_json, '$.commit_id')
           FROM operations o
           JOIN views v ON v.id = o.resulting_view_id
          WHERE o.repo_id = ?1
            AND o.kind IN ({})
            AND json_extract(v.state_json, '$.commit_id') IS NOT NULL",
        sync::SYNC_MERGED_OP_KIND_SQL_IN
    );
    let mut sync_merge_commits = connection.prepare(&query)?;
    let rows = sync_merge_commits.query_map(params![repo_id], |row| row.get::<_, String>(0))?;
    for row in rows {
        check_commit(row?);
    }

    Ok(findings)
}

/// Re-verify the full tamper-evident chain offline (NER-136 §U8): every evidence and
/// decision row's own content hash, plus every operation's chain link (which folds
/// the domain digest, so a *recomputed* row hash that slipped past the cheap gate
/// check is caught here at the operation that chained the old digest). Reads a
/// consistent ordered snapshot; a head-truncated chain (a lost latest op) is reported
/// as clean, NOT a tamper, because there is no expected-count check.
fn verify_integrity_chain(conn: &Connection) -> Result<Vec<TamperedRow>> {
    let mut tampered = Vec::new();
    let evidence_marker = evidence_high_water(conn)?;
    let op_marker = op_high_water(conn)?;
    let decision_marker = decision_high_water(conn)?;

    // (a) Every evidence row's own digest.
    let mut evidence_ids = conn.prepare("SELECT id FROM evidence ORDER BY rowid")?;
    let ids: Vec<String> = evidence_ids
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in ids {
        if let IntegrityStatus::Tampered(kind) =
            verify_evidence_integrity(conn, &id, evidence_marker)?
        {
            tampered.push(TamperedRow {
                id,
                table: "evidence".to_string(),
                kind,
            });
        }
    }

    // (b) Every decision row's own digest.
    let mut decision_rows = conn.prepare(
        "SELECT id, proposal_id, proposal_revision_id, decision, actor, content_hash, created_at_ms, rowid
         FROM decisions ORDER BY rowid",
    )?;
    let decisions: Vec<StoredDecision> = decision_rows
        .query_map([], |row| {
            Ok(StoredDecision {
                id: row.get(0)?,
                proposal_id: row.get(1)?,
                proposal_revision_id: row.get(2)?,
                decision: row.get(3)?,
                actor: row.get(4)?,
                content_hash: row.get(5)?,
                created_at_ms: row.get(6)?,
                rowid: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for row in decisions {
        match row.content_hash {
            None if row.rowid > decision_marker => tampered.push(TamperedRow {
                id: row.id,
                table: "decisions".to_string(),
                kind: TamperKind::MissingHash,
            }),
            None => {}
            Some(stored) => {
                let recomputed = integrity::decision_digest(&integrity::DecisionDigestInput {
                    proposal_id: &row.proposal_id,
                    proposal_revision_id: &row.proposal_revision_id,
                    decision: &row.decision,
                    actor: &row.actor,
                    created_at_ms: row.created_at_ms,
                });
                if recomputed != stored {
                    tampered.push(TamperedRow {
                        id: row.id,
                        table: "decisions".to_string(),
                        kind: TamperKind::ContentEdit,
                    });
                }
            }
        }
    }

    // (c) Every operation-owned conflict set's own digest.
    let mut conflict_rows = conn.prepare(
        "SELECT id FROM conflict_sets
         WHERE generated_by_operation_id IS NOT NULL
         ORDER BY rowid",
    )?;
    let conflict_ids: Vec<String> = conflict_rows
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in conflict_ids {
        match recompute_conflict_set_hash(conn, &id)? {
            None => tampered.push(TamperedRow {
                id,
                table: "conflict_sets".to_string(),
                kind: TamperKind::MissingHash,
            }),
            Some((stored, recomputed)) if stored != recomputed => tampered.push(TamperedRow {
                id,
                table: "conflict_sets".to_string(),
                kind: TamperKind::ContentEdit,
            }),
            Some(_) => {}
        }
    }

    // (d) Every operation's chain link (folding its domain digest), in chain order.
    let mut op_rows = conn.prepare(
        "SELECT id, parent_operation_id, command, kind, resulting_view_id, content_hash, created_at_ms, rowid
         FROM operations ORDER BY created_at_ms, rowid",
    )?;
    let ops: Vec<StoredOp> = op_rows
        .query_map([], |row| {
            Ok(StoredOp {
                id: row.get(0)?,
                parent_operation_id: row.get(1)?,
                command: row.get(2)?,
                kind: row.get(3)?,
                resulting_view_id: row.get(4)?,
                content_hash: row.get(5)?,
                created_at_ms: row.get(6)?,
                rowid: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    for row in ops {
        let Some(stored) = row.content_hash else {
            if row.rowid > op_marker {
                tampered.push(TamperedRow {
                    id: row.id,
                    table: "operations".to_string(),
                    kind: TamperKind::MissingHash,
                });
            }
            continue;
        };
        let parent_hash = op_content_hash(conn, row.parent_operation_id.as_deref())?;
        let domain_digest = op_domain_digest(conn, row.resulting_view_id.as_deref())?;
        let recomputed = integrity::operation_link_hash(
            &parent_hash,
            &integrity::OperationDigestInput {
                operation_id: &row.id,
                command: &row.command,
                kind: &row.kind,
                created_at_ms: row.created_at_ms,
            },
            domain_digest.as_deref(),
        );
        if recomputed != stored {
            tampered.push(TamperedRow {
                id: row.id,
                table: "operations".to_string(),
                kind: TamperKind::BrokenLink,
            });
        }
    }

    Ok(tampered)
}

fn recompute_conflict_set_hash(
    conn: &Connection,
    conflict_set_id: &str,
) -> Result<Option<(String, String)>> {
    let conflict: Option<StoredConflictSet> = conn
        .query_row(
            "SELECT id, repo_id, context, paths_json, created_at_ms, base_content_ref,
                    ours_content_ref, theirs_content_ref, generated_by_operation_id,
                    resolver_backend, content_hash
             FROM conflict_sets WHERE id = ?1",
            params![conflict_set_id],
            |row| {
                Ok(StoredConflictSet {
                    id: row.get(0)?,
                    repo_id: row.get(1)?,
                    context: row.get(2)?,
                    paths_json: row.get(3)?,
                    created_at_ms: row.get(4)?,
                    base_content_ref: row.get(5)?,
                    ours_content_ref: row.get(6)?,
                    theirs_content_ref: row.get(7)?,
                    generated_by_operation_id: row.get(8)?,
                    resolver_backend: row.get(9)?,
                    content_hash: row.get(10)?,
                })
            },
        )
        .optional()?;
    let Some(conflict) = conflict else {
        return Ok(None);
    };
    let Some(stored) = conflict.content_hash.clone() else {
        return Ok(None);
    };
    let mut path_stmt = conn.prepare(
        "SELECT id, path, path_fingerprint, base_path, ours_path, theirs_path, kind,
                base_ref, ours_ref, theirs_ref, base_status, ours_status, theirs_status,
                base_mode, ours_mode, theirs_mode, created_at_ms
         FROM path_conflicts WHERE conflict_set_id = ?1 ORDER BY rowid",
    )?;
    let path_rows: Vec<StoredPathConflict> = path_stmt
        .query_map(params![conflict_set_id], |row| {
            Ok(StoredPathConflict {
                id: row.get(0)?,
                path: row.get(1)?,
                path_fingerprint: row.get(2)?,
                base_path: row.get(3)?,
                ours_path: row.get(4)?,
                theirs_path: row.get(5)?,
                kind: row.get(6)?,
                base_ref: row.get(7)?,
                ours_ref: row.get(8)?,
                theirs_ref: row.get(9)?,
                base_status: row.get(10)?,
                ours_status: row.get(11)?,
                theirs_status: row.get(12)?,
                base_mode: row.get(13)?,
                ours_mode: row.get(14)?,
                theirs_mode: row.get(15)?,
                created_at_ms: row.get(16)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let digest_rows = path_rows
        .iter()
        .map(StoredPathConflict::immutable_digest_input)
        .collect::<Vec<_>>();
    let recomputed = integrity::conflict_set_digest(&integrity::ConflictSetDigestInput {
        id: &conflict.id,
        repo_id: &conflict.repo_id,
        context: &conflict.context,
        paths_json: &conflict.paths_json,
        base_content_ref: conflict.base_content_ref.as_deref(),
        ours_content_ref: conflict.ours_content_ref.as_deref(),
        theirs_content_ref: conflict.theirs_content_ref.as_deref(),
        generated_by_operation_id: conflict.generated_by_operation_id.as_deref(),
        resolver_backend: conflict.resolver_backend.as_deref(),
        status: "unresolved",
        created_at_ms: conflict.created_at_ms,
        path_conflicts: &digest_rows,
    });
    Ok(Some((stored, recomputed)))
}

struct StoredConflictSet {
    id: String,
    repo_id: String,
    context: String,
    paths_json: String,
    created_at_ms: i64,
    base_content_ref: Option<String>,
    ours_content_ref: Option<String>,
    theirs_content_ref: Option<String>,
    generated_by_operation_id: Option<String>,
    resolver_backend: Option<String>,
    content_hash: Option<String>,
}

struct StoredPathConflict {
    id: String,
    path: String,
    path_fingerprint: String,
    base_path: Option<String>,
    ours_path: Option<String>,
    theirs_path: Option<String>,
    kind: String,
    base_ref: Option<String>,
    ours_ref: Option<String>,
    theirs_ref: Option<String>,
    base_status: Option<String>,
    ours_status: Option<String>,
    theirs_status: Option<String>,
    base_mode: Option<String>,
    ours_mode: Option<String>,
    theirs_mode: Option<String>,
    created_at_ms: i64,
}

impl StoredPathConflict {
    fn immutable_digest_input(&self) -> integrity::PathConflictDigestInput<'_> {
        integrity::PathConflictDigestInput {
            id: &self.id,
            path: &self.path,
            path_fingerprint: &self.path_fingerprint,
            base_path: self.base_path.as_deref(),
            ours_path: self.ours_path.as_deref(),
            theirs_path: self.theirs_path.as_deref(),
            kind: &self.kind,
            base_ref: self.base_ref.as_deref(),
            ours_ref: self.ours_ref.as_deref(),
            theirs_ref: self.theirs_ref.as_deref(),
            base_status: self.base_status.as_deref(),
            ours_status: self.ours_status.as_deref(),
            theirs_status: self.theirs_status.as_deref(),
            base_mode: self.base_mode.as_deref(),
            ours_mode: self.ours_mode.as_deref(),
            theirs_mode: self.theirs_mode.as_deref(),
            resolution_ref: None,
            status: "unresolved",
            created_at_ms: self.created_at_ms,
        }
    }
}

/// A decision row read back for `doctor`'s chain pass.
struct StoredDecision {
    id: String,
    proposal_id: String,
    proposal_revision_id: String,
    decision: String,
    actor: String,
    content_hash: Option<String>,
    created_at_ms: i64,
    rowid: i64,
}

/// An operation row read back for `doctor`'s chain re-walk.
struct StoredOp {
    id: String,
    parent_operation_id: Option<String>,
    command: String,
    kind: String,
    resulting_view_id: Option<String>,
    content_hash: Option<String>,
    created_at_ms: i64,
    rowid: i64,
}

#[derive(Debug, Deserialize)]
struct StoredSyncMergeState {
    lifecycle: String,
    #[allow(dead_code)]
    protocol_version: String,
    direction: String,
    remote_path: String,
    base_native_head: String,
    ours_native_head: String,
    theirs_native_head: String,
    merged_content_ref: String,
    commit_id: String,
    materialized: bool,
    imported_native_objects: i64,
    imported_ledger_rows: i64,
    sync_merge_lineage_hash: String,
}

/// The recorded `operations` rowid high-water mark (the legacy/tampered boundary for
/// operation rows).
fn op_high_water(conn: &Connection) -> Result<i64> {
    let mark: Option<i64> = conn
        .query_row(
            "SELECT op_high_water FROM integrity_marker WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(mark.unwrap_or(0))
}

/// The domain-row digest an operation folded into its chain link, recovered for the
/// `doctor` re-walk by reading the operation's view `state_json` for an `evidence_id`
/// or `decision_id` and returning that row's stored `content_hash`. `None` for
/// operations with no domain row (init, propose, attach, …).
fn op_domain_digest(conn: &Connection, view_id: Option<&str>) -> Result<Option<String>> {
    let Some(view_id) = view_id else {
        return Ok(None);
    };
    let state_json: Option<String> = conn
        .query_row(
            "SELECT state_json FROM views WHERE id = ?1",
            params![view_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(state_json) = state_json else {
        return Ok(None);
    };
    let Ok(state) = serde_json::from_str::<Value>(&state_json) else {
        return Ok(None);
    };
    if let Some(evidence_id) = state.get("evidence_id").and_then(Value::as_str) {
        return conn
            .query_row(
                "SELECT content_hash FROM evidence WHERE id = ?1",
                params![evidence_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(Into::into);
    }
    if let Some(decision_id) = state.get("decision_id").and_then(Value::as_str) {
        return conn
            .query_row(
                "SELECT content_hash FROM decisions WHERE id = ?1",
                params![decision_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(Into::into);
    }
    if state
        .get("lifecycle")
        .and_then(Value::as_str)
        .is_some_and(sync::is_sync_merged_op_kind)
    {
        let Ok(sync_state) = serde_json::from_value::<StoredSyncMergeState>(state.clone()) else {
            return Ok(None);
        };
        if !sync::is_sync_merged_op_kind(&sync_state.lifecycle) {
            return Ok(None);
        }
        let recomputed =
            integrity::sync_merge_lineage_digest(&integrity::SyncMergeLineageDigestInput {
                protocol_version: &sync_state.protocol_version,
                direction: &sync_state.direction,
                remote_path: &sync_state.remote_path,
                base_native_head: &sync_state.base_native_head,
                ours_native_head: &sync_state.ours_native_head,
                theirs_native_head: &sync_state.theirs_native_head,
                merged_content_ref: &sync_state.merged_content_ref,
                commit_id: &sync_state.commit_id,
                materialized: sync_state.materialized,
                imported_native_objects: sync_state.imported_native_objects,
                imported_ledger_rows: sync_state.imported_ledger_rows,
            });
        return Ok(Some(if sync_state.sync_merge_lineage_hash == recomputed {
            sync_state.sync_merge_lineage_hash
        } else {
            recomputed
        }));
    }
    if let Some(stored) = state.get("merge_lineage_hash").and_then(Value::as_str) {
        let recomputed = integrity::merge_lineage_digest(&integrity::MergeLineageDigestInput {
            proposal_id: state
                .get("proposal_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            proposal_revision_id: state
                .get("proposal_revision_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            snapshot_id: state
                .get("snapshot_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            base_head: state
                .get("base_head")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            ours_head: state
                .get("ours_head")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            base_content_ref: state
                .get("base_content_ref")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            ours_content_ref: state
                .get("ours_content_ref")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            theirs_content_ref: state
                .get("theirs_content_ref")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            merged_content_ref: state
                .get("merged_content_ref")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        });
        return Ok(Some(if recomputed == stored {
            stored.to_string()
        } else {
            recomputed
        }));
    }
    if let Some(conflict_set_id) = state.get("conflict_set_id").and_then(Value::as_str) {
        return conn
            .query_row(
                "SELECT content_hash FROM conflict_sets WHERE id = ?1",
                params![conflict_set_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
            .map_err(Into::into);
    }
    // Contract-family ops (U9/KTD2) record the EXACT domain digest they folded in
    // their view `subject_digest`. Recovering it from here (rather than recomputing
    // from the current row) is load-bearing for the mutable stop kind: a stop's row
    // `content_hash` is rewritten on resolve, so the stop-opened op's chain link can
    // only be re-verified against the digest it originally folded. Immutable kinds
    // record it too, keeping recovery uniform.
    if let Some(subject_digest) = state.get("subject_digest").and_then(Value::as_str) {
        return Ok(Some(subject_digest.to_string()));
    }
    Ok(None)
}

/// Recursively scan a work tree for leftover crash-atomic-restore temp files
/// (`forge_content_native::RESTORE_TEMP_PREFIX`), skipping `.git` and `.forge`.
/// A match is the signature of a restore killed mid-flight (NER-132 U4/U7).
fn scan_restore_temps(root: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // unreadable dir is not a half-applied-restore signal
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                // Never descend into git's or forge's own state — at ANY depth, so a
                // submodule's nested .git (a large, unbounded object tree) is skipped
                // too. Restore temps only land in worktree dirs forge materializes
                // into, never inside a git/forge store, so this loses no real signal.
                if name == ".git" || name == ".forge" {
                    continue;
                }
                stack.push(entry.path());
            } else if name.starts_with(forge_content_native::RESTORE_TEMP_PREFIX) {
                found.push(entry.path().to_string_lossy().into_owned());
            }
        }
    }
    found.sort();
    Ok(found)
}
