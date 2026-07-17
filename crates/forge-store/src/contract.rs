//! CCX native contracts (NER U1): store foundation for the contract, run, stop,
//! and verdict ledger object kinds (migration 022).
//!
//! Each kind is inserted, queried, content-hashed, signed, and chained here so
//! later CLI units (lint/freeze, run, verify, triage) reuse a single signed,
//! tamper-evident substrate. The shapes and discipline mirror the working
//! precedents:
//! - `crate::embargo` — a full new object family added as one migration + one
//!   domain module (table shapes, lifecycle-state CHECK constraints, `_on`
//!   query helpers).
//! - `crate::evidence` — sign-then-chain: compute a content hash inside the
//!   `BEGIN IMMEDIATE` txn, `signer.sign_subject(..)` under a per-kind
//!   `subject_kind`, then fold that digest into the op-log spine via
//!   `insert_operation_view_chained` so `forge doctor`'s re-walk catches a later
//!   content-hash swap.
//!
//! Redaction (KTD3): agent-authored free text — the stop record's four fields —
//! passes `forge_content::redact_evidence_excerpt` BEFORE it is hashed and
//! signed, inside the same transaction, so agent content never enters an
//! append-only, signed record unredacted.
//!
//! Request-id replay (R18): mutating functions re-check `replay_guard` as the
//! first statement inside the IMMEDIATE txn, mirroring `record_evidence`.

use super::*;

/// Domain-separation tag prefix for a contract-family content hash. Each kind
/// appends its own suffix so a revision digest can never collide with a run,
/// stop, or verdict digest (the same TupleHash discipline as `integrity.rs`).
const CONTRACT_DIGEST_DOMAIN: &str = "forge.contract.v0";

/// The per-kind signing `subject_kind` strings. Kept as constants so the later
/// `doctor` / `expected_signed_subjects` extension (U9/KTD2) references the same
/// literals the writes use.
pub const SUBJECT_KIND_CONTRACT: &str = "contract";
pub const SUBJECT_KIND_CONTRACT_RUN: &str = "contract_run";
pub const SUBJECT_KIND_CONTRACT_STOP: &str = "contract_stop";
pub const SUBJECT_KIND_CONTRACT_VERDICT: &str = "contract_run_verdict";

/// A length-prefixed, domain-separated SHA-256 content-hash builder for the
/// contract family. Same injective encoding as `integrity::DigestWriter` (each
/// field is a little-endian `u64` length followed by the bytes; `Option` writes
/// a 1-byte presence tag so `None` != `Some("")`), kept local so U1 stays within
/// its declared file boundary (integrity.rs is not a U1 file).
struct ContractDigest {
    hasher: Sha256,
}

impl ContractDigest {
    fn new(kind: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CONTRACT_DIGEST_DOMAIN.as_bytes());
        hasher.update([0u8]);
        hasher.update(kind.as_bytes());
        hasher.update([0u8]);
        Self { hasher }
    }

    fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.hasher.update((value.len() as u64).to_le_bytes());
        self.hasher.update(value);
        self
    }

    fn str(&mut self, value: &str) -> &mut Self {
        self.bytes(value.as_bytes())
    }

    fn i64(&mut self, value: i64) -> &mut Self {
        self.bytes(&value.to_le_bytes())
    }

    fn bool(&mut self, value: bool) -> &mut Self {
        self.bytes(&[u8::from(value)])
    }

    fn opt_str(&mut self, value: Option<&str>) -> &mut Self {
        match value {
            Some(inner) => {
                self.hasher.update([1u8]);
                self.str(inner);
            }
            None => {
                self.hasher.update([0u8]);
            }
        }
        self
    }

    fn opt_i64(&mut self, value: Option<i64>) -> &mut Self {
        match value {
            Some(inner) => {
                self.hasher.update([1u8]);
                self.i64(inner);
            }
            None => {
                self.hasher.update([0u8]);
            }
        }
        self
    }

    fn finish(self) -> String {
        let digest = self.hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }
}

// ---------------------------------------------------------------------------
// Contracts and frozen revisions
// ---------------------------------------------------------------------------

/// A frozen, immutable contract revision as stored in the ledger. `source_yaml`
/// is the exact authored bytes, verbatim (R1).
#[derive(Debug, Clone, Serialize)]
pub struct ContractRevisionRecord {
    pub revision_row_id: String,
    pub contract_id: String,
    pub revision: i64,
    pub state: String,
    pub source_yaml: String,
    pub lint_clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predecessor_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_rationale: Option<String>,
    pub content_hash: String,
    pub created_at_ms: i64,
}

/// Inputs to freeze a new contract revision. A revision bump (`resolution_kind`
/// set, or simply a second freeze) records `predecessor_revision` automatically.
#[derive(Debug, Clone)]
pub struct FreezeContractRevisionInput {
    pub contract_id: String,
    /// Exact source YAML bytes, stored verbatim (R1).
    pub source_yaml: String,
    pub lint_clean: bool,
    /// R10: a stop-driven bump records whether it changed content (`revision`) or
    /// was a content-preserving explicit rejection (`rejection`) plus rationale.
    pub resolution_kind: Option<String>,
    pub resolution_rationale: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn contract_revision_digest(
    contract_id: &str,
    revision: i64,
    state: &str,
    source_yaml: &str,
    lint_clean: bool,
    predecessor_revision: Option<i64>,
    resolution_kind: Option<&str>,
    resolution_rationale: Option<&str>,
    created_at_ms: i64,
) -> String {
    let mut digest = ContractDigest::new(SUBJECT_KIND_CONTRACT);
    digest
        .str(contract_id)
        .i64(revision)
        .str(state)
        .str(source_yaml)
        .bool(lint_clean)
        .opt_i64(predecessor_revision)
        .opt_str(resolution_kind)
        .opt_str(resolution_rationale)
        .i64(created_at_ms);
    digest.finish()
}

/// Freeze a new, immutable, signed contract revision (R1/R2/R17). Creates the
/// `contracts` head row on first freeze; each freeze bumps `latest_revision`,
/// records `predecessor_revision`, signs under `"contract"`, and chains through
/// the op-log spine. `--request-id` replay-safe (R18).
pub fn freeze_contract_revision(
    cwd: &Path,
    request_id: Option<String>,
    input: FreezeContractRevisionInput,
) -> Result<ContractRevisionRecord> {
    if input.contract_id.trim().is_empty() {
        bail!("contract id must not be empty");
    }
    if let Some(kind) = input.resolution_kind.as_deref() {
        if !matches!(kind, "revision" | "rejection") {
            bail!("unsupported contract resolution kind `{kind}`");
        }
    }
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let latest_revision: Option<i64> = tx
            .query_row(
                "SELECT latest_revision FROM contracts WHERE repo_id = ?1 AND contract_id = ?2",
                params![context.repo_id, input.contract_id],
                |row| row.get(0),
            )
            .optional()?;
        let predecessor_revision = latest_revision.filter(|value| *value > 0);
        let revision = latest_revision.unwrap_or(0) + 1;
        // Upsert the head pointer.
        tx.execute(
            "INSERT INTO contracts (repo_id, contract_id, latest_revision, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(repo_id, contract_id)
             DO UPDATE SET latest_revision = excluded.latest_revision, updated_at_ms = excluded.updated_at_ms",
            params![context.repo_id, input.contract_id, revision, now],
        )?;
        let revision_row_id = new_id("contract_rev");
        let state = "frozen";
        let content_hash = contract_revision_digest(
            &input.contract_id,
            revision,
            state,
            &input.source_yaml,
            input.lint_clean,
            predecessor_revision,
            input.resolution_kind.as_deref(),
            input.resolution_rationale.as_deref(),
            now,
        );
        tx.execute(
            "INSERT INTO contract_revisions (
                id, repo_id, contract_id, revision, state, source_yaml, lint_clean,
                predecessor_revision, resolution_kind, resolution_rationale, content_hash, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                revision_row_id,
                context.repo_id,
                input.contract_id,
                revision,
                state,
                input.source_yaml,
                input.lint_clean as i64,
                predecessor_revision,
                input.resolution_kind,
                input.resolution_rationale,
                content_hash,
                now,
            ],
        )?;
        signer.sign_subject(
            tx,
            &context.repo_id,
            SUBJECT_KIND_CONTRACT,
            &revision_row_id,
            &content_hash,
            now,
        )?;
        insert_operation_view_chained(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                // Must equal the CLI command string so `command_result`'s pre-flight
                // replay (`replay_response`) folds a same-request-id retry to the
                // original result instead of raising REQUEST_ID_CONFLICT.
                command: "contract freeze".to_string(),
                kind: "contract_frozen".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({
                    "lifecycle": "contract_frozen",
                    "contract_id": input.contract_id,
                    "revision": revision,
                }),
            },
            Some(&content_hash),
        )?;
        Ok(ContractRevisionRecord {
            revision_row_id,
            contract_id: input.contract_id.clone(),
            revision,
            state: state.to_string(),
            source_yaml: input.source_yaml.clone(),
            lint_clean: input.lint_clean,
            predecessor_revision,
            resolution_kind: input.resolution_kind.clone(),
            resolution_rationale: input.resolution_rationale.clone(),
            content_hash,
            created_at_ms: now,
        })
    })
}

/// Read one frozen revision back verbatim (R1). Returns `None` when the
/// (contract, revision) pair does not exist.
pub fn contract_revision(
    cwd: &Path,
    contract_id: &str,
    revision: i64,
) -> Result<Option<ContractRevisionRecord>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    contract_revision_on(&connection, &context.repo_id, contract_id, revision)
}

pub(crate) fn contract_revision_on(
    conn: &Connection,
    repo_id: &str,
    contract_id: &str,
    revision: i64,
) -> Result<Option<ContractRevisionRecord>> {
    conn.query_row(
        "SELECT id, contract_id, revision, state, source_yaml, lint_clean,
                predecessor_revision, resolution_kind, resolution_rationale, content_hash, created_at_ms
         FROM contract_revisions
         WHERE repo_id = ?1 AND contract_id = ?2 AND revision = ?3",
        params![repo_id, contract_id, revision],
        map_contract_revision_row,
    )
    .optional()
    .map_err(Into::into)
}

/// The highest frozen revision of a contract, or `None` when it has none.
pub fn latest_contract_revision(
    cwd: &Path,
    contract_id: &str,
) -> Result<Option<ContractRevisionRecord>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let latest: Option<i64> = connection
        .query_row(
            "SELECT latest_revision FROM contracts WHERE repo_id = ?1 AND contract_id = ?2",
            params![context.repo_id, contract_id],
            |row| row.get(0),
        )
        .optional()?;
    match latest.filter(|value| *value > 0) {
        Some(revision) => {
            contract_revision_on(&connection, &context.repo_id, contract_id, revision)
        }
        None => Ok(None),
    }
}

fn map_contract_revision_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContractRevisionRecord> {
    Ok(ContractRevisionRecord {
        revision_row_id: row.get(0)?,
        contract_id: row.get(1)?,
        revision: row.get(2)?,
        state: row.get(3)?,
        source_yaml: row.get(4)?,
        lint_clean: row.get::<_, i64>(5)? != 0,
        predecessor_revision: row.get(6)?,
        resolution_kind: row.get(7)?,
        resolution_rationale: row.get(8)?,
        content_hash: row.get(9)?,
        created_at_ms: row.get(10)?,
    })
}

// ---------------------------------------------------------------------------
// Runs and per-task completion state
// ---------------------------------------------------------------------------

/// The recorded outcome of one task within a chain run (KTD9).
#[derive(Debug, Clone, Serialize)]
pub struct ContractRunTaskRecord {
    pub task_id: String,
    pub task_index: i64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_content_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_exit_code: Option<i64>,
    /// Redacted excerpt of the agent subprocess stdout (R7/R16). `None` when no
    /// agent ran (a resumed or skipped task) or the stream was empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_stdout_excerpt: Option<String>,
    /// Redacted excerpt of the agent subprocess stderr (R7/R16).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_stderr_excerpt: Option<String>,
}

/// One dependency-ordered chain run against a frozen contract revision (R7).
#[derive(Debug, Clone, Serialize)]
pub struct ContractRunRecord {
    pub run_id: String,
    pub contract_id: String,
    pub revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_stack_json: Option<String>,
    pub outcome: String,
    pub exit_code: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_content_ref: Option<String>,
    pub content_hash: String,
    pub created_at_ms: i64,
    pub tasks: Vec<ContractRunTaskRecord>,
}

/// Inputs to record a run and its per-task rows in one transaction.
#[derive(Debug, Clone)]
pub struct RecordContractRunInput {
    pub contract_id: String,
    pub revision: i64,
    pub base_head: Option<String>,
    pub dependency_stack_json: Option<String>,
    pub outcome: String,
    pub exit_code: i64,
    pub agent_exit_code: Option<i64>,
    pub patch_content_ref: Option<String>,
    pub tasks: Vec<ContractRunTaskInput>,
}

#[derive(Debug, Clone)]
pub struct ContractRunTaskInput {
    pub task_id: String,
    pub task_index: i64,
    pub outcome: String,
    pub patch_content_ref: Option<String>,
    pub agent_exit_code: Option<i64>,
    /// Already-redacted agent stdout/stderr excerpts (the CLI runs the redaction
    /// pass at capture time, KTD3 redact-before-sign). Folded into the run digest.
    pub agent_stdout_excerpt: Option<String>,
    pub agent_stderr_excerpt: Option<String>,
}

/// The machine-readable `outcome` discriminator carried in a `contract run`
/// envelope's `data` (R25) and persisted in `contract_runs.outcome`. Serializes as
/// snake_case; [`ContractRunOutcome::as_str`] is the persisted/validated string and
/// the two are kept in lockstep by the parity test in this module (the same
/// serde-vs-`as_str` discipline as `error::TamperKind`). Exit-code mapping is the
/// CLI's job (U3): completed→0, failed→1, stopped→2, blast_violation→3 (R14/KTD10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractRunOutcome {
    Completed,
    Stopped,
    BlastViolation,
    Failed,
}

impl ContractRunOutcome {
    /// Every run outcome, the single source of truth for the persisted vocabulary.
    pub const ALL: [ContractRunOutcome; 4] = [
        ContractRunOutcome::Completed,
        ContractRunOutcome::Stopped,
        ContractRunOutcome::BlastViolation,
        ContractRunOutcome::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ContractRunOutcome::Completed => "completed",
            ContractRunOutcome::Stopped => "stopped",
            ContractRunOutcome::BlastViolation => "blast_violation",
            ContractRunOutcome::Failed => "failed",
        }
    }

    /// Parse a persisted/supplied outcome string, or `None` if out of vocabulary.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == value)
    }
}

/// The machine-readable `outcome` discriminator carried in a `contract verify`
/// envelope's `data` (R25). Verify has no persisted outcome column — verdicts are
/// per-command rows (KTD4) — so this is purely the computed envelope discriminator.
/// Exit-code mapping is the CLI's job (U7): passed→0, fix_failed→2, guard_regressed→4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractVerifyOutcome {
    Passed,
    FixFailed,
    GuardRegressed,
}

impl ContractVerifyOutcome {
    pub const ALL: [ContractVerifyOutcome; 3] = [
        ContractVerifyOutcome::Passed,
        ContractVerifyOutcome::FixFailed,
        ContractVerifyOutcome::GuardRegressed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ContractVerifyOutcome::Passed => "passed",
            ContractVerifyOutcome::FixFailed => "fix_failed",
            ContractVerifyOutcome::GuardRegressed => "guard_regressed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == value)
    }
}

const RUN_TASK_OUTCOMES: &[&str] = &["pending", "completed", "stopped", "failed", "skipped"];

fn contract_run_digest(input: &RecordContractRunInput, created_at_ms: i64) -> String {
    let mut digest = ContractDigest::new(SUBJECT_KIND_CONTRACT_RUN);
    digest
        .str(&input.contract_id)
        .i64(input.revision)
        .opt_str(input.base_head.as_deref())
        .opt_str(input.dependency_stack_json.as_deref())
        .str(&input.outcome)
        .i64(input.exit_code)
        .opt_i64(input.agent_exit_code)
        .opt_str(input.patch_content_ref.as_deref())
        .i64(created_at_ms);
    // Fold each per-task row so editing a task's recorded completion state is
    // detectable (KTD9 resume integrity).
    digest.i64(input.tasks.len() as i64);
    for task in &input.tasks {
        digest
            .str(&task.task_id)
            .i64(task.task_index)
            .str(&task.outcome)
            .opt_str(task.patch_content_ref.as_deref())
            .opt_i64(task.agent_exit_code)
            .opt_str(task.agent_stdout_excerpt.as_deref())
            .opt_str(task.agent_stderr_excerpt.as_deref());
    }
    digest.finish()
}

/// Validate a run input's outcome vocabulary before any write (shared by
/// [`record_contract_run`] and [`record_contract_run_with_stop`]).
fn validate_run_input(input: &RecordContractRunInput) -> Result<()> {
    if ContractRunOutcome::parse(&input.outcome).is_none() {
        bail!("unsupported contract run outcome `{}`", input.outcome);
    }
    for task in &input.tasks {
        if !RUN_TASK_OUTCOMES.contains(&task.outcome.as_str()) {
            bail!("unsupported contract run task outcome `{}`", task.outcome);
        }
    }
    Ok(())
}

/// Insert a run plus its per-task rows, sign under `"contract_run"`, and chain the
/// op onto `parent_operation_id` — WITHIN an existing IMMEDIATE txn. Returns the
/// run record and the operation id the spine advanced to, so a caller composing a
/// second chained write in the SAME txn (the atomic stopped-run path, F1) threads
/// it as the next parent. Enforces R2 at the store boundary: a run may only be
/// recorded against a lint-clean frozen revision.
#[allow(clippy::too_many_arguments)]
fn insert_contract_run_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    parent_operation_id: &str,
    signer: &signing::LocalSigner,
    request_id: Option<String>,
    input: &RecordContractRunInput,
    now: i64,
) -> Result<(ContractRunRecord, String)> {
    // R2: only a lint-clean frozen revision can produce runs. An absent, draft,
    // or frozen-but-not-lint-clean revision all collapse to the typed
    // CONTRACT_NOT_FROZEN refusal (U2/KTD10) — carried in anyhow, recovered at the
    // CLI by downcast (the KTD6 typed-error pattern).
    let not_frozen = || ForgeError::ContractNotFrozen {
        contract_id: input.contract_id.clone(),
        revision: input.revision,
    };
    let frozen = contract_revision_on(tx, repo_id, &input.contract_id, input.revision)?
        .ok_or_else(not_frozen)?;
    if frozen.state != "frozen" || !frozen.lint_clean {
        return Err(not_frozen().into());
    }
    let run_id = new_id("contract_run");
    let content_hash = contract_run_digest(input, now);
    tx.execute(
        "INSERT INTO contract_runs (
            id, repo_id, contract_id, revision, base_head, dependency_stack_json,
            outcome, exit_code, agent_exit_code, patch_content_ref, request_id, content_hash,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
        params![
            run_id,
            repo_id,
            input.contract_id,
            input.revision,
            input.base_head,
            input.dependency_stack_json,
            input.outcome,
            input.exit_code,
            input.agent_exit_code,
            input.patch_content_ref,
            request_id,
            content_hash,
            now,
        ],
    )?;
    let mut tasks = Vec::with_capacity(input.tasks.len());
    for task in &input.tasks {
        let task_row_id = new_id("contract_run_task");
        tx.execute(
            "INSERT INTO contract_run_tasks (
                id, repo_id, run_id, task_id, task_index, outcome, patch_content_ref,
                agent_exit_code, agent_stdout_excerpt, agent_stderr_excerpt,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                task_row_id,
                repo_id,
                run_id,
                task.task_id,
                task.task_index,
                task.outcome,
                task.patch_content_ref,
                task.agent_exit_code,
                task.agent_stdout_excerpt,
                task.agent_stderr_excerpt,
                now,
            ],
        )?;
        tasks.push(ContractRunTaskRecord {
            task_id: task.task_id.clone(),
            task_index: task.task_index,
            outcome: task.outcome.clone(),
            patch_content_ref: task.patch_content_ref.clone(),
            agent_exit_code: task.agent_exit_code,
            agent_stdout_excerpt: task.agent_stdout_excerpt.clone(),
            agent_stderr_excerpt: task.agent_stderr_excerpt.clone(),
        });
    }
    signer.sign_subject(
        tx,
        repo_id,
        SUBJECT_KIND_CONTRACT_RUN,
        &run_id,
        &content_hash,
        now,
    )?;
    let op = insert_operation_view_chained(
        tx,
        repo_id,
        Some(parent_operation_id),
        OperationViewInput {
            request_id: request_id.clone(),
            // Must equal the CLI command string so `command_result`'s pre-flight
            // replay folds a same-request-id retry to the recorded run WITHOUT
            // re-executing the agent subprocess (KTD6).
            command: "contract run".to_string(),
            kind: "contract_run_recorded".to_string(),
            view_kind: ViewKind::Initialized,
            state: json!({
                "lifecycle": "contract_run_recorded",
                "run_id": run_id,
                "outcome": input.outcome,
            }),
        },
        Some(&content_hash),
    )?;
    let record = ContractRunRecord {
        run_id,
        contract_id: input.contract_id.clone(),
        revision: input.revision,
        base_head: input.base_head.clone(),
        dependency_stack_json: input.dependency_stack_json.clone(),
        outcome: input.outcome.clone(),
        exit_code: input.exit_code,
        agent_exit_code: input.agent_exit_code,
        patch_content_ref: input.patch_content_ref.clone(),
        content_hash,
        created_at_ms: now,
        tasks,
    };
    Ok((record, op.operation_id))
}

/// Record a run plus its per-task completion rows, signed under `"contract_run"`
/// and chained (R7/R17). Enforces R2 at the store boundary: a run may only be
/// recorded against a lint-clean frozen revision. `--request-id` replay-safe.
pub fn record_contract_run(
    cwd: &Path,
    request_id: Option<String>,
    input: RecordContractRunInput,
) -> Result<ContractRunRecord> {
    validate_run_input(&input)?;
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let (record, _op) = insert_contract_run_in_tx(
            tx,
            &context.repo_id,
            &context.current_operation_id,
            &signer,
            request_id.clone(),
            &input,
            now,
        )?;
        Ok(record)
    })
}

/// Record a STOPPED run AND open its stop record in ONE immediate transaction
/// (F1). The two writes previously ran in separate transactions, so a stop-insert
/// failure after the run row committed left an `outcome = "stopped"` run with no
/// stop row (a Leg-1/Leg-2 integrity hole: a halt with nothing to triage). Here the
/// run's op is the stop's chain parent, and both rows commit or roll back together.
/// The stop's `run_id` is set to the freshly-created run id regardless of the
/// caller-supplied value. The run carries `request_id` for replay; the stop's op is
/// unkeyed (it is part of the same replay-anchored `contract run` command).
pub fn record_contract_run_with_stop(
    cwd: &Path,
    request_id: Option<String>,
    run_input: RecordContractRunInput,
    stop_input: OpenContractStopInput,
) -> Result<(ContractRunRecord, ContractStopRecord)> {
    validate_run_input(&run_input)?;
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let (run, run_op) = insert_contract_run_in_tx(
            tx,
            &context.repo_id,
            &context.current_operation_id,
            &signer,
            request_id.clone(),
            &run_input,
            now,
        )?;
        // The stop references the freshly-created run and chains onto the run's op in
        // the SAME txn — atomic stopped-run + stop (F1). Clone (not move) because
        // `with_immediate_retry` may re-invoke this closure (FnMut).
        let mut stop_with_run = stop_input.clone();
        stop_with_run.run_id = Some(run.run_id.clone());
        let (stop, _stop_op) = insert_contract_stop_in_tx(
            tx,
            &context.repo_id,
            &run_op,
            &signer,
            None,
            &stop_with_run,
            now,
        )?;
        Ok((run, stop))
    })
}

/// Read a run and its per-task completion rows back (KTD9 resume support).
pub fn contract_run(cwd: &Path, run_id: &str) -> Result<Option<ContractRunRecord>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let run = connection
        .query_row(
            "SELECT id, contract_id, revision, base_head, dependency_stack_json, outcome,
                    exit_code, agent_exit_code, patch_content_ref, content_hash, created_at_ms
             FROM contract_runs
             WHERE repo_id = ?1 AND id = ?2",
            params![context.repo_id, run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    let Some(run) = run else { return Ok(None) };
    let mut statement = connection.prepare(
        "SELECT task_id, task_index, outcome, patch_content_ref, agent_exit_code,
                agent_stdout_excerpt, agent_stderr_excerpt
         FROM contract_run_tasks
         WHERE run_id = ?1
         ORDER BY task_index",
    )?;
    let tasks = statement
        .query_map(params![run_id], |row| {
            Ok(ContractRunTaskRecord {
                task_id: row.get(0)?,
                task_index: row.get(1)?,
                outcome: row.get(2)?,
                patch_content_ref: row.get(3)?,
                agent_exit_code: row.get(4)?,
                agent_stdout_excerpt: row.get(5)?,
                agent_stderr_excerpt: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(ContractRunRecord {
        run_id: run.0,
        contract_id: run.1,
        revision: run.2,
        base_head: run.3,
        dependency_stack_json: run.4,
        outcome: run.5,
        exit_code: run.6,
        agent_exit_code: run.7,
        patch_content_ref: run.8,
        content_hash: run.9,
        created_at_ms: run.10,
        tasks,
    }))
}

// ---------------------------------------------------------------------------
// Stops
// ---------------------------------------------------------------------------

/// A typed stop record ingested from an agent-filed `UNKNOWN.md` (R8). The four
/// free-text fields are secret-redacted before hashing/signing (R16/KTD3).
#[derive(Debug, Clone, Serialize)]
pub struct ContractStopRecord {
    pub stop_id: String,
    pub contract_id: String,
    pub revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub what_needed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_unanswered: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    pub malformed: bool,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_rationale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolving_revision: Option<i64>,
    pub content_hash: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Inputs to ingest a stop. The four free-text fields are raw agent content; the
/// store redacts them before persisting, hashing, and signing.
#[derive(Debug, Clone)]
pub struct OpenContractStopInput {
    pub contract_id: String,
    pub revision: i64,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub what_needed: Option<String>,
    pub why_unanswered: Option<String>,
    pub kind: Option<String>,
    pub evidence: Option<String>,
    /// Best-effort ingest flag: the four fields could not be fully extracted (R8).
    pub malformed: bool,
}

/// Redact a single agent-authored free-text field before it enters the ledger
/// (KTD3). `None` and empty stay as-is; otherwise the secret-redaction pass runs.
fn redact_field(value: Option<&str>) -> Option<String> {
    value.map(|text| forge_content::redact_evidence_excerpt(text).0)
}

#[allow(clippy::too_many_arguments)]
fn contract_stop_digest(
    contract_id: &str,
    revision: i64,
    run_id: Option<&str>,
    task_id: Option<&str>,
    what_needed: Option<&str>,
    why_unanswered: Option<&str>,
    kind: Option<&str>,
    evidence: Option<&str>,
    malformed: bool,
    state: &str,
    resolution_kind: Option<&str>,
    resolution_rationale: Option<&str>,
    resolving_revision: Option<i64>,
    created_at_ms: i64,
) -> String {
    let mut digest = ContractDigest::new(SUBJECT_KIND_CONTRACT_STOP);
    digest
        .str(contract_id)
        .i64(revision)
        .opt_str(run_id)
        .opt_str(task_id)
        .opt_str(what_needed)
        .opt_str(why_unanswered)
        .opt_str(kind)
        .opt_str(evidence)
        .bool(malformed)
        .str(state)
        .opt_str(resolution_kind)
        .opt_str(resolution_rationale)
        .opt_i64(resolving_revision)
        .i64(created_at_ms);
    digest.finish()
}

/// Ingest and open a stop record, redacting the four free-text fields BEFORE
/// hashing and signing, sign under `"contract_stop"`, and chain the op onto
/// `parent_operation_id` — WITHIN an existing IMMEDIATE txn. Returns the stop record
/// and the operation id the spine advanced to. Factored out so the atomic
/// stopped-run + stop path ([`record_contract_run_with_stop`], F1) can compose it in
/// the same txn as the run write.
#[allow(clippy::too_many_arguments)]
fn insert_contract_stop_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    parent_operation_id: &str,
    signer: &signing::LocalSigner,
    request_id: Option<String>,
    input: &OpenContractStopInput,
    now: i64,
) -> Result<(ContractStopRecord, String)> {
    let stop_id = new_id("contract_stop");
    // Redact-before-sign: agent free text is hardened before it is hashed or
    // written to the append-only, signed record.
    let what_needed = redact_field(input.what_needed.as_deref());
    let why_unanswered = redact_field(input.why_unanswered.as_deref());
    let kind = redact_field(input.kind.as_deref());
    let evidence = redact_field(input.evidence.as_deref());
    let state = "open";
    let content_hash = contract_stop_digest(
        &input.contract_id,
        input.revision,
        input.run_id.as_deref(),
        input.task_id.as_deref(),
        what_needed.as_deref(),
        why_unanswered.as_deref(),
        kind.as_deref(),
        evidence.as_deref(),
        input.malformed,
        state,
        None,
        None,
        None,
        now,
    );
    tx.execute(
        "INSERT INTO contract_stops (
            id, repo_id, contract_id, revision, run_id, task_id, what_needed, why_unanswered,
            kind, evidence, malformed, state, resolution_kind, resolution_rationale,
            resolving_revision, content_hash, request_id, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, NULL, NULL, ?13, ?14, ?15, ?15)",
        params![
            stop_id,
            repo_id,
            input.contract_id,
            input.revision,
            input.run_id,
            input.task_id,
            what_needed,
            why_unanswered,
            kind,
            evidence,
            input.malformed as i64,
            state,
            content_hash,
            request_id,
            now,
        ],
    )?;
    signer.sign_subject(
        tx,
        repo_id,
        SUBJECT_KIND_CONTRACT_STOP,
        &stop_id,
        &content_hash,
        now,
    )?;
    let op = insert_operation_view_chained(
        tx,
        repo_id,
        Some(parent_operation_id),
        OperationViewInput {
            request_id: request_id.clone(),
            command: "contract".to_string(),
            kind: "contract_stop_opened".to_string(),
            view_kind: ViewKind::Initialized,
            state: json!({
                "lifecycle": "contract_stop_opened",
                "stop_id": stop_id,
                "malformed": input.malformed,
            }),
        },
        Some(&content_hash),
    )?;
    let record = ContractStopRecord {
        stop_id,
        contract_id: input.contract_id.clone(),
        revision: input.revision,
        run_id: input.run_id.clone(),
        task_id: input.task_id.clone(),
        what_needed,
        why_unanswered,
        kind,
        evidence,
        malformed: input.malformed,
        state: state.to_string(),
        resolution_kind: None,
        resolution_rationale: None,
        resolving_revision: None,
        content_hash,
        created_at_ms: now,
        updated_at_ms: now,
    };
    Ok((record, op.operation_id))
}

/// Ingest and open a stop record, redacting the four free-text fields BEFORE
/// hashing and signing (R8/R16/KTD3), signed under `"contract_stop"` and
/// chained (R17). `--request-id` replay-safe (R18).
pub fn open_contract_stop(
    cwd: &Path,
    request_id: Option<String>,
    input: OpenContractStopInput,
) -> Result<ContractStopRecord> {
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let (record, _op) = insert_contract_stop_in_tx(
            tx,
            &context.repo_id,
            &context.current_operation_id,
            &signer,
            request_id.clone(),
            &input,
            now,
        )?;
        Ok(record)
    })
}

/// Resolve an open stop (R10): record a revision-bump link or an explicit
/// rejection with rationale, re-signing the mutated row under `"contract_stop"`.
/// Refuses to resolve an already-resolved stop (lifecycle guard). Replay-safe.
pub fn resolve_contract_stop(
    cwd: &Path,
    request_id: Option<String>,
    stop_id: &str,
    resolution_kind: &str,
    resolution_rationale: Option<&str>,
    resolving_revision: Option<i64>,
) -> Result<ContractStopRecord> {
    if !matches!(resolution_kind, "revision" | "rejection") {
        bail!("unsupported contract resolution kind `{resolution_kind}`");
    }
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let existing = contract_stop_on(tx, &context.repo_id, stop_id)?
            .ok_or_else(|| anyhow!("contract stop not found"))?;
        if existing.state != "open" {
            bail!("contract stop is not open (state `{}`)", existing.state);
        }
        let now = now_ms();
        let state = "resolved";
        let rationale = redact_field(resolution_rationale);
        let content_hash = contract_stop_digest(
            &existing.contract_id,
            existing.revision,
            existing.run_id.as_deref(),
            existing.task_id.as_deref(),
            existing.what_needed.as_deref(),
            existing.why_unanswered.as_deref(),
            existing.kind.as_deref(),
            existing.evidence.as_deref(),
            existing.malformed,
            state,
            Some(resolution_kind),
            rationale.as_deref(),
            resolving_revision,
            now,
        );
        tx.execute(
            "UPDATE contract_stops
             SET state = ?1, resolution_kind = ?2, resolution_rationale = ?3,
                 resolving_revision = ?4, content_hash = ?5, updated_at_ms = ?6
             WHERE repo_id = ?7 AND id = ?8",
            params![
                state,
                resolution_kind,
                rationale,
                resolving_revision,
                content_hash,
                now,
                context.repo_id,
                stop_id,
            ],
        )?;
        signer.sign_subject(
            tx,
            &context.repo_id,
            SUBJECT_KIND_CONTRACT_STOP,
            stop_id,
            &content_hash,
            now,
        )?;
        insert_operation_view_chained(
            tx,
            &context.repo_id,
            Some(&context.current_operation_id),
            OperationViewInput {
                request_id: request_id.clone(),
                command: "contract".to_string(),
                kind: "contract_stop_resolved".to_string(),
                view_kind: ViewKind::Initialized,
                state: json!({
                    "lifecycle": "contract_stop_resolved",
                    "stop_id": stop_id,
                    "resolution_kind": resolution_kind,
                }),
            },
            Some(&content_hash),
        )?;
        Ok(ContractStopRecord {
            stop_id: stop_id.to_string(),
            contract_id: existing.contract_id,
            revision: existing.revision,
            run_id: existing.run_id,
            task_id: existing.task_id,
            what_needed: existing.what_needed,
            why_unanswered: existing.why_unanswered,
            kind: existing.kind,
            evidence: existing.evidence,
            malformed: existing.malformed,
            state: state.to_string(),
            resolution_kind: Some(resolution_kind.to_string()),
            resolution_rationale: rationale,
            resolving_revision,
            content_hash,
            created_at_ms: existing.created_at_ms,
            updated_at_ms: now,
        })
    })
}

/// List stop records for a repo, optionally filtered to one contract and/or the
/// open state (R23 read surface; the CLI shape lands in U8).
pub fn contract_stops(
    cwd: &Path,
    contract_id: Option<&str>,
    open_only: bool,
) -> Result<Vec<ContractStopRecord>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let mut sql = String::from(
        "SELECT id, contract_id, revision, run_id, task_id, what_needed, why_unanswered, kind,
                evidence, malformed, state, resolution_kind, resolution_rationale, resolving_revision,
                content_hash, created_at_ms, updated_at_ms
         FROM contract_stops
         WHERE repo_id = ?1",
    );
    if contract_id.is_some() {
        sql.push_str(" AND contract_id = ?2");
    }
    if open_only {
        sql.push_str(" AND state = 'open'");
    }
    sql.push_str(" ORDER BY created_at_ms, id");
    let mut statement = connection.prepare(&sql)?;
    let mapper = map_contract_stop_row;
    let rows = if let Some(contract_id) = contract_id {
        statement.query_map(params![context.repo_id, contract_id], mapper)?
    } else {
        statement.query_map(params![context.repo_id], mapper)?
    };
    let stops = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(stops)
}

pub(crate) fn contract_stop_on(
    conn: &Connection,
    repo_id: &str,
    stop_id: &str,
) -> Result<Option<ContractStopRecord>> {
    conn.query_row(
        "SELECT id, contract_id, revision, run_id, task_id, what_needed, why_unanswered, kind,
                evidence, malformed, state, resolution_kind, resolution_rationale, resolving_revision,
                content_hash, created_at_ms, updated_at_ms
         FROM contract_stops
         WHERE repo_id = ?1 AND id = ?2",
        params![repo_id, stop_id],
        map_contract_stop_row,
    )
    .optional()
    .map_err(Into::into)
}

fn map_contract_stop_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContractStopRecord> {
    Ok(ContractStopRecord {
        stop_id: row.get(0)?,
        contract_id: row.get(1)?,
        revision: row.get(2)?,
        run_id: row.get(3)?,
        task_id: row.get(4)?,
        what_needed: row.get(5)?,
        why_unanswered: row.get(6)?,
        kind: row.get(7)?,
        evidence: row.get(8)?,
        malformed: row.get::<_, i64>(9)? != 0,
        state: row.get(10)?,
        resolution_kind: row.get(11)?,
        resolution_rationale: row.get(12)?,
        resolving_revision: row.get(13)?,
        content_hash: row.get(14)?,
        created_at_ms: row.get(15)?,
        updated_at_ms: row.get(16)?,
    })
}

// ---------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------

/// A per-command (or aggregate) verdict against a run (KTD4/R12/R13).
#[derive(Debug, Clone, Serialize)]
pub struct ContractRunVerdictRecord {
    pub verdict_id: String,
    pub run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub verdict_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<String>,
    pub content_hash: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct ContractRunVerdictInput {
    pub task_id: Option<String>,
    pub verdict_kind: String,
    pub command: Option<String>,
    pub passed: bool,
    pub detail: Option<String>,
    pub evidence_id: Option<String>,
}

const VERDICT_KINDS: &[&str] = &["blast", "fix", "guard", "aggregate"];

fn contract_verdict_digest(
    run_id: &str,
    input: &ContractRunVerdictInput,
    created_at_ms: i64,
) -> String {
    let mut digest = ContractDigest::new(SUBJECT_KIND_CONTRACT_VERDICT);
    digest
        .str(run_id)
        .opt_str(input.task_id.as_deref())
        .str(&input.verdict_kind)
        .opt_str(input.command.as_deref())
        .bool(input.passed)
        .opt_str(input.detail.as_deref())
        .opt_str(input.evidence_id.as_deref())
        .i64(created_at_ms);
    digest.finish()
}

/// Validate a verdict batch's shape (non-empty, known kinds) before any write —
/// shared by the standalone and combined recorders.
fn validate_verdicts(verdicts: &[ContractRunVerdictInput]) -> Result<()> {
    if verdicts.is_empty() {
        bail!("at least one verdict is required");
    }
    for verdict in verdicts {
        if !VERDICT_KINDS.contains(&verdict.verdict_kind.as_str()) {
            bail!("unsupported verdict kind `{}`", verdict.verdict_kind);
        }
    }
    Ok(())
}

/// Insert a batch of verdict rows, sign each under `"contract_run_verdict"`, and
/// chain ONE op onto `parent_operation_id` — WITHIN an existing IMMEDIATE txn. Shared
/// by [`record_contract_run_verdicts`] (parent = current op) and the atomic
/// run+verdicts path (parent = the run's op, so a blast-violation run and its
/// verdicts commit or roll back together, U6/KTD3).
#[allow(clippy::too_many_arguments)]
fn insert_contract_verdicts_in_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    parent_operation_id: &str,
    signer: &signing::LocalSigner,
    request_id: Option<String>,
    run_id: &str,
    verdicts: &[ContractRunVerdictInput],
    now: i64,
) -> Result<Vec<ContractRunVerdictRecord>> {
    let mut records = Vec::with_capacity(verdicts.len());
    for verdict in verdicts {
        let verdict_id = new_id("contract_verdict");
        let content_hash = contract_verdict_digest(run_id, verdict, now);
        tx.execute(
            "INSERT INTO contract_run_verdicts (
                id, repo_id, run_id, task_id, verdict_kind, command, passed, detail,
                evidence_id, content_hash, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                verdict_id,
                repo_id,
                run_id,
                verdict.task_id,
                verdict.verdict_kind,
                verdict.command,
                verdict.passed as i64,
                verdict.detail,
                verdict.evidence_id,
                content_hash,
                now,
            ],
        )?;
        signer.sign_subject(
            tx,
            repo_id,
            SUBJECT_KIND_CONTRACT_VERDICT,
            &verdict_id,
            &content_hash,
            now,
        )?;
        records.push(ContractRunVerdictRecord {
            verdict_id,
            run_id: run_id.to_string(),
            task_id: verdict.task_id.clone(),
            verdict_kind: verdict.verdict_kind.clone(),
            command: verdict.command.clone(),
            passed: verdict.passed,
            detail: verdict.detail.clone(),
            evidence_id: verdict.evidence_id.clone(),
            content_hash,
            created_at_ms: now,
        });
    }
    // One op-log link per batch, folding the last verdict's digest into the
    // spine (each verdict row carries its own signature above).
    let spine_digest = records
        .last()
        .map(|record| record.content_hash.clone())
        .expect("non-empty verdicts");
    insert_operation_view_chained(
        tx,
        repo_id,
        Some(parent_operation_id),
        OperationViewInput {
            request_id,
            command: "contract".to_string(),
            kind: "contract_verdicts_recorded".to_string(),
            view_kind: ViewKind::Initialized,
            state: json!({
                "lifecycle": "contract_verdicts_recorded",
                "run_id": run_id,
                "count": records.len(),
            }),
        },
        Some(&spine_digest),
    )?;
    Ok(records)
}

/// Record one or more verdict rows against a run in a single transaction, each
/// signed under `"contract_run_verdict"` and chained (R12/R13/R17). Replay-safe.
pub fn record_contract_run_verdicts(
    cwd: &Path,
    request_id: Option<String>,
    run_id: &str,
    verdicts: Vec<ContractRunVerdictInput>,
) -> Result<Vec<ContractRunVerdictRecord>> {
    validate_verdicts(&verdicts)?;
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let run_exists: bool = tx
            .query_row(
                "SELECT 1 FROM contract_runs WHERE repo_id = ?1 AND id = ?2",
                params![context.repo_id, run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !run_exists {
            bail!("contract run not found");
        }
        let now = now_ms();
        insert_contract_verdicts_in_tx(
            tx,
            &context.repo_id,
            &context.current_operation_id,
            &signer,
            request_id.clone(),
            run_id,
            &verdicts,
            now,
        )
    })
}

/// Record a run AND its verdict rows in ONE immediate transaction (U6). The two
/// writes previously would have to run in separate transactions, so a verdict-insert
/// failure after the run row committed could leave a `blast_violation` run with no
/// verdict to explain it. Here the verdicts chain onto the run's op and both commit
/// or roll back together. The run carries `request_id` for replay; the verdict batch's
/// op is unkeyed (part of the same replay-anchored `contract run` command). Used by
/// the blast postflight: a clean run records per-task `blast`-pass verdicts, and a
/// violation records the pass verdicts of prior tasks plus the offending task's
/// failing verdicts — the offending patch is never referenced by the run row, so GC
/// reclaims any secret-bearing tree object the snapshot step already wrote (KTD3/R16).
pub fn record_contract_run_with_verdicts(
    cwd: &Path,
    request_id: Option<String>,
    run_input: RecordContractRunInput,
    verdicts: Vec<ContractRunVerdictInput>,
) -> Result<(ContractRunRecord, Vec<ContractRunVerdictRecord>)> {
    validate_run_input(&run_input)?;
    validate_verdicts(&verdicts)?;
    let context = open_repository(cwd)?;
    let signer = signing::LocalSigner::load_or_create(&context.root_path)?;
    let mut connection = open_connection(&context.database_path)?;
    with_immediate_retry(&mut connection, |tx| {
        replay_guard(tx, &context.repo_id, request_id.as_deref())?;
        let now = now_ms();
        let (run, run_op) = insert_contract_run_in_tx(
            tx,
            &context.repo_id,
            &context.current_operation_id,
            &signer,
            request_id.clone(),
            &run_input,
            now,
        )?;
        let verdict_records = insert_contract_verdicts_in_tx(
            tx,
            &context.repo_id,
            &run_op,
            &signer,
            None,
            &run.run_id,
            &verdicts,
            now,
        )?;
        Ok((run, verdict_records))
    })
}

/// List verdict rows for a run in insertion order (R23 read surface).
pub fn contract_run_verdicts(cwd: &Path, run_id: &str) -> Result<Vec<ContractRunVerdictRecord>> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;
    let mut statement = connection.prepare(
        "SELECT id, run_id, task_id, verdict_kind, command, passed, detail, evidence_id,
                content_hash, created_at_ms
         FROM contract_run_verdicts
         WHERE repo_id = ?1 AND run_id = ?2
         ORDER BY created_at_ms, id",
    )?;
    let verdicts = statement
        .query_map(params![context.repo_id, run_id], |row| {
            Ok(ContractRunVerdictRecord {
                verdict_id: row.get(0)?,
                run_id: row.get(1)?,
                task_id: row.get(2)?,
                verdict_kind: row.get(3)?,
                command: row.get(4)?,
                passed: row.get::<_, i64>(5)? != 0,
                detail: row.get(6)?,
                evidence_id: row.get(7)?,
                content_hash: row.get(8)?,
                created_at_ms: row.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(verdicts)
}

// ---------------------------------------------------------------------------
// Brief emission (U4) — a byte-stable pure function of frozen revisions
// ---------------------------------------------------------------------------
//
// `forge contract brief` reproduces `tools/ccx/ccx-brief.py` output BYTE-FOR-BYTE
// for the same inputs (R5). The brief is a pure function of: the frozen global
// policy (reserved id `_global-policy`), the frozen task revision's verbatim
// source bytes, and its declared neighbors' frozen revisions — in declared order.
//
// Parity notes vs. ccx-brief.py:
// - Section framing is `--- {header} ---\n` + verbatim source bytes + `\n`,
//   char-for-char identical to the Python `section()` (which the pilot brief.sh
//   `emit()` established). Because `freeze` stores the exact authored bytes (R1),
//   `source_yaml` equals the file bytes the Python emitter would read.
// - Neighbor resolution: the Python resolves `ccx-<name>` to `<name>.yaml` on
//   disk; here the neighbor's ledger `contract_id` IS the full declared id (freeze
//   keys the row on the YAML `id`), so a declared neighbor id resolves directly to
//   its latest frozen revision. Emission is in the contract's declared order.
// - Missing neighbor: the Python emits a MISSING marker and still exits 0 when the
//   neighbor FILE is absent; the native analogue is a declared neighbor with NO
//   frozen revision in the ledger (the plan's residual-risk case). Both emit the
//   identical marker bytes `--- NEIGHBOR CONTRACT MISSING: {id} (surface as
//   unknown, do not guess) ---\n\n` and the brief still succeeds.
// - Global policy / task fail-closed: the Python fails closed (nonzero exit, no
//   stdout) when the contract or global policy cannot be read. The native analogue
//   is the typed `CONTRACT_NOT_FROZEN` refusal when either has no lint-clean frozen
//   revision (R2/R5 gate the task; the policy is required just as in the harness).
//
// R6 (task-instruction wording) is NOT emitted here, matching ccx-brief.py, which
// does not append it — `run-task.sh` `cat`s `prompts/task-instruction.txt` onto the
// brief at RUN time. The verbatim wording is exposed as a single CLI-side constant
// (`CONTRACT_TASK_INSTRUCTION`, U4) so U5's prompt assembly appends it and R6's
// verbatim-travel guarantee is satisfied there without breaking brief byte-parity.

/// Reserved ledger contract id for the repo-level global policy revision. The
/// single source of truth shared by U3's freeze (which frozen the policy under
/// this id) and U4's brief (which retrieves it). Mirrors ccx-brief.py's
/// `_global-policy.yaml` default.
pub const GLOBAL_POLICY_CONTRACT_ID: &str = "_global-policy";

/// One declared neighbor's resolution status in an emitted brief (R23-friendly).
#[derive(Debug, Clone, Serialize)]
pub struct ContractBriefNeighbor {
    pub id: String,
    /// The frozen revision emitted, or `None` when the neighbor has no frozen
    /// revision (a MISSING marker was emitted instead).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    pub present: bool,
}

/// The result of emitting a brief: the byte-stable text plus the resolved inputs.
#[derive(Debug, Clone, Serialize)]
pub struct ContractBriefRecord {
    pub contract_id: String,
    pub revision: i64,
    pub global_policy_revision: i64,
    pub neighbors: Vec<ContractBriefNeighbor>,
    /// The byte-stable brief text (R5). Emitting the same frozen inputs twice
    /// yields identical bytes.
    pub brief: String,
}

/// One framed section, char-for-char identical to ccx-brief.py's `section()`:
/// `--- {header} ---\n` + verbatim body + one trailing `\n`.
fn brief_section(header: &str, body: &str) -> String {
    format!("--- {header} ---\n{body}\n")
}

/// The missing-neighbor marker, byte-identical to ccx-brief.py: the header line
/// plus a trailing blank line (`\n\n`), no body.
fn brief_missing_neighbor(nid: &str) -> String {
    format!("--- NEIGHBOR CONTRACT MISSING: {nid} (surface as unknown, do not guess) ---\n\n")
}

/// Parse the declared `neighbors:` id list from a frozen revision's verbatim YAML,
/// mirroring ccx-brief.py: `neighbors: null`/absent is empty; a list of strings is
/// accepted in declared order; anything else is an error.
fn parse_brief_neighbors(source_yaml: &str) -> Result<Vec<String>> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(source_yaml)
        .map_err(|err| anyhow!("frozen contract is not valid YAML: {err}"))?;
    let mapping = parsed
        .as_mapping()
        .ok_or_else(|| anyhow!("frozen contract is not a YAML mapping"))?;
    match mapping.get(serde_yaml::Value::from("neighbors")) {
        None | Some(serde_yaml::Value::Null) => Ok(Vec::new()),
        Some(serde_yaml::Value::Sequence(items)) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(id) => ids.push(id.to_string()),
                    None => bail!("neighbors: must be a list of ids"),
                }
            }
            Ok(ids)
        }
        Some(_) => bail!("neighbors: must be a list of ids"),
    }
}

/// The latest frozen revision of a contract on an already-open connection, or
/// `None` when it has no frozen revision. Connection-scoped sibling of
/// [`latest_contract_revision`] so brief emission resolves policy, task, and every
/// neighbor over one connection.
fn latest_contract_revision_on(
    conn: &Connection,
    repo_id: &str,
    contract_id: &str,
) -> Result<Option<ContractRevisionRecord>> {
    let latest: Option<i64> = conn
        .query_row(
            "SELECT latest_revision FROM contracts WHERE repo_id = ?1 AND contract_id = ?2",
            params![repo_id, contract_id],
            |row| row.get(0),
        )
        .optional()?;
    match latest.filter(|value| *value > 0) {
        Some(revision) => contract_revision_on(conn, repo_id, contract_id, revision),
        None => Ok(None),
    }
}

/// Emit a byte-stable brief for a frozen contract revision (R5/R6). `revision`
/// selects a specific frozen revision; `None` uses the contract's latest. The task
/// revision must be lint-clean and frozen (R2/R5) and the global policy must be
/// frozen, or a typed `CONTRACT_NOT_FROZEN` refusal is returned (fail-closed,
/// mirroring the Python emitter's fail-closed exit on an unreadable contract or
/// policy). Read-only: no lock, no signing.
pub fn contract_brief(
    cwd: &Path,
    contract_id: &str,
    revision: Option<i64>,
) -> Result<ContractBriefRecord> {
    let context = open_repository(cwd)?;
    let connection = open_connection(&context.database_path)?;

    // Global policy is required and prepended, exactly as in the harness.
    let policy =
        latest_contract_revision_on(&connection, &context.repo_id, GLOBAL_POLICY_CONTRACT_ID)?
            .ok_or_else(|| ForgeError::ContractNotFrozen {
                contract_id: GLOBAL_POLICY_CONTRACT_ID.to_string(),
                revision: 0,
            })?;

    // Task revision: a specific one if pinned, else the latest. Must be a
    // lint-clean frozen revision to produce a brief (R2/R5).
    let contract = match revision {
        Some(rev) => contract_revision_on(&connection, &context.repo_id, contract_id, rev)?,
        None => latest_contract_revision_on(&connection, &context.repo_id, contract_id)?,
    }
    .ok_or_else(|| ForgeError::ContractNotFrozen {
        contract_id: contract_id.to_string(),
        revision: revision.unwrap_or(0),
    })?;
    if contract.state != "frozen" || !contract.lint_clean {
        return Err(ForgeError::ContractNotFrozen {
            contract_id: contract_id.to_string(),
            revision: contract.revision,
        }
        .into());
    }

    let neighbor_ids = parse_brief_neighbors(&contract.source_yaml)?;

    let mut brief = String::new();
    brief.push_str(&brief_section(
        "GLOBAL POLICY (normative)",
        &policy.source_yaml,
    ));
    brief.push_str(&brief_section(
        "TASK CONTRACT (normative)",
        &contract.source_yaml,
    ));

    let mut neighbors = Vec::with_capacity(neighbor_ids.len());
    for nid in &neighbor_ids {
        match latest_contract_revision_on(&connection, &context.repo_id, nid)? {
            Some(neighbor) => {
                brief.push_str(&brief_section(
                    &format!("NEIGHBOR CONTRACT (normative): {nid}"),
                    &neighbor.source_yaml,
                ));
                neighbors.push(ContractBriefNeighbor {
                    id: nid.clone(),
                    revision: Some(neighbor.revision),
                    present: true,
                });
            }
            None => {
                brief.push_str(&brief_missing_neighbor(nid));
                neighbors.push(ContractBriefNeighbor {
                    id: nid.clone(),
                    revision: None,
                    present: false,
                });
            }
        }
    }

    Ok(ContractBriefRecord {
        contract_id: contract.contract_id,
        revision: contract.revision,
        global_policy_revision: policy.revision,
        neighbors,
        brief,
    })
}

// ---------------------------------------------------------------------------
// Acceptance command grammar (R15) — the single source of truth
// ---------------------------------------------------------------------------
//
// This is deliberately the ONE place the acceptance-command grammar is decided,
// so lint (U3) and the fix/guard verifier (U7) can never drift: a string that
// lint accepts is exactly a string the verifier will run, and vice versa. It
// ports `command_is_safe`/`COMMAND_GRAMMAR`/`SHELL_METACHARACTERS` from
// `tools/ccx/ccx-lint.py` verbatim.
//
// The grammar is prefix-anchored (`^cargo (test|clippy|fmt|build|run)\b`) by
// design — cargo takes arbitrary trailing args — so command SAFETY is a
// separate, explicit metacharacter check: an acceptance entry must BOTH start
// with an allowed cargo subcommand AND contain no shell control character that
// would turn a lint-passing `cargo ...` prefix into an injection when the string
// reaches the verifier's process spawn (the eval-sink hardening R15 preserves).

/// The allowed leading cargo subcommands for an acceptance command (R15).
const ACCEPTANCE_SUBCOMMANDS: [&str; 5] = ["test", "clippy", "fmt", "build", "run"];

/// Shell control characters that must never appear in an acceptance command,
/// character-for-character identical to `ccx-lint.py`'s `SHELL_METACHARACTERS`.
const ACCEPTANCE_SHELL_METACHARACTERS: &str = ";&|`$(){}<>\n\r\\!*?[]#\"'";

/// The classification of one acceptance command against the reviewed grammar.
/// Distinguishing the two failure modes lets lint emit the same two messages
/// `ccx-lint.py` rule 6 emits (metacharacter vs. grammar), and lets the verifier
/// refuse with a precise reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceCommandCheck {
    /// Grammar-valid and metacharacter-free — safe to execute.
    Ok,
    /// Starts with an allowed cargo subcommand but carries a shell metacharacter.
    ShellMetacharacter,
    /// Does not match `^cargo (test|clippy|fmt|build|run)\b` at all.
    GrammarViolation,
}

/// Does `cmd` match `^cargo (test|clippy|fmt|build|run)\b`? Prefix-anchored, with
/// a word boundary after the subcommand so `cargo testfoo` does NOT match.
fn acceptance_matches_grammar(cmd: &str) -> bool {
    let Some(rest) = cmd.strip_prefix("cargo ") else {
        return false;
    };
    for sub in ACCEPTANCE_SUBCOMMANDS {
        if let Some(after) = rest.strip_prefix(sub) {
            // `\b`: the subcommand must be followed by end-of-string or a
            // non-word character (word = [A-Za-z0-9_]).
            let boundary = after
                .chars()
                .next()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if boundary {
                return true;
            }
        }
    }
    false
}

/// Classify an acceptance command against the reviewed grammar (R15). This is the
/// single source of truth shared by lint (U3) and the verifier (U7).
pub fn check_acceptance_command(cmd: &str) -> AcceptanceCommandCheck {
    if !acceptance_matches_grammar(cmd) {
        return AcceptanceCommandCheck::GrammarViolation;
    }
    if cmd
        .chars()
        .any(|c| ACCEPTANCE_SHELL_METACHARACTERS.contains(c))
    {
        return AcceptanceCommandCheck::ShellMetacharacter;
    }
    AcceptanceCommandCheck::Ok
}

/// A grammar-valid, metacharacter-free acceptance command (R15). The fail-closed
/// gate both lint and the verifier consult before an acceptance string is ever
/// stored or executed.
pub fn acceptance_command_is_safe(cmd: &str) -> bool {
    matches!(check_acceptance_command(cmd), AcceptanceCommandCheck::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal native-backend repo: no git binary needed, and `init` seeds the
    /// `current_state` singleton the op-log chain advances.
    fn init_native_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("temp dir");
        crate::init_repository(temp.path(), None, "native".to_string())
            .expect("init native repository");
        temp
    }

    fn frozen_input(contract_id: &str, yaml: &str) -> FreezeContractRevisionInput {
        FreezeContractRevisionInput {
            contract_id: contract_id.to_string(),
            source_yaml: yaml.to_string(),
            lint_clean: true,
            resolution_kind: None,
            resolution_rationale: None,
        }
    }

    #[test]
    fn frozen_revision_reads_back_verbatim() {
        let temp = init_native_repo();
        // Deliberately awkward bytes: trailing space, tabs, CRLF — the store must
        // preserve them exactly (R1, no normalization).
        let yaml = "id: c1\nname: demo  \n\tprimitive: foo\r\n";
        let record = freeze_contract_revision(temp.path(), None, frozen_input("c1", yaml))
            .expect("freeze revision");
        assert_eq!(record.revision, 1);
        assert_eq!(record.state, "frozen");
        assert_eq!(record.predecessor_revision, None);

        let read = contract_revision(temp.path(), "c1", 1)
            .expect("read revision")
            .expect("revision exists");
        assert_eq!(
            read.source_yaml, yaml,
            "source bytes must round-trip verbatim"
        );
        assert_eq!(read.content_hash, record.content_hash);
        assert!(read.lint_clean);
    }

    #[test]
    fn revision_bump_references_predecessor() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\nv: 1\n"))
            .expect("freeze rev 1");
        let bump = FreezeContractRevisionInput {
            contract_id: "c1".to_string(),
            source_yaml: "id: c1\nv: 2\n".to_string(),
            lint_clean: true,
            resolution_kind: Some("revision".to_string()),
            resolution_rationale: Some("addressed the open stop".to_string()),
        };
        let rev2 = freeze_contract_revision(temp.path(), None, bump).expect("freeze rev 2");
        assert_eq!(rev2.revision, 2);
        assert_eq!(rev2.predecessor_revision, Some(1));
        assert_eq!(rev2.resolution_kind.as_deref(), Some("revision"));

        let latest = latest_contract_revision(temp.path(), "c1")
            .expect("latest")
            .expect("has latest");
        assert_eq!(latest.revision, 2);
    }

    /// Hex-decode a lowercase hex string (test helper, mirroring signing.rs).
    fn hex_decode(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("hex byte"))
            .collect()
    }

    /// Verify a locally-signed ledger row's Ed25519 signature at the U1 level:
    /// reconstruct the v1 signing message (`forge-ledger-signature-v1` ...) and
    /// verify it against the stored public key. This proves the row is genuinely
    /// signed WITHOUT depending on `forge doctor`'s per-kind digest wiring, which
    /// is U9's slice.
    fn assert_ledger_signature_verifies(
        connection: &Connection,
        subject_kind: &str,
        subject_id: &str,
        signed_digest: &str,
    ) {
        use ring::signature::{UnparsedPublicKey, ED25519};
        let (public_key_hex, signature_hex, trust_level): (String, String, String) = connection
            .query_row(
                "SELECT public_key, signature, trust_level FROM ledger_signatures
                 WHERE subject_kind = ?1 AND subject_id = ?2 AND signed_digest = ?3
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT 1",
                params![subject_kind, subject_id, signed_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("a signature row must exist for the subject");
        assert_eq!(trust_level, "locally_signed");
        let message = format!(
            "forge-ledger-signature-v1\nsubject_kind={subject_kind}\nsubject_id={subject_id}\nsigned_digest={signed_digest}\n"
        );
        UnparsedPublicKey::new(&ED25519, hex_decode(&public_key_hex))
            .verify(message.as_bytes(), &hex_decode(&signature_hex))
            .expect("the stored Ed25519 signature must verify against the stored public key");
    }

    #[test]
    fn frozen_revision_signature_verifies() {
        let temp = init_native_repo();
        let record = freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        assert_ledger_signature_verifies(
            &connection,
            SUBJECT_KIND_CONTRACT,
            &record.revision_row_id,
            &record.content_hash,
        );
    }

    #[test]
    fn check_forbids_invalid_revision_state() {
        let temp = init_native_repo();
        // A valid contracts head row must exist for the composite FK.
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        let repo_id: String = connection
            .query_row("SELECT id FROM repositories LIMIT 1", [], |row| row.get(0))
            .expect("repo id");
        // `state = 'bogus'` violates the CHECK IN ('draft','frozen') constraint.
        let result = connection.execute(
            "INSERT INTO contract_revisions (
                id, repo_id, contract_id, revision, state, source_yaml, lint_clean, content_hash, created_at_ms
             ) VALUES ('rev_bad', ?1, 'c1', 99, 'bogus', 'x', 1, 'h', 1)",
            params![repo_id],
        );
        assert!(
            result.is_err(),
            "the state CHECK constraint must reject an out-of-vocabulary lifecycle value"
        );
    }

    #[test]
    fn run_records_per_task_completion_state() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let input = RecordContractRunInput {
            contract_id: "c1".to_string(),
            revision: 1,
            base_head: Some("HEAD0".to_string()),
            dependency_stack_json: Some("[]".to_string()),
            outcome: "stopped".to_string(),
            exit_code: 2,
            agent_exit_code: Some(0),
            patch_content_ref: None,
            tasks: vec![
                ContractRunTaskInput {
                    task_id: "t1".to_string(),
                    task_index: 0,
                    outcome: "stopped".to_string(),
                    patch_content_ref: None,
                    agent_exit_code: Some(0),
                    agent_stdout_excerpt: Some("hello from the agent".to_string()),
                    agent_stderr_excerpt: Some("password=[REDACTED]".to_string()),
                },
                ContractRunTaskInput {
                    task_id: "t2".to_string(),
                    task_index: 1,
                    outcome: "skipped".to_string(),
                    patch_content_ref: None,
                    agent_exit_code: None,
                    agent_stdout_excerpt: None,
                    agent_stderr_excerpt: None,
                },
            ],
        };
        let recorded = record_contract_run(temp.path(), None, input).expect("record run");
        assert_eq!(recorded.tasks.len(), 2);

        let read = contract_run(temp.path(), &recorded.run_id)
            .expect("read run")
            .expect("run exists");
        assert_eq!(read.outcome, "stopped");
        assert_eq!(read.exit_code, 2);
        assert_eq!(read.tasks.len(), 2);
        assert_eq!(read.tasks[0].task_id, "t1");
        assert_eq!(read.tasks[0].outcome, "stopped");
        // The captured agent excerpts round-trip on the per-task row (R7/R16).
        assert_eq!(
            read.tasks[0].agent_stdout_excerpt.as_deref(),
            Some("hello from the agent")
        );
        assert_eq!(
            read.tasks[0].agent_stderr_excerpt.as_deref(),
            Some("password=[REDACTED]")
        );
        assert_eq!(read.tasks[1].task_id, "t2");
        assert_eq!(read.tasks[1].outcome, "skipped");
        assert_eq!(read.tasks[1].agent_stdout_excerpt, None);
    }

    #[test]
    fn run_refuses_non_frozen_or_unclean_revision() {
        let temp = init_native_repo();
        // A revision that is frozen but NOT lint-clean must not produce a run (R2).
        let unclean = FreezeContractRevisionInput {
            contract_id: "c1".to_string(),
            source_yaml: "id: c1\n".to_string(),
            lint_clean: false,
            resolution_kind: None,
            resolution_rationale: None,
        };
        freeze_contract_revision(temp.path(), None, unclean).expect("freeze unclean revision");
        let input = RecordContractRunInput {
            contract_id: "c1".to_string(),
            revision: 1,
            base_head: None,
            dependency_stack_json: None,
            outcome: "completed".to_string(),
            exit_code: 0,
            agent_exit_code: Some(0),
            patch_content_ref: None,
            tasks: vec![],
        };
        let error = record_contract_run(temp.path(), None, input).expect_err("must refuse");
        assert!(
            error.to_string().contains("lint-clean frozen"),
            "unexpected error: {error}"
        );
        // U2: the refusal now downcasts to the typed CONTRACT_NOT_FROZEN variant
        // (KTD6 downcast pattern) rather than being a plain anyhow bail.
        let typed = error
            .downcast_ref::<ForgeError>()
            .expect("refusal downcasts to a typed ForgeError");
        assert_eq!(typed.code(), "CONTRACT_NOT_FROZEN");
        assert_eq!(
            *typed,
            ForgeError::ContractNotFrozen {
                contract_id: "c1".to_string(),
                revision: 1,
            }
        );
    }

    #[test]
    fn run_refuses_absent_revision_as_not_frozen() {
        let temp = init_native_repo();
        // No contract exists at all: the run refusal must still be the typed
        // CONTRACT_NOT_FROZEN, collapsing "absent" and "not lint-clean" into one code.
        let input = RecordContractRunInput {
            contract_id: "missing".to_string(),
            revision: 1,
            base_head: None,
            dependency_stack_json: None,
            outcome: "completed".to_string(),
            exit_code: 0,
            agent_exit_code: Some(0),
            patch_content_ref: None,
            tasks: vec![],
        };
        let error = record_contract_run(temp.path(), None, input).expect_err("must refuse");
        let typed = error
            .downcast_ref::<ForgeError>()
            .expect("refusal downcasts to a typed ForgeError");
        assert_eq!(typed.code(), "CONTRACT_NOT_FROZEN");
    }

    #[test]
    fn run_outcome_discriminator_matches_persisted_vocabulary() {
        // The serde snake_case discriminator, its as_str, and the values
        // record_contract_run accepts must not drift (KTD10/R25).
        for outcome in ContractRunOutcome::ALL {
            assert_eq!(
                serde_json::to_value(outcome).expect("serialize"),
                outcome.as_str()
            );
            assert_eq!(ContractRunOutcome::parse(outcome.as_str()), Some(outcome));
        }
        assert!(ContractRunOutcome::parse("bogus").is_none());
        for outcome in ContractVerifyOutcome::ALL {
            assert_eq!(
                serde_json::to_value(outcome).expect("serialize"),
                outcome.as_str()
            );
            assert_eq!(
                ContractVerifyOutcome::parse(outcome.as_str()),
                Some(outcome)
            );
        }
        // The verify discriminator values match R25's inventory exactly.
        assert_eq!(
            ContractVerifyOutcome::ALL.map(|o| o.as_str()),
            ["passed", "fix_failed", "guard_regressed"]
        );
    }

    #[test]
    fn stop_free_text_is_redacted_before_persist() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let input = OpenContractStopInput {
            contract_id: "c1".to_string(),
            revision: 1,
            run_id: None,
            task_id: Some("t1".to_string()),
            what_needed: Some("need API_TOKEN=supersecretvalue to proceed".to_string()),
            why_unanswered: Some("the brief omits it".to_string()),
            kind: Some("missing_primitive".to_string()),
            evidence: Some("src/main.rs:42".to_string()),
            malformed: false,
        };
        let stop = open_contract_stop(temp.path(), None, input).expect("open stop");
        assert_eq!(stop.state, "open");
        assert!(!stop.malformed);
        assert!(
            !stop
                .what_needed
                .as_deref()
                .unwrap()
                .contains("supersecretvalue"),
            "secret must be redacted before the record is written: {:?}",
            stop.what_needed
        );

        // The persisted column is redacted too (redaction happened before write).
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        let stored: String = connection
            .query_row(
                "SELECT what_needed FROM contract_stops WHERE id = ?1",
                params![stop.stop_id],
                |row| row.get(0),
            )
            .expect("query stop");
        assert!(
            !stored.contains("supersecretvalue"),
            "leaked secret: {stored}"
        );
    }

    #[test]
    fn malformed_stop_opens_and_resolve_lifecycle_enforced() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        // Fail-closed ingest: a malformed stop still opens (R8).
        let stop = open_contract_stop(
            temp.path(),
            None,
            OpenContractStopInput {
                contract_id: "c1".to_string(),
                revision: 1,
                run_id: None,
                task_id: None,
                what_needed: None,
                why_unanswered: None,
                kind: None,
                evidence: None,
                malformed: true,
            },
        )
        .expect("open malformed stop");
        assert!(stop.malformed);

        let open = contract_stops(temp.path(), Some("c1"), true).expect("list open");
        assert_eq!(open.len(), 1);

        let resolved = resolve_contract_stop(
            temp.path(),
            None,
            &stop.stop_id,
            "rejection",
            Some("not a real gap; brief already covers it"),
            Some(2),
        )
        .expect("resolve stop");
        assert_eq!(resolved.state, "resolved");
        assert_eq!(resolved.resolving_revision, Some(2));

        // Second resolve is a forbidden lifecycle transition.
        let error =
            resolve_contract_stop(temp.path(), None, &stop.stop_id, "revision", None, Some(3))
                .expect_err("double-resolve must fail");
        assert!(
            error.to_string().contains("not open"),
            "unexpected: {error}"
        );

        // No longer surfaced as open.
        assert!(contract_stops(temp.path(), Some("c1"), true)
            .expect("list open")
            .is_empty());
    }

    #[test]
    fn verdicts_record_and_sign() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let run = record_contract_run(
            temp.path(),
            None,
            RecordContractRunInput {
                contract_id: "c1".to_string(),
                revision: 1,
                base_head: None,
                dependency_stack_json: None,
                outcome: "completed".to_string(),
                exit_code: 0,
                agent_exit_code: Some(0),
                patch_content_ref: None,
                tasks: vec![],
            },
        )
        .expect("record run");
        let verdicts = record_contract_run_verdicts(
            temp.path(),
            None,
            &run.run_id,
            vec![
                ContractRunVerdictInput {
                    task_id: Some("t1".to_string()),
                    verdict_kind: "fix".to_string(),
                    command: Some("cargo test".to_string()),
                    passed: true,
                    detail: None,
                    evidence_id: None,
                },
                ContractRunVerdictInput {
                    task_id: Some("t1".to_string()),
                    verdict_kind: "aggregate".to_string(),
                    command: None,
                    passed: true,
                    detail: None,
                    evidence_id: None,
                },
            ],
        )
        .expect("record verdicts");
        assert_eq!(verdicts.len(), 2);

        let listed = contract_run_verdicts(temp.path(), &run.run_id).expect("list verdicts");
        assert_eq!(listed.len(), 2);

        // Each verdict row's local signature verifies.
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        for verdict in &verdicts {
            assert_ledger_signature_verifies(
                &connection,
                SUBJECT_KIND_CONTRACT_VERDICT,
                &verdict.verdict_id,
                &verdict.content_hash,
            );
        }
    }

    #[test]
    fn acceptance_grammar_is_fail_closed() {
        use AcceptanceCommandCheck::*;
        // Grammar-valid, metacharacter-free: the only accepted shape.
        assert_eq!(check_acceptance_command("cargo test"), Ok);
        assert_eq!(
            check_acceptance_command("cargo test -p forge-cli --test forge_contract"),
            Ok
        );
        assert_eq!(
            check_acceptance_command("cargo clippy --workspace --all-targets -- -D warnings"),
            Ok
        );
        assert!(acceptance_command_is_safe("cargo build"));
        // Non-cargo / wrong subcommand -> grammar violation.
        assert_eq!(check_acceptance_command("echo hi"), GrammarViolation);
        assert_eq!(check_acceptance_command("cargo publish"), GrammarViolation);
        // Word boundary: `cargo testfoo` is not `cargo test`.
        assert_eq!(check_acceptance_command("cargo testfoo"), GrammarViolation);
        // Grammar-valid prefix but a shell metacharacter reaches the eval sink.
        assert_eq!(
            check_acceptance_command("cargo test; rm -rf /"),
            ShellMetacharacter
        );
        assert_eq!(
            check_acceptance_command("cargo test && echo pwned"),
            ShellMetacharacter
        );
        assert_eq!(
            check_acceptance_command("cargo test $(whoami)"),
            ShellMetacharacter
        );
        assert!(!acceptance_command_is_safe("cargo test `id`"));
    }

    #[test]
    fn request_id_replay_does_not_duplicate_run() {
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let make_input = || RecordContractRunInput {
            contract_id: "c1".to_string(),
            revision: 1,
            base_head: None,
            dependency_stack_json: None,
            outcome: "completed".to_string(),
            exit_code: 0,
            agent_exit_code: Some(0),
            patch_content_ref: None,
            tasks: vec![],
        };
        let first = record_contract_run(temp.path(), Some("req-1".to_string()), make_input())
            .expect("first run");
        // Replaying the same request id must not insert a second run row.
        let replay = record_contract_run(temp.path(), Some("req-1".to_string()), make_input());
        assert!(
            replay.is_err(),
            "replay is surfaced as a RequestIdReplay for the caller to fold to the original"
        );
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM contract_runs", [], |row| row.get(0))
            .expect("count runs");
        assert_eq!(count, 1, "replay must not create a duplicate run");
        // Sanity: the original run is intact and readable.
        assert!(contract_run(temp.path(), &first.run_id)
            .expect("read run")
            .is_some());
    }

    #[test]
    fn brief_is_byte_stable_and_emits_neighbors_in_declared_order() {
        let temp = init_native_repo();
        freeze_contract_revision(
            temp.path(),
            None,
            frozen_input("_global-policy", "policy\n"),
        )
        .expect("freeze policy");
        freeze_contract_revision(temp.path(), None, frozen_input("ccx-a", "id: ccx-a\n"))
            .expect("freeze a");
        freeze_contract_revision(temp.path(), None, frozen_input("ccx-b", "id: ccx-b\n"))
            .expect("freeze b");
        let task = "id: ccx-task\nneighbors:\n  - ccx-a\n  - ccx-b\n";
        freeze_contract_revision(temp.path(), None, frozen_input("ccx-task", task))
            .expect("freeze task");

        let first = contract_brief(temp.path(), "ccx-task", None).expect("brief");
        let second = contract_brief(temp.path(), "ccx-task", None).expect("brief again");
        assert_eq!(first.brief, second.brief, "same inputs must be byte-stable");

        // The exact bytes: policy, task, then neighbors in declared order (a, b).
        let expected = "--- GLOBAL POLICY (normative) ---\npolicy\n\
\n--- TASK CONTRACT (normative) ---\nid: ccx-task\nneighbors:\n  - ccx-a\n  - ccx-b\n\
\n--- NEIGHBOR CONTRACT (normative): ccx-a ---\nid: ccx-a\n\
\n--- NEIGHBOR CONTRACT (normative): ccx-b ---\nid: ccx-b\n\n";
        assert_eq!(first.brief, expected);
        assert_eq!(first.neighbors.len(), 2);
        assert_eq!(first.neighbors[0].id, "ccx-a");
        assert_eq!(first.neighbors[1].id, "ccx-b");
        assert!(first.neighbors.iter().all(|n| n.present));
    }

    #[test]
    fn brief_missing_neighbor_emits_marker_and_still_succeeds() {
        let temp = init_native_repo();
        freeze_contract_revision(
            temp.path(),
            None,
            frozen_input("_global-policy", "policy\n"),
        )
        .expect("freeze policy");
        freeze_contract_revision(temp.path(), None, frozen_input("ccx-a", "id: ccx-a\n"))
            .expect("freeze a");
        // ccx-ghost is declared but never frozen — the native missing case.
        let task = "id: ccx-task\nneighbors:\n  - ccx-a\n  - ccx-ghost\n";
        freeze_contract_revision(temp.path(), None, frozen_input("ccx-task", task))
            .expect("freeze task");

        let brief = contract_brief(temp.path(), "ccx-task", None).expect("brief");
        assert!(
            brief.brief.contains(
                "--- NEIGHBOR CONTRACT MISSING: ccx-ghost (surface as unknown, do not guess) ---\n\n"
            ),
            "missing marker must be byte-exact: {:?}",
            brief.brief
        );
        assert!(brief
            .brief
            .contains("--- NEIGHBOR CONTRACT (normative): ccx-a ---\n"));
        assert!(brief.neighbors[0].present);
        assert!(!brief.neighbors[1].present);
        assert_eq!(brief.neighbors[1].revision, None);
    }

    #[test]
    fn brief_without_global_policy_is_a_not_frozen_refusal() {
        let temp = init_native_repo();
        freeze_contract_revision(
            temp.path(),
            None,
            frozen_input("ccx-task", "id: ccx-task\n"),
        )
        .expect("freeze task");
        // Fail-closed: no frozen global policy → typed CONTRACT_NOT_FROZEN (the
        // native analogue of ccx-brief.py's fail-closed exit on a missing policy).
        let error = contract_brief(temp.path(), "ccx-task", None).expect_err("must refuse");
        let typed = error
            .downcast_ref::<ForgeError>()
            .expect("refusal downcasts to a typed ForgeError");
        assert_eq!(typed.code(), "CONTRACT_NOT_FROZEN");
    }

    #[test]
    fn run_with_stop_records_both_atomically_and_signed() {
        // F1: the stopped run and its stop are written in ONE transaction, so a stop
        // failure can never orphan a `stopped` run. Both rows are present, each
        // individually signed, and the stop's run_id is set atomically to the new run.
        let temp = init_native_repo();
        freeze_contract_revision(temp.path(), None, frozen_input("c1", "id: c1\n"))
            .expect("freeze revision");
        let run_input = RecordContractRunInput {
            contract_id: "c1".to_string(),
            revision: 1,
            base_head: Some("HEAD0".to_string()),
            dependency_stack_json: None,
            outcome: "stopped".to_string(),
            exit_code: 2,
            agent_exit_code: Some(0),
            patch_content_ref: None,
            tasks: vec![ContractRunTaskInput {
                task_id: "c1".to_string(),
                task_index: 0,
                outcome: "stopped".to_string(),
                patch_content_ref: None,
                agent_exit_code: Some(0),
                agent_stdout_excerpt: None,
                agent_stderr_excerpt: None,
            }],
        };
        // Caller passes run_id: None; the combined writer sets it to the new run id.
        let stop_input = OpenContractStopInput {
            contract_id: "c1".to_string(),
            revision: 1,
            run_id: None,
            task_id: Some("c1".to_string()),
            what_needed: Some("need the shape".to_string()),
            why_unanswered: Some("brief omits it".to_string()),
            kind: Some("blocking".to_string()),
            evidence: Some("src/lib.rs:1".to_string()),
            malformed: false,
        };
        let (run, stop) = record_contract_run_with_stop(temp.path(), None, run_input, stop_input)
            .expect("atomic run + stop");
        assert_eq!(run.outcome, "stopped");
        assert_eq!(
            stop.run_id.as_deref(),
            Some(run.run_id.as_str()),
            "the stop links to the freshly-created run id atomically"
        );

        // Both rows exist; the stop is discoverable as open and references the run.
        let open = contract_stops(temp.path(), Some("c1"), true).expect("list open");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].run_id.as_deref(), Some(run.run_id.as_str()));
        assert!(contract_run(temp.path(), &run.run_id)
            .expect("read run")
            .is_some());

        // Each row carries its own verifiable local signature (both chained in one txn).
        let database_path = temp.path().join(".forge/forge.db");
        let connection = crate::open_connection(&database_path).expect("open db");
        assert_ledger_signature_verifies(
            &connection,
            SUBJECT_KIND_CONTRACT_RUN,
            &run.run_id,
            &run.content_hash,
        );
        assert_ledger_signature_verifies(
            &connection,
            SUBJECT_KIND_CONTRACT_STOP,
            &stop.stop_id,
            &stop.content_hash,
        );
    }
}
