use super::*;

#[derive(Debug, Clone, Serialize)]
pub struct GcDryRunReport {
    pub dry_run: bool,
    pub unreachable_snapshots: Vec<String>,
    pub unreachable_evidence: Vec<String>,
    pub unreachable_native_objects: Vec<String>,
    pub protected_native_objects: Vec<String>,
    pub pack_candidate_native_objects: Vec<String>,
    pub loose_duplicate_native_objects: Vec<String>,
    pub deletable_native_packs: Vec<String>,
    pub protection_window_days: u64,
    pub storage: StorageAccounting,
    pub storage_policy: StoragePolicy,
    pub storage_budget: StorageBudgetStatus,
    pub plan_digest: String,
    pub deleted: Vec<String>,
    pub created_packs: Vec<String>,
    pub deleted_packs: Vec<String>,
}

pub fn gc_dry_run(cwd: &Path) -> Result<GcDryRunReport> {
    Ok(gc_plan(cwd)?.into_report(true, Vec::new(), Vec::new(), Vec::new()))
}

pub fn gc_delete(cwd: &Path, expected_plan_digest: &str) -> Result<GcDryRunReport> {
    let doctor_report = doctor(cwd)?;
    if !doctor_report.ok {
        bail!("gc refuses deletion while doctor reports repository issues");
    }
    let plan = gc_plan(cwd)?;
    if plan.plan_digest != expected_plan_digest {
        return Err(ForgeError::GcPlanChanged {
            expected_digest: expected_plan_digest.to_string(),
            actual_digest: plan.plan_digest.clone(),
        }
        .into());
    }
    let context = open_repository(cwd)?;
    let native_store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let mut deleted = Vec::new();
    let mut created_packs = Vec::new();
    let mut loose_deletions = plan.loose_duplicate_native_objects.clone();
    let pack_candidate_ids = parse_native_object_ids(&plan.pack_candidate_native_objects)?;
    if let Some(pack) = native_store.write_pack_from_loose_objects(&pack_candidate_ids)? {
        created_packs.push(pack.pack_id);
        loose_deletions.extend(pack.object_ids.into_iter().map(|id| id.to_string()));
        loose_deletions.sort();
        loose_deletions.dedup();
        forge_content::maybe_crash("gc_after_pack_before_loose_delete");
    }
    for object in &loose_deletions {
        let id = forge_content_native::ObjectId::parse(object)?;
        native_store.delete_loose_duplicate(&id)?;
        deleted.push(object.clone());
        forge_content::maybe_crash("gc_after_unlink");
    }
    let mut deleted_packs = Vec::new();
    for pack_id in &plan.deletable_native_packs {
        native_store.delete_pack(pack_id)?;
        deleted_packs.push(pack_id.clone());
        forge_content::maybe_crash("gc_after_unlink");
    }
    Ok(plan.into_report(false, deleted, created_packs, deleted_packs))
}

struct GcPlan {
    unreachable_native_objects: Vec<String>,
    protected_native_objects: Vec<String>,
    pack_candidate_native_objects: Vec<String>,
    loose_duplicate_native_objects: Vec<String>,
    deletable_native_packs: Vec<String>,
    storage: StorageAccounting,
    storage_policy: StoragePolicy,
    storage_budget: StorageBudgetStatus,
    protection_window_days: u64,
    plan_digest: String,
}

impl GcPlan {
    fn into_report(
        self,
        dry_run: bool,
        deleted: Vec<String>,
        created_packs: Vec<String>,
        deleted_packs: Vec<String>,
    ) -> GcDryRunReport {
        GcDryRunReport {
            dry_run,
            unreachable_snapshots: Vec::new(),
            unreachable_evidence: Vec::new(),
            unreachable_native_objects: self.unreachable_native_objects,
            protected_native_objects: self.protected_native_objects,
            pack_candidate_native_objects: self.pack_candidate_native_objects,
            loose_duplicate_native_objects: self.loose_duplicate_native_objects,
            deletable_native_packs: self.deletable_native_packs,
            protection_window_days: self.protection_window_days,
            storage: self.storage,
            storage_policy: self.storage_policy,
            storage_budget: self.storage_budget,
            plan_digest: self.plan_digest,
            deleted,
            created_packs,
            deleted_packs,
        }
    }
}

fn gc_plan(cwd: &Path) -> Result<GcPlan> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let storage = storage::storage_accounting_for_root(&context.root_path)?;
    let storage_policy = storage::storage_policy(&connection)?;
    let storage_budget = storage::storage_budget_status_for(&storage, &storage_policy);
    let protection_window = protection_window_duration(storage_policy.protection_window_days);
    let native_store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let mut reachable = std::collections::BTreeSet::new();
    let mut statement = connection.prepare(
        "SELECT content_ref FROM snapshots
         UNION
         SELECT content_ref FROM proposal_revisions
         UNION
         SELECT base_content_ref AS content_ref FROM conflict_sets
          WHERE resolver_backend = 'native_merge' AND base_content_ref IS NOT NULL
         UNION
         SELECT ours_content_ref AS content_ref FROM conflict_sets
          WHERE resolver_backend = 'native_merge' AND ours_content_ref IS NOT NULL
         UNION
         SELECT theirs_content_ref AS content_ref FROM conflict_sets
          WHERE resolver_backend = 'native_merge' AND theirs_content_ref IS NOT NULL
         UNION
         -- CCX contract runs (U5/KTD3): a completed run's produced-patch tree object
         -- and every per-task patch tree object are durable GC roots, walked here via
         -- verify_content_ref exactly like a snapshot/proposal content_ref. A new
         -- ObjectKind that is not a GC root is a data-loss bug. (U9 formalizes the
         -- two-sided doctor/gc test coverage.)
         SELECT patch_content_ref AS content_ref FROM contract_runs
          WHERE patch_content_ref IS NOT NULL
         UNION
         SELECT patch_content_ref AS content_ref FROM contract_run_tasks
          WHERE patch_content_ref IS NOT NULL",
    )?;
    let refs = statement.query_map([], |row| row.get::<_, String>(0))?;
    for content_ref in refs {
        let content_ref = content_ref?;
        if content_ref.starts_with("forge-tree:") {
            if let Ok(ids) = native_store.verify_content_ref(&content_ref) {
                reachable.extend(ids);
            }
        }
    }
    // NER-138 Phase 7 slice 3: seed reachability from the AUTHORITATIVE ledger tip (not only
    // the ref-store HEAD, which a lock-free, never-reconciled gc could read stale), plus every
    // accepted `decisions.commit_id` and every op-log-referenced commit (a `checkout` target
    // writes NO decision row, so its commit is reachable only through the op-log). Each is
    // walked as a DAG root (commit → ancestry → trees). Best-effort: a dangling root is
    // surfaced by `doctor`, not fatal to this dry-run report. No-op for git-backend repos.
    let roots = ledger_commit_roots(&context, &connection)?;
    if roots
        .view_issues
        .iter()
        .any(|finding| finding.kind == LedgerViewFindingKind::CorruptStateJson)
    {
        return Err(anyhow!(
            "gc cannot read a ledger view row (corrupt views.state_json); run `forge doctor`"
        ));
    }
    if roots
        .view_issues
        .iter()
        .any(|finding| finding.kind == LedgerViewFindingKind::UnparseableCommitId)
    {
        return Err(anyhow!(
            "gc found an unparseable reachability root in the ledger; run `forge doctor`"
        ));
    }
    for id in &roots.roots {
        if let Ok(ids) = native_store.reachable_from(id) {
            reachable.extend(ids);
        }
    }
    let loose = native_store.loose_object_ids()?;
    let packed_infos = native_store.packed_object_infos()?;
    let mut packed = std::collections::BTreeSet::new();
    let mut infos_by_pack: std::collections::BTreeMap<
        String,
        Vec<forge_content_native::PackedObjectInfo>,
    > = std::collections::BTreeMap::new();
    for info in packed_infos {
        packed.insert(info.object_id.clone());
        infos_by_pack
            .entry(info.pack_id.clone())
            .or_default()
            .push(info);
    }
    let mut all = loose.clone();
    all.extend(packed.iter().cloned());
    let now = SystemTime::now();
    let now_ms = system_time_ms(now).unwrap_or(u64::MAX);
    let mut unreachable_native_objects = Vec::new();
    let mut protected_native_objects = Vec::new();
    for id in all.difference(&reachable) {
        let rendered = id.to_string();
        unreachable_native_objects.push(rendered.clone());
        let protected = if loose.contains(id) {
            loose_object_protected(&native_store, id, now, protection_window)
        } else {
            infos_by_pack
                .values()
                .flatten()
                .filter(|info| &info.object_id == id)
                .all(|info| pack_entry_protected(info, now_ms, protection_window))
        };
        if protected {
            protected_native_objects.push(rendered);
        }
    }

    let mut pack_candidate_native_objects = Vec::new();
    for id in &loose {
        if !packed.contains(id)
            && !loose_object_protected(&native_store, id, now, protection_window)
        {
            pack_candidate_native_objects.push(id.to_string());
        }
    }

    let mut loose_duplicate_native_objects = Vec::new();
    for id in loose.intersection(&packed) {
        if native_store.has_verified_packed_object(id)? {
            loose_duplicate_native_objects.push(id.to_string());
        }
    }

    let mut deletable_native_packs = Vec::new();
    for (pack_id, infos) in &infos_by_pack {
        if !infos.is_empty()
            && infos.iter().all(|info| {
                !reachable.contains(&info.object_id)
                    && !pack_entry_protected(info, now_ms, protection_window)
            })
        {
            deletable_native_packs.push(pack_id.clone());
        }
    }

    let plan_digest = gc_plan_digest(
        &pack_candidate_native_objects,
        &loose_duplicate_native_objects,
        &deletable_native_packs,
        &protected_native_objects,
        storage_policy.protection_window_days,
    );
    Ok(GcPlan {
        unreachable_native_objects,
        protected_native_objects,
        pack_candidate_native_objects,
        loose_duplicate_native_objects,
        deletable_native_packs,
        storage,
        storage_policy: storage_policy.clone(),
        storage_budget,
        protection_window_days: storage_policy.protection_window_days,
        plan_digest,
    })
}

fn gc_plan_digest(
    pack_candidate_native_objects: &[String],
    loose_duplicate_native_objects: &[String],
    deletable_native_packs: &[String],
    protected_native_objects: &[String],
    protection_window_days: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"forge-gc-plan-v2\n");
    hasher.update(format!("protection_window_days={protection_window_days}\n"));
    for id in pack_candidate_native_objects {
        hasher.update(b"pack ");
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    for id in loose_duplicate_native_objects {
        hasher.update(b"delete-loose ");
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    for pack_id in deletable_native_packs {
        hasher.update(b"delete-pack ");
        hasher.update(pack_id.as_bytes());
        hasher.update(b"\n");
    }
    for id in protected_native_objects {
        hasher.update(b"protect ");
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn parse_native_object_ids(values: &[String]) -> Result<Vec<forge_content_native::ObjectId>> {
    values
        .iter()
        .map(|value| forge_content_native::ObjectId::parse(value))
        .collect()
}

fn loose_object_protected(
    native_store: &forge_content_native::NativeObjectStore,
    id: &forge_content_native::ObjectId,
    now: SystemTime,
    protection_window: Duration,
) -> bool {
    native_store
        .object_modified_time(id)
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_none_or(|age| age < protection_window)
}

fn pack_entry_protected(
    info: &forge_content_native::PackedObjectInfo,
    now_ms: u64,
    protection_window: Duration,
) -> bool {
    let Some(packed_at_ms) = info.packed_at_ms else {
        return true;
    };
    let Some(loose_mtime_ms) = info.loose_mtime_ms else {
        return true;
    };
    let newest_ms = packed_at_ms.max(loose_mtime_ms);
    let protection_ms: u64 = match protection_window.as_millis().try_into() {
        Ok(value) => value,
        Err(_) => return true,
    };
    now_ms.saturating_sub(newest_ms) < protection_ms
}

fn protection_window_duration(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(60 * 60 * 24))
}

fn system_time_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

pub(crate) struct LedgerCommitRoots {
    pub(crate) roots: std::collections::BTreeSet<forge_content_native::ObjectId>,
    pub(crate) view_issues: Vec<LedgerViewFinding>,
}

pub(crate) fn ledger_commit_roots(
    context: &RepositoryContext,
    connection: &Connection,
) -> Result<LedgerCommitRoots> {
    let mut root_strings: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut view_issues = Vec::new();
    if let Some(tip) = native_tip(context, connection)? {
        root_strings.insert(tip.to_string());
    }
    let mut decision_stmt = connection
        .prepare("SELECT commit_id FROM decisions WHERE repo_id = ?1 AND commit_id IS NOT NULL")?;
    for row in decision_stmt.query_map(params![context.repo_id], |row| row.get::<_, String>(0))? {
        root_strings.insert(row?);
    }
    let mut view_stmt = connection.prepare(
        "SELECT id, operation_id, state_json
         FROM views
         WHERE repo_id = ?1
         ORDER BY created_at_ms, rowid",
    )?;
    for row in view_stmt.query_map(params![context.repo_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (view_id, operation_id, state_json) = row?;
        let value: Value = match serde_json::from_str(&state_json) {
            Ok(value) => value,
            Err(_) => {
                view_issues.push(LedgerViewFinding {
                    kind: LedgerViewFindingKind::CorruptStateJson,
                    view_id,
                    operation_id,
                });
                continue;
            }
        };
        if let Some(commit_id) = value.get("commit_id").and_then(|value| value.as_str()) {
            if forge_content_native::ObjectId::parse(commit_id).is_err() {
                view_issues.push(LedgerViewFinding {
                    kind: LedgerViewFindingKind::UnparseableCommitId,
                    view_id,
                    operation_id,
                });
                continue;
            }
            root_strings.insert(commit_id.to_string());
        }
    }
    let mut roots = std::collections::BTreeSet::new();
    for root in root_strings {
        let id = forge_content_native::ObjectId::parse(&root).map_err(|_| {
            anyhow!("gc found an unparseable reachability root in the ledger; run `forge doctor`")
        })?;
        roots.insert(id);
    }
    Ok(LedgerCommitRoots { roots, view_issues })
}
