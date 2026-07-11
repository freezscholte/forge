-- CCX native contracts (NER U1): contracts, runs, stops, and verdicts become
-- first-class signed ledger object kinds. Follows the 021_embargo_workflow.sql
-- conventions: `repo_id` REFERENCES repositories(id), lifecycle-state CHECK
-- constraints, `created_at_ms` / `updated_at_ms` INTEGER millis, and companion
-- child tables (per-task run rows, per-command verdict rows).
--
-- Every row's content is folded into a tamper-evident `content_hash` and signed
-- via LocalSigner::sign_subject under the per-kind subject_kind strings
-- "contract" (a frozen revision), "contract_run", "contract_stop", and
-- "contract_run_verdict" -- the write is chained through the op-log spine so a
-- later swap of any content_hash is caught by `forge doctor`'s re-walk. See
-- crates/forge-store/src/contract.rs.

-- Widen the ledger_signatures subject_kind CHECK to admit the contract-family
-- kinds. `sign_subject` uses INSERT OR IGNORE, so a subject_kind outside the
-- CHECK set is SILENTLY dropped (SQLite's IGNORE swallows CHECK violations) --
-- leaving an unsigned row. Rebuild the table (the 014/017 pattern) so contract,
-- contract_run, contract_stop, and contract_run_verdict rows can be signed.
CREATE TABLE ledger_signatures_022 (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    subject_kind TEXT NOT NULL CHECK (
        subject_kind IN (
            'evidence', 'decision', 'commit', 'sync_merge_commit',
            'contract', 'contract_run', 'contract_stop', 'contract_run_verdict'
        )
    ),
    subject_id TEXT NOT NULL,
    signed_digest TEXT NOT NULL,
    signature_alg TEXT NOT NULL CHECK (signature_alg = 'ed25519'),
    public_key TEXT NOT NULL,
    key_fingerprint TEXT NOT NULL,
    signature TEXT NOT NULL,
    trust_level TEXT NOT NULL CHECK (
        trust_level IN ('locally_signed', 'hosted_runner_signed', 'third_party_attested')
    ),
    created_at_ms INTEGER NOT NULL,
    UNIQUE(repo_id, subject_kind, subject_id, signed_digest, key_fingerprint, trust_level)
);

INSERT INTO ledger_signatures_022 (
    id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
    public_key, key_fingerprint, signature, trust_level, created_at_ms
)
SELECT
    id, repo_id, subject_kind, subject_id, signed_digest, signature_alg,
    public_key, key_fingerprint, signature, trust_level, created_at_ms
FROM ledger_signatures;

DROP TABLE ledger_signatures;
ALTER TABLE ledger_signatures_022 RENAME TO ledger_signatures;

CREATE INDEX idx_ledger_signatures_subject
ON ledger_signatures(repo_id, subject_kind, subject_id);

-- A contract's identity plus a head pointer to its highest frozen revision.
-- Authoring (the YAML source) lives per-revision in `contract_revisions` -- this
-- table is the queryable anchor and the composite-FK parent for revisions.
CREATE TABLE IF NOT EXISTS contracts (
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    contract_id TEXT NOT NULL,
    -- Highest frozen revision integer that exists (0 before the first freeze).
    latest_revision INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (repo_id, contract_id)
);

-- An immutable, frozen revision of a contract. R1: stores the exact source YAML
-- bytes verbatim (no re-serialization or normalization) so brief emission can
-- reproduce harness output byte-for-byte. R2: only lint-clean frozen revisions
-- can produce briefs or runs. A revision-bump row references its predecessor,
-- an explicit-rejection bump (R10) carries `resolution_kind`/`resolution_rationale`
-- and reuses the predecessor's content unchanged.
CREATE TABLE IF NOT EXISTS contract_revisions (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    contract_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('draft', 'frozen')),
    -- Exact source YAML bytes, stored verbatim (R1).
    source_yaml TEXT NOT NULL,
    -- Only a lint-clean frozen revision produces briefs/runs (R2/R4).
    lint_clean INTEGER NOT NULL DEFAULT 0,
    -- The prior revision integer this one bumps from (NULL for revision 1).
    predecessor_revision INTEGER,
    -- Set when this revision resolves an open stop (R10): a content change
    -- ('revision') or a content-preserving explicit rejection ('rejection').
    resolution_kind TEXT CHECK (resolution_kind IN ('revision', 'rejection')),
    resolution_rationale TEXT,
    content_hash TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (repo_id, contract_id, revision),
    FOREIGN KEY (repo_id, contract_id) REFERENCES contracts(repo_id, contract_id)
);

CREATE INDEX IF NOT EXISTS idx_contract_revisions_contract
ON contract_revisions(repo_id, contract_id, revision);

-- A dependency-ordered chain run against one frozen contract revision. R7:
-- records contract id + revision, base state, dependency stack, outcome, and
-- captured artifacts (the produced patch content ref, agent exit metadata).
-- R25: `outcome` is the machine-readable run discriminator, and `exit_code` mirrors
-- the process exit code (0 completed, 1 failed, 2 stopped, 3 blast_violation).
CREATE TABLE IF NOT EXISTS contract_runs (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    contract_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    base_head TEXT,
    -- Per-id acknowledged dependency stack the run was licensed against (R20).
    dependency_stack_json TEXT,
    outcome TEXT NOT NULL CHECK (
        outcome IN ('completed', 'stopped', 'blast_violation', 'failed')
    ),
    exit_code INTEGER NOT NULL,
    -- The opaque agent subprocess exit status captured during the run (R7).
    agent_exit_code INTEGER,
    -- The redacted, full native content object holding the produced patch (R27).
    patch_content_ref TEXT,
    request_id TEXT,
    content_hash TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_contract_runs_contract
ON contract_runs(repo_id, contract_id, revision, created_at_ms);

-- Per-task completion state for a run (KTD9): a rerun-after-triage resumes from
-- the halted task by replaying recorded, per-id acknowledged completed-task
-- outputs. Each task row carries its own outcome and produced-patch ref.
CREATE TABLE IF NOT EXISTS contract_run_tasks (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    run_id TEXT NOT NULL REFERENCES contract_runs(id),
    task_id TEXT NOT NULL,
    -- Position of this task in the dependency-ordered chain.
    task_index INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (
        outcome IN ('pending', 'completed', 'stopped', 'failed', 'skipped')
    ),
    patch_content_ref TEXT,
    agent_exit_code INTEGER,
    -- Redacted excerpts of the agent subprocess stdout/stderr (R7 exit metadata,
    -- R16 defense-in-depth). Captured through the forge-evidence redact pass and
    -- EXCERPT_LIMIT cap before storage, then folded into the run content hash and
    -- signature. NULL when no agent ran for this task (a resumed or skipped task).
    agent_stdout_excerpt TEXT,
    agent_stderr_excerpt TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (run_id, task_id)
);

CREATE INDEX IF NOT EXISTS idx_contract_run_tasks_run
ON contract_run_tasks(run_id, task_index);

-- A typed stop record ingested from an agent-filed UNKNOWN.md (Leg 1, R8). The
-- four required fields (what is needed, why the brief does not answer it, kind,
-- file:line evidence) are secret-redacted before hashing/signing (R16/KTD3).
-- Ingestion is fail-closed: a stop always opens even when the four fields cannot
-- be fully extracted — `malformed` flags a best-effort record (a flag, not a
-- failure per R8/R25). The open/resolved lifecycle enforces triage-before-rerun
-- (Leg 3, R10). Resolution is a content revision or an explicit rejection, both
-- recorded here with the revision that resolved it.
CREATE TABLE IF NOT EXISTS contract_stops (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    contract_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    run_id TEXT REFERENCES contract_runs(id),
    task_id TEXT,
    -- The four required, secret-redacted free-text fields (R8/R16).
    what_needed TEXT,
    why_unanswered TEXT,
    kind TEXT,
    evidence TEXT,
    -- Best-effort ingest flag: fields could not be fully extracted (R8/R25).
    malformed INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL CHECK (state IN ('open', 'resolved')),
    resolution_kind TEXT CHECK (resolution_kind IN ('revision', 'rejection')),
    resolution_rationale TEXT,
    -- The frozen revision that resolved this stop (R10).
    resolving_revision INTEGER,
    content_hash TEXT NOT NULL,
    request_id TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_contract_stops_open
ON contract_stops(repo_id, contract_id, state, updated_at_ms);

-- Per-command verdict rows plus an aggregate for a run's blast check and
-- fix/guard verification (KTD4, R12/R13). Verdicts are contract-owned (not
-- forge-policy CheckSpec): `verdict_kind` distinguishes the blast check from the
-- fix/guard command lists and the aggregate roll-up. `evidence_id` links the
-- forge-evidence capture that produced the outcome (R16).
CREATE TABLE IF NOT EXISTS contract_run_verdicts (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL REFERENCES repositories(id),
    run_id TEXT NOT NULL REFERENCES contract_runs(id),
    task_id TEXT,
    verdict_kind TEXT NOT NULL CHECK (
        verdict_kind IN ('blast', 'fix', 'guard', 'aggregate')
    ),
    -- The command that produced this verdict -- NULL for the aggregate roll-up.
    command TEXT,
    passed INTEGER NOT NULL,
    -- Human-readable detail (e.g. the forbidden path for a blast violation).
    detail TEXT,
    evidence_id TEXT,
    content_hash TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_contract_run_verdicts_run
ON contract_run_verdicts(run_id, created_at_ms);
