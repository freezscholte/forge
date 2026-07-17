//! CCX native contracts (U3): the `forge contract lint|freeze` CLI family.
//!
//! This is the response home for the contract command family (KTD5); `main.rs`
//! carries one dispatch arm and `core.rs` only the bounded `is_mutating_command`
//! wiring entry. Lint ports the six rule families of `tools/ccx/ccx-lint.py`
//! faithfully, tightened to native-only v1 strictness:
//!
//! - unknown top-level keys are ERRORS, not tolerated (R3 of the plan);
//! - `ccx.contract.v0` is rejected outright (out of scope for the native surface);
//! - the acceptance-command grammar (R6/R15) delegates to the single source of
//!   truth `forge_store::check_acceptance_command`, which U7's verifier reuses so
//!   a lint-accepted string is exactly an execution-accepted string.
//!
//! Every operand path is canonicalized at the argument boundary (R19). Freeze
//! lints first — a contract that fails lint never reaches the ledger — then
//! records the exact source bytes via U1's `freeze_contract_revision` (R1/R2).
//! A repo-level `_global-policy.yaml` (`kind: global_policy`) is linted with the
//! reduced set of applicable rules and frozen under the reserved id
//! `_global-policy`, so U4's brief emitter can retrieve it.
//!
//! Note (repo scope): lint and freeze both route through `command_result`, which
//! brings the schema to head and therefore requires an initialized forge repo —
//! unlike the standalone Python harness, which needed only a git root. This is
//! deliberate: lint's satisfiability (R2) and primitive (R3) rules read the repo
//! working tree (`scripts/check-rust-line-count.sh`, `crates/<crate>/src`) and
//! the native surface is repo-scoped.

use anyhow::{anyhow, Context, Result};
use forge_content_native::{
    diff_native_content_refs, materialize_content_ref, merge_native_content_refs,
    snapshot_worktree_into_store_excluding, DiffOptions, NativeObjectStore,
};
use forge_protocol::ResponseEnvelope;
use forge_store::{
    ContractIntegrationRecord, ContractRunTaskInput, ContractRunVerdictInput,
    OpenContractStopInput, RecordContractRunInput,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use crate::commands::contract_blast::{self, BlastViolationClass};
use crate::{
    command_result, current_base, owner_base_content_ref, ContractArgs, ContractBriefArgs,
    ContractCommand, ContractFreezeArgs, ContractIntegrateArgs, ContractLintArgs, ContractRunArgs,
    ForgeError,
};

/// Reserved ledger contract id for the repo-level global policy file. Shared with
/// the store (`forge_store::GLOBAL_POLICY_CONTRACT_ID`) so freeze and brief agree.
const GLOBAL_POLICY_ID: &str = forge_store::GLOBAL_POLICY_CONTRACT_ID;

/// The task-instruction stop-rule wording (R6), verbatim from the single harness
/// source `tools/ccx/prompts/task-instruction.txt`. `ccx-brief.py` does NOT emit
/// this — `run-task.sh` `cat`s the file onto the brief at RUN time — so
/// `forge contract brief` keeps byte-parity with the Python emitter by NOT
/// appending it, and U5's prompt assembly appends this constant verbatim instead.
/// `include_str!` guarantees the wording can never drift from the harness file;
/// when the R21 retirement criterion is met and `tools/ccx` is removed, inline the
/// literal here. U5's prompt assembly (`assemble_prompt`) appends it verbatim after
/// the store-emitted brief — the first non-test consumer, so the previous
/// `dead_code` allow is removed.
pub(crate) const CONTRACT_TASK_INSTRUCTION: &str =
    include_str!("../../../../tools/ccx/prompts/task-instruction.txt");

/// Required top-level keys for a task contract (R1 shape). Mirrors
/// `ccx-lint.py`'s `REQUIRED_KEYS`.
const REQUIRED_KEYS: [&str; 9] = [
    "schema",
    "id",
    "revision",
    "ticket",
    "task",
    "interface",
    "acceptance",
    "allowed_changes",
    "authority",
];

/// The full set of recognized top-level keys for a task contract. Native-only
/// v1 strictness: any key outside this set is an error (the Python harness
/// tolerated unknown keys; the native port must not — plan R3).
const ALLOWED_KEYS: [&str; 15] = [
    "schema",
    "id",
    "revision",
    "ticket",
    "task",
    "interface",
    "invariants",
    "acceptance",
    "negative_constraints",
    "neighbors",
    "depends_on",
    "primitives",
    "exclusion_contract",
    "allowed_changes",
    "authority",
];

const SCHEMA_V0: &str = "ccx.contract.v0";
const SCHEMA_V1: &str = "ccx.contract.v1";

/// Filesystem-enumeration signal words (R5). Character-for-character the
/// `ENUMERATION_SIGNALS` list in `ccx-lint.py`.
const ENUMERATION_SIGNALS: [&str; 7] = [
    "read_dir",
    "walk",
    "enumerate",
    "scan",
    "drift",
    "workspace files",
    "directory tree",
];

const EXCLUSION_DOC: &str = "docs/solutions/architecture-patterns/\
filesystem-enumeration-shared-exclusion-contract.md";

const SKIP_DIRS: [&str; 6] = [
    ".git",
    "target",
    ".forge",
    "node_modules",
    ".venv",
    "__pycache__",
];

/// cargo-test flags that consume a value (so the value is not read as a filter).
const CARGO_VALUE_FLAGS: [&str; 15] = [
    "-p",
    "--package",
    "--test",
    "--features",
    "--exclude",
    "--bin",
    "--example",
    "--bench",
    "--profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--jobs",
    "-j",
    "--color",
];

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

pub(crate) fn contract_response(
    request_id: Option<String>,
    args: ContractArgs,
) -> ResponseEnvelope {
    match args.command {
        ContractCommand::Lint(args) => lint_response(request_id, args),
        ContractCommand::Freeze(args) => freeze_response(request_id, args),
        ContractCommand::Brief(args) => brief_response(request_id, args),
        ContractCommand::Run(args) => run_response(request_id, args),
        ContractCommand::Integrate(args) => integrate_response(request_id, args),
        ContractCommand::Verify(args) => {
            crate::commands::contract_verify::verify_response(request_id, args)
        }
    }
}

/// `forge contract brief <contract-id>` — read-only (no repo lock). Emits the
/// byte-stable brief for a frozen revision (R5): global policy, task contract, and
/// declared neighbors in order, byte-for-byte identical to `tools/ccx/ccx-brief.py`
/// (R5). The `--json` envelope carries the brief text in `data.brief`; `--out`
/// writes it to a file; plain mode prints it verbatim to stdout. The R6
/// task-instruction wording is intentionally NOT appended here (see
/// `CONTRACT_TASK_INSTRUCTION`), preserving Python parity.
fn brief_response(request_id: Option<String>, args: ContractBriefArgs) -> ResponseEnvelope {
    command_result("contract brief", request_id, move |cwd, _| {
        let record = forge_store::contract_brief(&cwd, &args.contract_id, args.revision)?;
        let out_written = match &args.out {
            Some(raw) => {
                // Resolve (not canonicalize) the write target: the file may not
                // exist yet, so canonicalize the parent directory when it exists
                // and rejoin the filename; otherwise fall back to cwd-joining.
                // Full R19 canonicalization applies to read operands elsewhere.
                let joined = if raw.is_absolute() {
                    raw.clone()
                } else {
                    cwd.join(raw)
                };
                let path = match (joined.parent(), joined.file_name()) {
                    (Some(parent), Some(name)) if parent.exists() => parent
                        .canonicalize()
                        .map(|p| p.join(name))
                        .unwrap_or(joined.clone()),
                    _ => joined.clone(),
                };
                std::fs::write(&path, record.brief.as_bytes())
                    .with_context(|| format!("cannot write brief to {}", path.display()))?;
                Some(path.display().to_string())
            }
            None => None,
        };
        let mut data = json!({
            "contract_id": record.contract_id,
            "revision": record.revision,
            "global_policy_revision": record.global_policy_revision,
            "neighbors": serde_json::to_value(&record.neighbors)?,
            "brief": record.brief,
        });
        if let Some(out) = out_written {
            data["out"] = json!(out);
        }
        Ok((None, data, Vec::new()))
    })
}

/// `forge contract lint <path>` — read-only. Surfaces findings machine-readably
/// in `data`; a grammar violation maps to the typed `CONTRACT_GRAMMAR_VIOLATION`
/// (AE6) and any other error finding to `CONTRACT_LINT_FAILED` (AE4).
fn lint_response(request_id: Option<String>, args: ContractLintArgs) -> ResponseEnvelope {
    command_result("contract lint", request_id, move |cwd, _| {
        let path = canonicalize_operand(&cwd, &args.path)?;
        let repo_root = forge_store::repository_root_path(&cwd)?;
        let outcome = lint_contract_file(&path, &repo_root)?;
        outcome.ensure_lint_clean()?; // typed refusal on any error finding
        Ok((
            None,
            json!({
                "contract": path.display().to_string(),
                "contract_id": outcome.contract_id,
                "verdict": outcome.verdict(),
                "findings": outcome.findings_json(),
            }),
            Vec::new(),
        ))
    })
}

/// `forge contract freeze <path>` — mutating (R18 replay via `command_result`).
/// Lints first; a contract that fails lint never freezes. Records the EXACT
/// source bytes as a signed revision (R1) and reads back as revision N.
fn freeze_response(request_id: Option<String>, args: ContractFreezeArgs) -> ResponseEnvelope {
    command_result("contract freeze", request_id, move |cwd, request_id| {
        let path = canonicalize_operand(&cwd, &args.path)?;
        let repo_root = forge_store::repository_root_path(&cwd)?;
        let outcome = lint_contract_file(&path, &repo_root)?;
        outcome.ensure_lint_clean()?; // no frozen revision on a lint failure

        // Exact source bytes, verbatim (R1). UTF-8 is required; a non-UTF-8 file
        // would already have failed to parse as YAML in lint.
        let source_yaml = read_source_string(&path)?;
        let record = forge_store::freeze_contract_revision(
            &cwd,
            request_id,
            forge_store::FreezeContractRevisionInput {
                contract_id: outcome.contract_id.clone(),
                source_yaml,
                lint_clean: true,
                resolution_kind: None,
                resolution_rationale: None,
            },
        )?;
        Ok((
            None,
            json!({
                "revision": serde_json::to_value(&record)?,
                "verdict": outcome.verdict(),
                "findings": outcome.findings_json(),
            }),
            Vec::new(),
        ))
    })
}

/// Canonicalize an operand path at the argument boundary (R19). Relative paths
/// resolve against the command's working directory.
fn canonicalize_operand(cwd: &Path, raw: &Path) -> Result<PathBuf> {
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    };
    std::fs::canonicalize(&candidate)
        .with_context(|| format!("cannot resolve contract path {}", candidate.display()))
}

fn read_source_string(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("cannot read contract {}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| anyhow!("contract {} is not valid UTF-8", path.display()))
}

// ---------------------------------------------------------------------------
// Lint outcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Finding {
    rule: &'static str,
    severity: Severity,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Error,
    Warning,
    Note,
}

impl Severity {
    fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Note => "note",
        }
    }
}

struct LintOutcome {
    contract_id: String,
    findings: Vec<Finding>,
    /// First acceptance command that failed the grammar (drives the typed
    /// `CONTRACT_GRAMMAR_VIOLATION`).
    grammar_violation: Option<String>,
}

impl LintOutcome {
    fn has_error(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Error)
    }

    fn verdict(&self) -> &'static str {
        if self.has_error() {
            "errors"
        } else if self
            .findings
            .iter()
            .any(|f| f.severity == Severity::Warning)
        {
            "warnings"
        } else {
            "clean"
        }
    }

    fn findings_json(&self) -> Vec<Value> {
        self.findings
            .iter()
            .map(|f| {
                json!({
                    "rule": f.rule,
                    "severity": f.severity.as_str(),
                    "message": f.message,
                })
            })
            .collect()
    }

    /// Convert a failing lint into a typed refusal (grammar first, so AE6 gets
    /// `CONTRACT_GRAMMAR_VIOLATION`; otherwise `CONTRACT_LINT_FAILED`).
    fn ensure_lint_clean(&self) -> Result<()> {
        if let Some(command) = &self.grammar_violation {
            return Err(ForgeError::ContractGrammarViolation {
                command: command.clone(),
            }
            .into());
        }
        if self.has_error() {
            let violations = self
                .findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .map(|f| format!("[{}] {}", f.rule, f.message))
                .collect();
            return Err(ForgeError::ContractLintFailed { violations }.into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Linter
// ---------------------------------------------------------------------------

struct Linter<'a> {
    /// Canonical path of the contract file under lint.
    path: &'a Path,
    /// Directory that resolves neighbor / depends_on ids (the file's parent).
    contracts_dir: PathBuf,
    /// Repo root for cap / primitive / file-enumeration rules.
    repo_root: &'a Path,
    findings: Vec<Finding>,
    grammar_violation: Option<String>,
    repo_files: Option<Vec<String>>,
}

/// Lint a contract file and return its outcome. A read/parse failure is itself an
/// error finding (fail-closed, like the harness).
fn lint_contract_file(path: &Path, repo_root: &Path) -> Result<LintOutcome> {
    let contracts_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut linter = Linter {
        path,
        contracts_dir,
        repo_root,
        findings: Vec::new(),
        grammar_violation: None,
        repo_files: None,
    };
    let contract_id = linter.run();
    Ok(LintOutcome {
        contract_id,
        findings: linter.findings,
        grammar_violation: linter.grammar_violation,
    })
}

impl Linter<'_> {
    fn add(&mut self, rule: &'static str, severity: Severity, message: impl Into<String>) {
        self.findings.push(Finding {
            rule,
            severity,
            message: message.into(),
        });
    }

    /// Run the applicable rules and return the derived contract id (empty on an
    /// unusable file — the caller still surfaces the error findings).
    fn run(&mut self) -> String {
        let bytes = match std::fs::read(self.path) {
            Ok(bytes) => bytes,
            Err(err) => {
                self.add(
                    "R1",
                    Severity::Error,
                    format!("cannot read contract: {err}"),
                );
                return String::new();
            }
        };
        let contract: Value = match serde_yaml::from_slice(&bytes) {
            Ok(value) => value,
            Err(err) => {
                self.add(
                    "R1",
                    Severity::Error,
                    format!("contract is not valid YAML: {err}"),
                );
                return String::new();
            }
        };
        let Some(map) = contract.as_object() else {
            self.add("R1", Severity::Error, "contract is not a YAML mapping");
            return String::new();
        };

        // Global policy file: linted with only the applicable rules (mapping +
        // v1 schema), frozen under the reserved id. Its body is intentionally
        // freeform, so task-contract shape/rules do not apply.
        if map.get("kind").and_then(Value::as_str) == Some("global_policy") {
            self.lint_global_policy(&contract);
            return GLOBAL_POLICY_ID.to_string();
        }

        let contract_id = map
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let shape_ok = self.rule1_shape(&contract);
        let (fix, guard) = self.normalize_acceptance(&contract);
        if !shape_ok {
            return contract_id; // shape broken; deeper rules would just cascade
        }
        self.rule2_satisfiability(&contract);
        self.rule3_primitives(&contract);
        self.rule4_acceptance(&fix, &guard, &contract);
        self.rule5_exclusion(&contract);
        self.rule6_grammar(&fix, &guard);
        contract_id
    }

    fn lint_global_policy(&mut self, contract: &Value) {
        match contract.get("schema").and_then(Value::as_str) {
            Some(SCHEMA_V1) => {}
            Some(SCHEMA_V0) => self.add(
                "R1",
                Severity::Error,
                format!("global policy uses {SCHEMA_V0}, which is out of scope for the native surface (expected {SCHEMA_V1})"),
            ),
            Some(other) => self.add(
                "R1",
                Severity::Error,
                format!("global policy has unknown schema {other:?} (expected {SCHEMA_V1})"),
            ),
            None => self.add(
                "R1",
                Severity::Error,
                format!("global policy is missing schema (expected {SCHEMA_V1})"),
            ),
        }
    }

    // ---- R1: parse + shape -------------------------------------------------

    fn rule1_shape(&mut self, contract: &Value) -> bool {
        let map = contract.as_object().expect("checked mapping");
        let missing: Vec<&str> = REQUIRED_KEYS
            .iter()
            .copied()
            .filter(|key| !map.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            self.add(
                "R1",
                Severity::Error,
                format!("missing required keys: {}", missing.join(", ")),
            );
        }

        // Native-only v1 strictness: any unrecognized top-level key is an error
        // that names the key(s) (AE4).
        let unknown: Vec<&str> = map
            .keys()
            .map(String::as_str)
            .filter(|key| !ALLOWED_KEYS.contains(key))
            .collect();
        if !unknown.is_empty() {
            self.add(
                "R1",
                Severity::Error,
                format!("unrecognized top-level key(s): {}", unknown.join(", ")),
            );
        }

        // Schema: v1 only. v0 is out of scope for the native surface.
        match map.get("schema").and_then(Value::as_str) {
            Some(SCHEMA_V1) => {}
            Some(SCHEMA_V0) => self.add(
                "R1",
                Severity::Error,
                format!(
                    "{SCHEMA_V0} is out of scope for the native surface (expected {SCHEMA_V1})"
                ),
            ),
            Some(other) => self.add(
                "R1",
                Severity::Error,
                format!("unknown schema {other:?} (expected {SCHEMA_V1})"),
            ),
            None => {
                if map.contains_key("schema") {
                    self.add("R1", Severity::Error, "schema must be a string");
                }
            }
        }

        // id / filename correspondence: `ccx-<name>` must live in `<name>.yaml`
        // so neighbor / depends_on resolution is sound.
        if let Some(id) = map.get("id").and_then(Value::as_str) {
            if let Some(name) = id.strip_prefix("ccx-") {
                let expected = format!("{name}.yaml");
                let actual = self
                    .path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if actual != expected {
                    self.add(
                        "R1",
                        Severity::Error,
                        format!(
                            "id {id:?} requires filename {expected:?} for id resolution, but the file is {actual:?}"
                        ),
                    );
                }
            } else {
                self.add(
                    "R1",
                    Severity::Error,
                    format!("id {id:?} must be of the form ccx-<name>"),
                );
            }
        }

        // neighbors / depends_on must be lists of resolvable ids.
        for key in ["neighbors", "depends_on"] {
            match map.get(key) {
                None | Some(Value::Null) => {}
                Some(Value::Array(ids)) => {
                    for cid in ids {
                        let resolvable = cid
                            .as_str()
                            .map(|s| self.resolve_id(s).is_some())
                            .unwrap_or(false);
                        if !resolvable {
                            self.add(
                                "R1",
                                Severity::Error,
                                format!(
                                    "{key} id {cid} does not resolve to a contract file in {}",
                                    self.contracts_dir.display()
                                ),
                            );
                        }
                    }
                }
                Some(_) => self.add(
                    "R1",
                    Severity::Error,
                    format!("{key}: must be a list of contract ids"),
                ),
            }
        }

        self.rule1_cycles(contract);
        missing.is_empty()
    }

    fn resolve_id(&self, cid: &str) -> Option<PathBuf> {
        let name = cid.strip_prefix("ccx-")?;
        let path = self.contracts_dir.join(format!("{name}.yaml"));
        path.is_file().then_some(path)
    }

    fn rule1_cycles(&mut self, contract: &Value) {
        let Some(start) = contract.get("id").and_then(Value::as_str) else {
            return;
        };
        // DFS over the depends_on graph reachable from this contract.
        let mut visiting: Vec<String> = Vec::new();
        let mut done: BTreeSet<String> = BTreeSet::new();
        let mut cycle: Option<Vec<String>> = None;
        self.visit_deps(start, contract, &mut visiting, &mut done, &mut cycle);
        if let Some(cycle) = cycle {
            self.add(
                "R1",
                Severity::Error,
                format!("depends_on cycle: {}", cycle.join(" -> ")),
            );
        }
    }

    fn deps_of(&self, cid: &str, start: &str, start_contract: &Value) -> Vec<String> {
        let raw = if cid == start {
            start_contract.get("depends_on").cloned()
        } else {
            let Some(path) = self.resolve_id(cid) else {
                return Vec::new();
            };
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_yaml::from_slice::<Value>(&bytes).ok())
                .and_then(|value| value.get("depends_on").cloned())
        };
        match raw {
            Some(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn visit_deps(
        &self,
        cid: &str,
        start_contract: &Value,
        visiting: &mut Vec<String>,
        done: &mut BTreeSet<String>,
        cycle: &mut Option<Vec<String>>,
    ) {
        let start = start_contract
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if done.contains(cid) || cycle.is_some() {
            return;
        }
        if let Some(pos) = visiting.iter().position(|v| v == cid) {
            let mut trail = visiting[pos..].to_vec();
            trail.push(cid.to_string());
            *cycle = Some(trail);
            return;
        }
        visiting.push(cid.to_string());
        for dep in self.deps_of(cid, start, start_contract) {
            self.visit_deps(&dep, start_contract, visiting, done, cycle);
            if cycle.is_some() {
                return;
            }
        }
        visiting.pop();
        done.insert(cid.to_string());
    }

    // ---- R2: satisfiability ------------------------------------------------

    fn rule2_satisfiability(&mut self, contract: &Value) {
        let Some(allowed) = contract.get("allowed_changes").and_then(Value::as_object) else {
            self.add(
                "R2",
                Severity::Error,
                "allowed_changes must be a mapping with paths",
            );
            return;
        };
        let paths: Vec<String> = match allowed.get("paths") {
            Some(Value::Array(items)) if !items.is_empty() => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => {
                self.add(
                    "R2",
                    Severity::Error,
                    "allowed_changes.paths must be a non-empty list",
                );
                return;
            }
        };

        let (max_lines, caps) = self.parse_line_caps();
        let repo_files = self.repo_files().to_vec();
        let mut matched: BTreeSet<String> = BTreeSet::new();
        let mut new_file_room = false;
        for glob in &paths {
            let hits: Vec<&String> = repo_files.iter().filter(|f| glob_match(f, glob)).collect();
            if hits.is_empty() {
                new_file_room = true; // room to create a new file at/under this glob
            }
            matched.extend(hits.into_iter().cloned());
        }

        let rs_files: Vec<String> = matched
            .iter()
            .filter(|f| f.ends_with(".rs"))
            .cloned()
            .collect();
        let mut at_cap: Vec<(String, usize, usize)> = Vec::new();
        let mut has_room = false;
        for f in &rs_files {
            let cap = caps.get(f).copied().unwrap_or(max_lines);
            let Ok(text) = std::fs::read(self.repo_root.join(f)) else {
                continue;
            };
            let lines = text.iter().filter(|b| **b == b'\n').count();
            if lines >= cap {
                at_cap.push((f.clone(), cap, lines));
            } else {
                has_room = true;
            }
        }
        if !rs_files.is_empty() && !has_room && !new_file_room {
            let detail = at_cap
                .iter()
                .map(|(f, c, n)| format!("{f} at {n} lines (cap {c})"))
                .collect::<Vec<_>>()
                .join("; ");
            self.add(
                "R2",
                Severity::Error,
                format!(
                    "unsatisfiable: every existing allowed .rs file is at/over its line-count cap and no allowed glob leaves new-file room: {detail}"
                ),
            );
        } else {
            for (f, cap, lines) in &at_cap {
                self.add(
                    "R2",
                    Severity::Warning,
                    format!(
                        "allowed file {f} is at/over its line-count cap ({lines} lines, cap {cap}); changes there cannot add lines"
                    ),
                );
            }
        }

        // Trivial total-overlap: every allow glob also matches a forbidden glob.
        let forbidden: Vec<String> = match allowed.get("forbidden_paths") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        if !paths.is_empty() && !forbidden.is_empty() {
            let emptied = paths
                .iter()
                .all(|g| forbidden.iter().any(|f| g == f || glob_match(g, f)));
            if emptied {
                self.add(
                    "R2",
                    Severity::Error,
                    "allowed_changes.paths is emptied by forbidden_paths: every allow glob also matches a forbidden glob",
                );
            }
        }
    }

    fn parse_line_caps(&self) -> (usize, std::collections::BTreeMap<String, usize>) {
        let mut max_lines = 3000usize;
        let mut caps = std::collections::BTreeMap::new();
        let script = self.repo_root.join("scripts/check-rust-line-count.sh");
        let Ok(text) = std::fs::read_to_string(&script) else {
            return (max_lines, caps);
        };
        let max_re = regex::Regex::new(r"(?m)^\s*max_lines=(\d+)\s*$").expect("valid regex");
        if let Some(caps_match) = max_re.captures(&text) {
            if let Ok(value) = caps_match[1].parse() {
                max_lines = value;
            }
        }
        let arm_re = regex::Regex::new(r"(?m)^\s*(crates/\S+\.rs)\)\s+echo\s+(\d+)\s*;;")
            .expect("valid regex");
        for arm in arm_re.captures_iter(&text) {
            if let Ok(value) = arm[2].parse() {
                caps.insert(arm[1].to_string(), value);
            }
        }
        (max_lines, caps)
    }

    fn repo_files(&mut self) -> &Vec<String> {
        if self.repo_files.is_none() {
            self.repo_files = Some(enumerate_repo_files(self.repo_root));
        }
        self.repo_files.as_ref().expect("populated")
    }

    // ---- R3: primitive existence + visibility -------------------------------

    fn rule3_primitives(&mut self, contract: &Value) {
        let allow_globs = allowed_globs(contract);
        match contract.get("primitives") {
            Some(Value::Array(primitives)) if !primitives.is_empty() => {
                for entry in primitives {
                    self.rule3_primitive_entry(entry, &allow_globs);
                }
            }
            _ => self.rule3_best_effort(contract),
        }
    }

    fn rule3_primitive_entry(&mut self, entry: &Value, allow_globs: &[String]) {
        let Some(map) = entry.as_object() else {
            self.add(
                "R3",
                Severity::Error,
                format!("malformed primitives entry: {entry}"),
            );
            return;
        };
        let Some(name) = map.get("name").and_then(Value::as_str) else {
            self.add(
                "R3",
                Severity::Error,
                format!("malformed primitives entry: {entry}"),
            );
            return;
        };
        let crate_name = map.get("crate").and_then(Value::as_str).unwrap_or_default();
        let crate_src = self.repo_root.join("crates").join(crate_name).join("src");
        if !crate_src.is_dir() {
            self.add(
                "R3",
                Severity::Error,
                format!("primitive {name}: crate {crate_name:?} has no src/ under crates/"),
            );
            return;
        }
        match find_definition(name, &[crate_src]) {
            None => self.add(
                "R3",
                Severity::Error,
                format!("primitive {name}: no definition found in crates/{crate_name}/src/"),
            ),
            Some(vis) if vis != "pub" => {
                let prefix = format!("crates/{crate_name}/");
                let inside = allow_globs
                    .iter()
                    .any(|g| g.starts_with(&prefix) || glob_match(&format!("{prefix}src/x.rs"), g));
                if !inside {
                    self.add(
                        "R3",
                        Severity::Error,
                        format!(
                            "primitive {name} in crates/{crate_name} is {vis}, but every allowed_changes path lies outside the owning crate — consumers cannot see it (see {EXCLUSION_DOC})"
                        ),
                    );
                }
            }
            Some(_) => {}
        }
    }

    fn rule3_best_effort(&mut self, contract: &Value) {
        let Some(interface) = contract.get("interface").and_then(Value::as_str) else {
            return;
        };
        let ident_re = regex::Regex::new(r"`([a-z][a-z0-9_]*)`").expect("valid regex");
        let ids: BTreeSet<String> = ident_re
            .captures_iter(interface)
            .map(|c| c[1].to_string())
            .filter(|i| i.contains('_'))
            .collect();
        if ids.is_empty() {
            return;
        }
        let roots = crate_src_roots(self.repo_root);
        for name in ids {
            if find_definition(&name, &roots).is_none() {
                self.add(
                    "R3",
                    Severity::Warning,
                    format!(
                        "interface names `{name}` but no definition was found in any workspace crate (best-effort scan)"
                    ),
                );
            }
        }
    }

    // ---- R4: acceptance non-vacuity ------------------------------------------

    fn rule4_acceptance(&mut self, fix: &[String], guard: &[String], contract: &Value) {
        let allow_globs = allowed_globs(contract);
        for cmd in fix.iter().chain(guard) {
            self.rule4_command(cmd, &allow_globs);
        }
    }

    fn rule4_command(&mut self, cmd: &str, allow_globs: &[String]) {
        // Whitespace split, not shlex like ccx-lint.py: quoting is unreachable here
        // because rule 6's metacharacter rejection forbids quotes outright. If the
        // grammar is ever relaxed to admit quoted arguments, this must become a
        // shell-aware tokenizer or the divergence reactivates.
        let toks: Vec<&str> = cmd.split_whitespace().collect();
        if toks.len() < 2 || toks[0] != "cargo" {
            return;
        }
        if toks[1] != "test" {
            if forge_store::acceptance_command_is_safe(cmd)
                || matches!(
                    forge_store::check_acceptance_command(cmd),
                    forge_store::AcceptanceCommandCheck::ShellMetacharacter
                )
            {
                self.add(
                    "R4",
                    Severity::Note,
                    format!("non-test command exempt from non-vacuity: {cmd}"),
                );
            }
            return;
        }
        let (crate_name, test_target, filters) = parse_cargo_test(&toks);

        if let Some(test_target) = test_target {
            let crates: Vec<String> = match &crate_name {
                Some(c) => vec![c.clone()],
                None => crate_dir_names(self.repo_root),
            };
            let rels: Vec<String> = crates
                .iter()
                .map(|c| format!("crates/{c}/tests/{test_target}.rs"))
                .collect();
            let existing: Vec<String> = rels
                .iter()
                .filter(|r| self.repo_root.join(r).is_file())
                .cloned()
                .collect();
            if existing.is_empty() {
                let deliverable = rels.iter().any(|r| inside_allowed(r, allow_globs));
                if deliverable {
                    self.add(
                        "R4",
                        Severity::Warning,
                        format!("acceptance target is a deliverable — verify post-run: {cmd} (test file does not exist yet)"),
                    );
                } else {
                    self.add(
                        "R4",
                        Severity::Error,
                        format!("vacuous acceptance: {cmd} names --test {test_target} but no such test file exists and it is outside allowed_changes.paths"),
                    );
                }
                return;
            }
            if filters.is_empty() {
                return;
            }
            let mut candidates = Vec::new();
            for r in &existing {
                let c = Path::new(r)
                    .components()
                    .nth(1)
                    .and_then(|comp| comp.as_os_str().to_str())
                    .unwrap_or_default();
                candidates.extend(self.crate_candidates(c, Some(&test_target)));
            }
            self.check_filters(cmd, &filters, &candidates);
        } else if !filters.is_empty() {
            let Some(crate_name) = crate_name else {
                return; // workspace-wide filter: conservative skip
            };
            let candidates = self.crate_candidates(&crate_name, None);
            self.check_filters(cmd, &filters, &candidates);
        }
        // filterless `cargo test` with no target is exempt.
    }

    fn check_filters(&mut self, cmd: &str, filters: &[String], candidates: &[String]) {
        for filt in filters {
            if candidates.iter().any(|cand| cand.contains(filt)) {
                continue;
            }
            self.add(
                "R4",
                Severity::Error,
                format!(
                    "vacuous acceptance: filter {filt:?} in {cmd:?} matches no candidate test path in the crate's code"
                ),
            );
        }
    }

    fn crate_candidates(&self, crate_name: &str, test_file: Option<&str>) -> Vec<String> {
        let crate_dir = self.repo_root.join("crates").join(crate_name);
        let mut files: Vec<PathBuf> = Vec::new();
        if let Some(test_file) = test_file {
            files.push(crate_dir.join("tests").join(format!("{test_file}.rs")));
        } else {
            for sub in ["src", "tests"] {
                let dir = crate_dir.join(sub);
                if dir.is_dir() {
                    collect_rs_files(&dir, &mut files);
                }
            }
            files.sort();
        }
        let mut candidates = Vec::new();
        for f in files {
            if !f.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let rel = f.strip_prefix(&crate_dir).unwrap_or(&f);
            candidates.extend(scan_candidates(&text, &module_base(rel)));
        }
        candidates
    }

    // ---- R5: exclusion clause -------------------------------------------------

    fn rule5_exclusion(&mut self, contract: &Value) {
        let mut chunks: Vec<String> = Vec::new();
        for key in ["task", "interface"] {
            if let Some(v) = contract.get(key).and_then(Value::as_str) {
                chunks.push(v.to_string());
            }
        }
        if let Some(nc) = contract.get("negative_constraints") {
            if !nc.is_null() {
                if let Ok(dumped) = serde_yaml::to_string(nc) {
                    chunks.push(dumped);
                }
            }
        }
        let text = chunks.join("\n").to_lowercase();
        if !ENUMERATION_SIGNALS.iter().any(|sig| text.contains(sig)) {
            return;
        }
        if contract.get("exclusion_contract").is_some() {
            return;
        }
        if let Some(Value::Array(primitives)) = contract.get("primitives") {
            let has_walk = primitives.iter().any(|p| {
                p.get("name")
                    .and_then(Value::as_str)
                    .map(|n| n.contains("walk"))
                    .unwrap_or(false)
            });
            if has_walk {
                return;
            }
        }
        self.add(
            "R5",
            Severity::Error,
            format!(
                "contract text touches filesystem enumeration but declares no exclusion_contract: and no owning walk primitive — a second walker with weaker exclusion semantics is licensed; see {EXCLUSION_DOC}"
            ),
        );
    }

    // ---- R6: command grammar (delegates to the shared SSOT) --------------------

    fn rule6_grammar(&mut self, fix: &[String], guard: &[String]) {
        for cmd in fix.iter().chain(guard) {
            match forge_store::check_acceptance_command(cmd) {
                forge_store::AcceptanceCommandCheck::Ok => {}
                forge_store::AcceptanceCommandCheck::ShellMetacharacter => {
                    self.grammar_violation.get_or_insert_with(|| cmd.clone());
                    self.add(
                        "R6",
                        Severity::Error,
                        format!("acceptance entry contains shell metacharacters (reaches the eval sink): {cmd:?}"),
                    );
                }
                forge_store::AcceptanceCommandCheck::GrammarViolation => {
                    self.grammar_violation.get_or_insert_with(|| cmd.clone());
                    self.add(
                        "R6",
                        Severity::Error,
                        format!("acceptance entry violates command grammar ^cargo (test|clippy|fmt|build|run): {cmd:?}"),
                    );
                }
            }
        }
    }

    // ---- driver helpers ----------------------------------------------------

    fn normalize_acceptance(&mut self, contract: &Value) -> (Vec<String>, Vec<String>) {
        match contract.get("acceptance") {
            Some(Value::Object(acc)) => {
                let fix = string_list(acc.get("fix"));
                let guard = string_list(acc.get("guard"));
                (fix, guard)
            }
            Some(Value::Array(items)) => {
                // A flat list is a legacy v0 shape; the native surface is v1-only,
                // but treat it as a fix set so R6 still gates the eval sink.
                self.add(
                    "R1",
                    Severity::Error,
                    "acceptance must be a mapping with fix/guard for ccx.contract.v1",
                );
                let fix = items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                (fix, Vec::new())
            }
            Some(Value::Null) | None => (Vec::new(), Vec::new()),
            Some(_) => {
                self.add("R1", Severity::Error, "acceptance must be a mapping");
                (Vec::new(), Vec::new())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers (parity with ccx-lint.py module functions)
// ---------------------------------------------------------------------------

fn allowed_globs(contract: &Value) -> Vec<String> {
    contract
        .get("allowed_changes")
        .and_then(Value::as_object)
        .map(|allowed| string_list(allowed.get("paths")))
        .unwrap_or_default()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// fnmatch-style glob match. `**` is normalized to `*` (author-friendly,
/// same as ccx-lint.py's `matches`), then `*`→`.*`, `?`→`.`, full-string anchor.
/// `pub(crate)` so the U6 blast postflight (`contract_blast.rs`) reuses the SINGLE
/// ported matcher rather than duplicating it (plan U3 single-source rule).
pub(crate) fn glob_match(path: &str, pattern: &str) -> bool {
    let normalized = pattern.replace("**", "*");
    let mut regex = String::from("^");
    for ch in normalized.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex)
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

fn enumerate_repo_files(repo_root: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut stack = vec![repo_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(repo_root) {
                    files.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    files
}

fn crate_src_roots(repo_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_root.join("crates")) {
        for entry in entries.flatten() {
            let src = entry.path().join("src");
            if src.is_dir() {
                roots.push(src);
            }
        }
    }
    roots.sort();
    roots
}

fn crate_dir_names(repo_root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(repo_root.join("crates")) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Find a definition of `name` in the given roots and return its visibility
/// qualifier (`pub`, `pub(...)`, or `private`), or `None` if undefined. Text
/// search, at parity with ccx-lint.py's `_find_definition`.
fn find_definition(name: &str, roots: &[PathBuf]) -> Option<String> {
    let pattern = format!(
        r"^\s*(pub(?:\s*\([^)]*\))?)?\s*(?:unsafe\s+)?(?:async\s+)?(?:fn|struct|enum|trait|mod|const)\s+{}\b",
        regex::escape(name)
    );
    let re = regex::Regex::new(&pattern).ok()?;
    let mut files = Vec::new();
    for root in roots {
        if root.is_dir() {
            collect_rs_files(root, &mut files);
        }
    }
    files.sort();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines() {
            if let Some(caps) = re.captures(line) {
                let qual = caps
                    .get(1)
                    .map(|m| m.as_str().replace(' ', ""))
                    .unwrap_or_default();
                let vis = if qual == "pub" {
                    "pub".to_string()
                } else if qual.starts_with("pub(") {
                    qual
                } else {
                    "private".to_string()
                };
                return Some(vis);
            }
        }
    }
    None
}

/// Parse `(crate, test_target, filters)` from a tokenized `cargo test ...`
/// command (parity with ccx-lint.py's inline parser).
fn parse_cargo_test(toks: &[&str]) -> (Option<String>, Option<String>, Vec<String>) {
    let mut crate_name = None;
    let mut test_target = None;
    let mut filters = Vec::new();
    let mut i = 2;
    while i < toks.len() {
        let t = toks[i];
        if t == "--" {
            break;
        }
        if t == "-p" || t == "--package" {
            crate_name = toks.get(i + 1).map(|s| s.to_string());
            i += 2;
        } else if t == "--test" {
            test_target = toks.get(i + 1).map(|s| s.to_string());
            i += 2;
        } else if CARGO_VALUE_FLAGS.contains(&t) {
            i += 2;
        } else if t.starts_with('-') {
            i += 1;
        } else {
            filters.push(t.to_string());
            i += 1;
        }
    }
    (crate_name, test_target, filters)
}

/// `src/foo.rs`→`[foo]`, `src/foo/mod.rs`→`[foo]`, `src/lib.rs`/`tests/x.rs`→`[]`
/// (parity with ccx-lint.py's `_module_base`). `rel` is relative to the crate dir.
fn module_base(rel: &Path) -> Vec<String> {
    let mut parts: Vec<String> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    if parts.first().map(String::as_str) == Some("tests") {
        return Vec::new();
    }
    if !parts.is_empty() {
        parts.remove(0); // drop src/
    }
    match parts.last().map(String::as_str) {
        Some("lib.rs") | Some("main.rs") | Some("mod.rs") => {
            parts.pop();
        }
        Some(last) => {
            let stripped = last.strip_suffix(".rs").unwrap_or(last).to_string();
            *parts.last_mut().unwrap() = stripped;
        }
        None => {}
    }
    parts
}

/// Collect `::`-joined function paths in a Rust source, mod-nesting aware
/// (parity with ccx-lint.py's `_scan_candidates`).
fn scan_candidates(text: &str, base: &[String]) -> Vec<String> {
    let mod_re = regex::Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*\{")
        .expect("valid regex");
    let fn_re = regex::Regex::new(
        r#"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)"#,
    )
    .expect("valid regex");
    let mut depth: i64 = 0;
    let mut stack: Vec<(String, i64)> = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(caps) = fn_re.captures(line) {
            let mut path: Vec<String> = base.to_vec();
            path.extend(stack.iter().map(|(m, _)| m.clone()));
            path.push(caps[1].to_string());
            out.push(path.join("::"));
        }
        if let Some(caps) = mod_re.captures(line) {
            stack.push((caps[1].to_string(), depth));
        }
        depth += line.matches('{').count() as i64 - line.matches('}').count() as i64;
        while let Some((_, opened)) = stack.last() {
            if depth <= *opened {
                stack.pop();
            } else {
                break;
            }
        }
    }
    out
}

fn inside_allowed(rel_path: &str, allow_globs: &[String]) -> bool {
    allow_globs
        .iter()
        .any(|g| rel_path == g || glob_match(rel_path, g))
}

// ===========================================================================
// U5: `forge contract run` and `forge contract integrate`
// ===========================================================================
//
// The runner ports `tools/ccx/run-task.sh` onto native primitives (KTD7): a
// per-run scratch workspace materialized from the base tree (never the user
// worktree, so `DIRTY_WORKTREE` cannot apply to it), the acknowledged dependency
// stack applied on top per-id (R20), a fresh opaque agent subprocess per task, and
// halt-on-`UNKNOWN.md` (R8). The agent's patch is a native tree diff of the
// post-run workspace against the post-dependency-application baseline (KTD7's
// misattribution guard), never against the raw base.
//
// The agent command is taken ONLY from the explicit `--agent-cmd` flag — there is
// no repo-config fallback in v1 (a repo-shipped command source is a supply-chain
// surface, Scope Boundaries). It is executed via `sh -c "<cmd>"`: the flag is
// explicit operator input (not repo-derived), and `sh -c` preserves the harness's
// `AGENT_CMD` shell semantics (run-task.sh L105, `bash -c "$AGENT_CMD"`) so a
// ported command string behaves identically. No retries, auth, or supervision.

/// The agent-facing stop file, at the scratch workspace root (R8/R26).
const UNKNOWN_FILE: &str = "UNKNOWN.md";

/// One task in a resolved, topologically-ordered run plan (KTD-run ordering).
#[derive(Debug, Clone)]
struct PlannedTask {
    contract_id: String,
    revision: i64,
    depends_on: Vec<String>,
}

impl PlannedTask {
    /// The per-run task id: the contract id itself, unique within a chain (so the
    /// `contract_run_tasks` UNIQUE(run_id, task_id) holds) and stable for KTD9 resume.
    fn task_id(&self) -> String {
        self.contract_id.clone()
    }
}

/// Parse a frozen revision's `depends_on:` id list, mirroring the neighbor parser.
fn parse_depends_on(source_yaml: &str) -> Result<Vec<String>> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(source_yaml)
        .map_err(|err| anyhow!("frozen contract is not valid YAML: {err}"))?;
    let Some(map) = parsed.as_mapping() else {
        return Ok(Vec::new());
    };
    match map.get(serde_yaml::Value::from("depends_on")) {
        None | Some(serde_yaml::Value::Null) => Ok(Vec::new()),
        Some(serde_yaml::Value::Sequence(items)) => {
            let mut ids = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(id) => ids.push(id.to_string()),
                    None => return Err(anyhow!("depends_on: must be a list of ids")),
                }
            }
            Ok(ids)
        }
        Some(_) => Err(anyhow!("depends_on: must be a list of ids")),
    }
}

/// Read a frozen revision's `allowed_changes.paths` / `.forbidden_paths` globs for
/// the U6 blast postflight. Parses the EXACT stored source bytes (R1) into the same
/// serde_json shape the linter uses, so blast and lint read one allowlist.
fn contract_allowed_changes(
    cwd: &Path,
    contract_id: &str,
    revision: i64,
) -> Result<(Vec<String>, Vec<String>)> {
    let record = forge_store::contract_revision(cwd, contract_id, revision)?
        .ok_or_else(|| anyhow!("frozen revision {contract_id}@{revision} not found for blast"))?;
    let value: Value = serde_yaml::from_str(&record.source_yaml)
        .map_err(|err| anyhow!("frozen contract is not valid YAML: {err}"))?;
    let allow = allowed_globs(&value);
    let forbid = value
        .get("allowed_changes")
        .and_then(Value::as_object)
        .map(|allowed| string_list(allowed.get("forbidden_paths")))
        .unwrap_or_default();
    Ok((allow, forbid))
}

/// Resolve the requested contract ids to frozen revisions, enforce per-id
/// out-of-chain dependency acknowledgement (R20 — no silent count guard), and
/// return the Kahn topological order plus the `dep_id -> run/task ref` ack map.
fn resolve_run_plan(
    cwd: &Path,
    args: &ContractRunArgs,
) -> Result<(Vec<PlannedTask>, BTreeMap<String, String>)> {
    let mut ack: BTreeMap<String, String> = BTreeMap::new();
    for spec in &args.dep {
        let (id, reference) = spec
            .split_once('=')
            .ok_or_else(|| anyhow!("--dep must be <id>=<run-or-task-id>: {spec:?}"))?;
        ack.insert(id.to_string(), reference.to_string());
    }

    let mut info: BTreeMap<String, (i64, Vec<String>)> = BTreeMap::new();
    let mut order_in: Vec<String> = Vec::new();
    for id in &args.contract_ids {
        let revision = forge_store::latest_contract_revision(cwd, id)?
            .filter(|record| record.state == "frozen" && record.lint_clean)
            .ok_or_else(|| ForgeError::ContractNotFrozen {
                contract_id: id.clone(),
                revision: 0,
            })?;
        let deps = parse_depends_on(&revision.source_yaml)?;
        if info.insert(id.clone(), (revision.revision, deps)).is_some() {
            return Err(anyhow!("duplicate contract id in chain: {id}"));
        }
        order_in.push(id.clone());
    }
    if order_in.len() > 1 && !args.chain {
        return Err(anyhow!("multiple contracts require --chain"));
    }

    let in_set: BTreeSet<String> = info.keys().cloned().collect();

    // Every out-of-chain dependency must be acknowledged by its own --dep (R20).
    let mut missing_ack: Vec<String> = Vec::new();
    for (_rev, deps) in info.values() {
        for dep in deps {
            if !in_set.contains(dep) && !ack.contains_key(dep) && !missing_ack.contains(dep) {
                missing_ack.push(dep.clone());
            }
        }
    }
    if !missing_ack.is_empty() {
        missing_ack.sort();
        return Err(anyhow!(
            "chain refusal: out-of-chain dependencies not acknowledged by --dep: {}. Supply one --dep <id>=<run-or-task-id> per missing dependency",
            missing_ack.join(", ")
        ));
    }

    // An --dep that names something that is not an out-of-chain dependency is a
    // mismatch (R20: name exactly which dependency each patch satisfies).
    let all_deps: BTreeSet<String> = info
        .values()
        .flat_map(|(_rev, deps)| deps.iter().cloned())
        .collect();
    let mut stray: Vec<String> = ack
        .keys()
        .filter(|id| !all_deps.contains(*id) || in_set.contains(*id))
        .cloned()
        .collect();
    if !stray.is_empty() {
        stray.sort();
        return Err(anyhow!(
            "chain refusal: --dep names id(s) that are not out-of-chain dependencies: {}",
            stray.join(", ")
        ));
    }

    // Kahn topological order, stable on the given contract order.
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut topo: Vec<String> = Vec::new();
    let mut pending: Vec<String> = order_in;
    while !pending.is_empty() {
        let mut rest = Vec::new();
        for id in &pending {
            let ready = info[id]
                .1
                .iter()
                .filter(|dep| in_set.contains(*dep))
                .all(|dep| placed.contains(dep));
            if ready {
                placed.insert(id.clone());
                topo.push(id.clone());
            } else {
                rest.push(id.clone());
            }
        }
        if rest.len() == pending.len() {
            return Err(anyhow!(
                "depends_on cycle among chain contracts: {}",
                rest.join(", ")
            ));
        }
        pending = rest;
    }

    let plan = topo
        .into_iter()
        .map(|id| {
            let (revision, depends_on) = info[&id].clone();
            PlannedTask {
                contract_id: id,
                revision,
                depends_on,
            }
        })
        .collect();
    Ok((plan, ack))
}

/// Append the verbatim task-instruction stop wording to the store-emitted brief
/// (R6). `ccx-brief.py` does not emit it; `run-task.sh` `cat`s it on at run time —
/// so appending here reproduces the harness prompt exactly.
fn assemble_prompt(brief: &str) -> String {
    format!("{brief}{CONTRACT_TASK_INSTRUCTION}")
}

/// The captured, redacted outcome of one opaque agent subprocess (R7/R16).
struct AgentOutcome {
    exit_code: i32,
    /// Redacted, EXCERPT_LIMIT-capped agent stdout — `None` when the stream was
    /// empty. Redacted BEFORE it is stored, hashed, and signed (KTD3).
    stdout_excerpt: Option<String>,
    stderr_excerpt: Option<String>,
}

/// Execute the opaque agent command once via `sh -c` in `workspace`, feeding the
/// prompt on stdin (R7). Stdout/stderr are captured to files (so a chatty agent
/// cannot deadlock against the stdin pipe), then redacted through the shared
/// evidence pass and capped at `EXCERPT_LIMIT` before they are returned for storage
/// on the per-task run row (R16 defense-in-depth). No retries or supervision.
fn run_agent(agent_cmd: &str, workspace: &Path, prompt: &str) -> Result<AgentOutcome> {
    let capture = tempfile::tempdir().context("create agent capture dir")?;
    let stdout_path = capture.path().join("stdout");
    let stderr_path = capture.path().join("stderr");
    let stdout_file = std::fs::File::create(&stdout_path).context("create agent stdout capture")?;
    let stderr_file = std::fs::File::create(&stderr_path).context("create agent stderr capture")?;
    let mut child = ProcessCommand::new("sh")
        .arg("-c")
        .arg(agent_cmd)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("cannot spawn agent command via sh -c: {agent_cmd:?}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // Best-effort: a command that ignores stdin may close the pipe early.
        let _ = stdin.write_all(prompt.as_bytes());
        // `stdin` drops here, closing the pipe so a reader sees EOF before wait().
    }
    let status = child.wait().context("agent subprocess wait failed")?;
    Ok(AgentOutcome {
        exit_code: status.code().unwrap_or(-1),
        stdout_excerpt: redacted_excerpt(&stdout_path)?,
        stderr_excerpt: redacted_excerpt(&stderr_path)?,
    })
}

/// Read a capture file, redact it through the shared `redact_evidence_excerpt`
/// pass, and cap it at `EXCERPT_LIMIT` bytes — the same redact-then-truncate
/// ordering the evidence pipeline uses so a secret straddling the cap is removed
/// before its prefix is persisted. Returns `None` for an empty stream.
fn redacted_excerpt(path: &Path) -> Result<Option<String>> {
    // Read a bounded window (4x the cap) so a secret near the boundary is redacted
    // before truncation, without loading an unbounded stream into memory.
    let window = forge_evidence::EXCERPT_LIMIT * 4;
    let mut file = std::fs::File::open(path).context("open agent capture")?;
    let mut bytes = vec![0u8; window + 1];
    let read = std::io::Read::read(&mut file, &mut bytes)?;
    bytes.truncate(read.min(window));
    if bytes.is_empty() {
        return Ok(None);
    }
    let (redacted, _kinds) =
        forge_content::redact_evidence_excerpt(&String::from_utf8_lossy(&bytes));
    let capped = truncate_to_char_boundary(&redacted, forge_evidence::EXCERPT_LIMIT);
    Ok((!capped.is_empty()).then(|| capped.to_string()))
}

/// Truncate `text` to at most `limit` bytes on a UTF-8 char boundary (never split
/// a multi-byte scalar), mirroring the evidence excerpt cap.
fn truncate_to_char_boundary(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The four required stop fields (R8). A best-effort labelled parse: `What:`,
/// `Why:`, `Kind:`, `Evidence:` (case-insensitive, split on the first colon). A
/// stop is `malformed` when any field is missing or `kind` is out of the
/// blocking/assumption/observation vocabulary — but it still opens (fail-closed).
/// A wholly unlabelled file keeps its raw text as `what_needed` for triage context.
#[allow(clippy::type_complexity)]
fn parse_unknown_fields(
    text: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
) {
    let (mut what, mut why, mut kind, mut evidence) = (None, None, None, None);
    for line in text.lines() {
        let Some((label, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest.trim().to_string();
        if value.is_empty() {
            continue;
        }
        match label.trim().to_lowercase().as_str() {
            "what" | "what is needed" | "what needed" | "need" => what = Some(value),
            "why" => why = Some(value),
            "kind" => kind = Some(value),
            "evidence" | "file" => evidence = Some(value),
            _ => {}
        }
    }
    let kind_ok = kind
        .as_deref()
        .map(|k| matches!(k, "blocking" | "assumption" | "observation"))
        .unwrap_or(false);
    let malformed =
        what.is_none() || why.is_none() || kind.is_none() || evidence.is_none() || !kind_ok;
    if what.is_none() && why.is_none() && kind.is_none() && evidence.is_none() {
        // Wholly unlabelled: preserve the raw text so a triager can reconstruct.
        let raw = text.trim();
        if !raw.is_empty() {
            what = Some(raw.to_string());
        }
    }
    (what, why, kind, evidence, malformed)
}

/// One per-task row accumulated during a run, including the redacted agent
/// stdout/stderr excerpts (R7/R16). Agentless rows (a resumed replay or a skipped
/// dependent) carry `None` for the agent fields.
struct TaskState {
    task_id: String,
    outcome: String,
    patch_content_ref: Option<String>,
    agent_exit_code: Option<i64>,
    agent_stdout_excerpt: Option<String>,
    agent_stderr_excerpt: Option<String>,
}

impl TaskState {
    /// A task recorded without running an agent this run (resumed replay or a
    /// skipped dependent): no exit code, no captured output.
    fn agentless(task_id: String, outcome: &str, patch: Option<String>) -> Self {
        Self {
            task_id,
            outcome: outcome.to_string(),
            patch_content_ref: patch,
            agent_exit_code: None,
            agent_stdout_excerpt: None,
            agent_stderr_excerpt: None,
        }
    }

    /// A task whose agent ran: capture its exit code and redacted stdout/stderr
    /// excerpts on the row (R7/R16).
    fn from_agent(
        task_id: String,
        outcome: &str,
        patch: Option<String>,
        agent: &AgentOutcome,
    ) -> Self {
        Self {
            task_id,
            outcome: outcome.to_string(),
            patch_content_ref: patch,
            agent_exit_code: Some(i64::from(agent.exit_code)),
            agent_stdout_excerpt: agent.stdout_excerpt.clone(),
            agent_stderr_excerpt: agent.stderr_excerpt.clone(),
        }
    }
}

/// Build the run-record input, folding the per-task completion states plus the
/// integrate-time reconstruction context (baseline ref, base commit, ack map) into
/// `dependency_stack_json` (R7/KTD8/KTD9).
#[allow(clippy::too_many_arguments)]
fn build_run_input(
    target: &PlannedTask,
    base_commit: &str,
    baseline_ref: &Option<String>,
    ack: &BTreeMap<String, String>,
    outcome: &str,
    exit_code: i64,
    agent_exit: Option<i64>,
    patch_ref: Option<String>,
    task_states: &[TaskState],
) -> RecordContractRunInput {
    let dependency_stack_json = json!({
        "baseline_ref": baseline_ref,
        "base_commit": base_commit,
        "acknowledged": ack,
    })
    .to_string();
    RecordContractRunInput {
        contract_id: target.contract_id.clone(),
        revision: target.revision,
        base_head: Some(base_commit.to_string()),
        dependency_stack_json: Some(dependency_stack_json),
        outcome: outcome.to_string(),
        exit_code,
        agent_exit_code: agent_exit,
        patch_content_ref: patch_ref,
        tasks: task_states
            .iter()
            .enumerate()
            .map(|(index, state)| ContractRunTaskInput {
                task_id: state.task_id.clone(),
                task_index: index as i64,
                outcome: state.outcome.clone(),
                patch_content_ref: state.patch_content_ref.clone(),
                agent_exit_code: state.agent_exit_code,
                agent_stdout_excerpt: state.agent_stdout_excerpt.clone(),
                agent_stderr_excerpt: state.agent_stderr_excerpt.clone(),
            })
            .collect(),
    }
}

/// Append `skipped` task rows for every plan task after `stopped_index` (dependents
/// do not execute past a halt or failure, R8/R11).
fn fill_skipped(task_states: &mut Vec<TaskState>, plan: &[PlannedTask], stopped_index: usize) {
    for task in &plan[stopped_index + 1..] {
        task_states.push(TaskState::agentless(task.task_id(), "skipped", None));
    }
}

/// `forge contract run` — materialize a scratch base, run the agent per task in
/// dependency order, and record the run fail-closed. Envelope status is always
/// SUCCESS (the run recorded its outcome); the outcome discriminator and the
/// harness exit code (0/1/2/3) travel in `data` (R14/R25), mapped to the process
/// exit by `main`. Preflight refusals (open stop, stale UNKNOWN, not-frozen,
/// unacknowledged dep) are typed errors — no agent session started.
///
/// Lock decision (DELIBERATE): `"contract run"` is a mutating command
/// (`is_mutating_command` / `locks_repo_for_command` in `commands/core.rs`), so it
/// holds the repo advisory lock for the WHOLE command — INCLUDING the agent
/// subprocess execution below. This is unlike plain `forge run`, which has an
/// explicit lock carve-out around its child process. The plan (2026-07-10 CCX
/// native contracts, Scope Boundaries) assumes single-run-at-a-time per repo and
/// defers concurrent chain runs; holding the lock across the agent enforces that
/// assumption (one chain mutating the ledger at a time) rather than interleaving
/// two chains' run/stop/verdict writes. Do not add a carve-out without lifting that
/// single-run assumption.
fn run_response(request_id: Option<String>, args: ContractRunArgs) -> ResponseEnvelope {
    command_result("contract run", request_id, move |cwd, request_id| {
        let repo_root = forge_store::repository_root_path(&cwd)?;
        let (plan, ack) = resolve_run_plan(&cwd, &args)?;

        // Leg 3 (R10/AE2/AE9): refuse if ANY contract in the FULL transitive
        // dependency closure has an open stop, naming the blocking stop ids — no
        // agent runs. The seeds are the chain's contracts plus every acknowledged
        // out-of-chain dependency; from there we expand each contract's frozen
        // `depends_on` to fixpoint, so an open stop on a DEEP dependency (reached only
        // through an acknowledged dep, e.g. an ack'd `b` whose frozen `depends_on`
        // names a stopped `a`) still blocks the run. Walking only the direct seeds
        // (the prior behavior) let such a transitive stop slip the gate. Cycle-safe
        // via the visited set; a contract with no frozen revision contributes no
        // edges (and can carry no open stop of its own either).
        let mut closure_ids: Vec<String> = Vec::new();
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut frontier: VecDeque<String> = plan
            .iter()
            .map(|task| task.contract_id.clone())
            .chain(ack.keys().cloned())
            .collect();
        while let Some(id) = frontier.pop_front() {
            if !visited.insert(id.clone()) {
                continue;
            }
            closure_ids.push(id.clone());
            if let Some(revision) = forge_store::latest_contract_revision(&cwd, &id)?
                .filter(|record| record.state == "frozen")
            {
                for dep in parse_depends_on(&revision.source_yaml)? {
                    if !visited.contains(&dep) {
                        frontier.push_back(dep);
                    }
                }
            }
        }
        let open = forge_store::open_stops_for_contracts(&cwd, &closure_ids)?;
        if !open.is_empty() {
            let stop_ids = open.into_iter().map(|stop| stop.stop_id).collect();
            return Err(ForgeError::ContractOpenStop { stop_ids }.into());
        }

        // R26/AE10: a stale UNKNOWN.md at the operator-visible workspace root refuses
        // before any agent session or stop record, so it cannot be attributed to the
        // wrong run.
        if cwd.join(UNKNOWN_FILE).exists() || repo_root.join(UNKNOWN_FILE).exists() {
            return Err(ForgeError::StaleUnknownFile.into());
        }

        let target = plan
            .last()
            .expect("resolve_run_plan yields a non-empty plan")
            .clone();
        let base_commit = current_base(&cwd)?;
        let base_tree_ref = owner_base_content_ref(&cwd, &base_commit)?;
        let store = NativeObjectStore::new(&repo_root);
        let excluded = [UNKNOWN_FILE.to_string()];

        // KTD9: a rerun of a halted chain resumes from the halted task, replaying the
        // prior run's recorded per-task completed outputs instead of re-executing
        // their agents. `--fresh` forces a full re-run. Resume refuses when the
        // recorded state no longer applies (base moved, or a recorded patch object is
        // gone) — fresh-run guidance travels in the refusal message.
        let mut resume_outputs: BTreeMap<String, String> = BTreeMap::new();
        if let (Some(resume_id), false) = (&args.resume, args.fresh) {
            let prior = forge_store::contract_run_by_ref(&cwd, resume_id)?
                .ok_or_else(|| anyhow!("no recorded run to resume for {resume_id:?}"))?;
            if prior.base_head.as_deref() != Some(base_commit.as_str()) {
                return Err(ForgeError::ContractNotIntegrable {
                    reason: format!(
                        "resume refused: run {} was recorded against base {}, but the current base is {}. Run fresh (omit --resume or pass --fresh)",
                        prior.run_id,
                        prior.base_head.as_deref().unwrap_or("<none>"),
                        base_commit
                    ),
                }
                .into());
            }
            for prior_task in &prior.tasks {
                if prior_task.outcome != "completed" {
                    continue;
                }
                let Some(patch_ref) = prior_task.patch_content_ref.as_deref() else {
                    continue;
                };
                if store.verify_content_ref(patch_ref).is_err() {
                    return Err(ForgeError::ContractNotIntegrable {
                        reason: format!(
                            "resume refused: recorded patch for completed task {} no longer applies (content object unavailable). Run fresh (omit --resume or pass --fresh)",
                            prior_task.task_id
                        ),
                    }
                    .into());
                }
                resume_outputs.insert(prior_task.task_id.clone(), patch_ref.to_string());
            }
        }

        let mut completed_outputs: BTreeMap<String, String> = BTreeMap::new();
        let mut task_states: Vec<TaskState> = Vec::new();
        let mut final_baseline: Option<String> = None;
        // U6: one `blast`-pass verdict per freshly-completed task, recorded atomically
        // with the run at the end (clean chain) or folded into a violation's verdict
        // batch (a task's blast failure halts the chain).
        let mut blast_verdicts: Vec<ContractRunVerdictInput> = Vec::new();

        for (index, task) in plan.iter().enumerate() {
            // KTD9 resume: a task the prior run completed is replayed from its
            // recorded output — no agent session, so no captured output.
            if let Some(prev) = resume_outputs.get(&task.task_id()) {
                completed_outputs.insert(task.contract_id.clone(), prev.clone());
                task_states.push(TaskState::agentless(
                    task.task_id(),
                    "completed",
                    Some(prev.clone()),
                ));
                continue;
            }
            // KTD7: materialize the base into a fresh scratch workspace, then overlay
            // each dependency's produced tree on top per-id (never the user worktree).
            let scratch = tempfile::tempdir().context("create scratch workspace")?;
            materialize_content_ref(&repo_root, scratch.path(), &base_tree_ref)?;
            for dep in &task.depends_on {
                let dep_ref = if let Some(reference) = completed_outputs.get(dep) {
                    reference.clone()
                } else {
                    let ack_ref = ack
                        .get(dep)
                        .ok_or_else(|| anyhow!("internal: dependency {dep} not acknowledged"))?;
                    forge_store::contract_run_by_ref(&cwd, ack_ref)?
                        .and_then(|run| run.patch_content_ref)
                        .ok_or_else(|| {
                            anyhow!(
                                "acknowledged dependency {dep} ({ack_ref}) has no completed patch"
                            )
                        })?
                };
                materialize_content_ref(&repo_root, scratch.path(), &dep_ref)?;
            }
            // The diff baseline is (base + dep patches), NOT the raw base (KTD7).
            let baseline =
                snapshot_worktree_into_store_excluding(&repo_root, scratch.path(), &excluded)?
                    .content_ref;

            let brief = forge_store::contract_brief(&cwd, &task.contract_id, Some(task.revision))?;
            let prompt = assemble_prompt(&brief.brief);
            let agent = run_agent(&args.agent_cmd, scratch.path(), &prompt)?;

            // Halt-on-unknown (R8): fail-closed ingest into a signed stop, halt, exit 2.
            if scratch.path().join(UNKNOWN_FILE).exists() {
                let raw =
                    std::fs::read_to_string(scratch.path().join(UNKNOWN_FILE)).unwrap_or_default();
                let (what, why, kind, evidence, malformed) = parse_unknown_fields(&raw);
                task_states.push(TaskState::from_agent(
                    task.task_id(),
                    "stopped",
                    None,
                    &agent,
                ));
                fill_skipped(&mut task_states, &plan, index);
                // F1: record the stopped run AND open its stop in ONE transaction, so
                // a stop-insert failure can never leave an `outcome = "stopped"` run
                // with no stop row to triage. The store sets the stop's `run_id` to
                // the freshly-created run id atomically.
                let (run, stop) = forge_store::record_contract_run_with_stop(
                    &cwd,
                    request_id.clone(),
                    build_run_input(
                        &target,
                        &base_commit,
                        &final_baseline,
                        &ack,
                        "stopped",
                        2,
                        Some(i64::from(agent.exit_code)),
                        None,
                        &task_states,
                    ),
                    OpenContractStopInput {
                        contract_id: task.contract_id.clone(),
                        revision: task.revision,
                        run_id: None,
                        task_id: Some(task.task_id()),
                        what_needed: what,
                        why_unanswered: why,
                        kind,
                        evidence,
                        malformed,
                    },
                )?;
                let mut data = json!({
                    "outcome": "stopped",
                    "exit_code": 2,
                    "run_id": run.run_id,
                    "stop_id": stop.stop_id,
                    "malformed": stop.malformed,
                    "contract_id": task.contract_id,
                    "revision": task.revision,
                });
                if stop.malformed {
                    // R25: a malformed ingest surfaces a distinct typed code.
                    data["code"] = json!("CONTRACT_STOP_MALFORMED");
                }
                return Ok((Some(run.run_id), data, Vec::new()));
            }

            // Crashed/unauthenticated agent (R11/AE5): nonzero exit, no UNKNOWN.md.
            if agent.exit_code != 0 {
                task_states.push(TaskState::from_agent(
                    task.task_id(),
                    "failed",
                    None,
                    &agent,
                ));
                fill_skipped(&mut task_states, &plan, index);
                let run = forge_store::record_contract_run(
                    &cwd,
                    request_id,
                    build_run_input(
                        &target,
                        &base_commit,
                        &final_baseline,
                        &ack,
                        "failed",
                        1,
                        Some(i64::from(agent.exit_code)),
                        None,
                        &task_states,
                    ),
                )?;
                let data = json!({
                    "outcome": "failed",
                    "exit_code": 1,
                    "run_id": run.run_id,
                    "reason": "agent exited nonzero without filing UNKNOWN.md",
                    "agent_exit_code": agent.exit_code,
                });
                return Ok((Some(run.run_id), data, Vec::new()));
            }

            // The agent patch is the tree diff of post-run vs the baseline (KTD7).
            let post =
                snapshot_worktree_into_store_excluding(&repo_root, scratch.path(), &excluded)?
                    .content_ref;
            let diff = diff_native_content_refs(&store, &baseline, &post, &DiffOptions::default())?;
            if diff.files.is_empty() {
                // Empty patch never passes as success (R11/AE8): failed, exit 1.
                task_states.push(TaskState::from_agent(
                    task.task_id(),
                    "failed",
                    None,
                    &agent,
                ));
                fill_skipped(&mut task_states, &plan, index);
                let run = forge_store::record_contract_run(
                    &cwd,
                    request_id,
                    build_run_input(
                        &target,
                        &base_commit,
                        &final_baseline,
                        &ack,
                        "failed",
                        1,
                        Some(i64::from(agent.exit_code)),
                        None,
                        &task_states,
                    ),
                )?;
                let data = json!({
                    "outcome": "failed",
                    "exit_code": 1,
                    "run_id": run.run_id,
                    "reason": "agent produced an empty patch (zero-delta diff)",
                });
                return Ok((Some(run.run_id), data, Vec::new()));
            }

            // U6 BLAST POSTFLIGHT (R12/R16/AE7): classify the agent patch against the
            // contract allow/forbid globs + the non-weakenable default-forbid list, and
            // scan added/modified post-state content for secrets. A violation halts the
            // chain with exit 3, records a `blast` verdict naming the path (never the
            // content), and does NOT persist the offending patch — the post tree object
            // the snapshot already wrote is left unreferenced so GC reclaims it (KTD3/R16).
            let (allow, forbid) = contract_allowed_changes(&cwd, &task.contract_id, task.revision)?;
            let blast = contract_blast::evaluate_blast(&diff, &allow, &forbid, scratch.path())?;
            if blast.has_violation() {
                task_states.push(TaskState::from_agent(
                    task.task_id(),
                    "failed",
                    None,
                    &agent,
                ));
                fill_skipped(&mut task_states, &plan, index);
                // Prior tasks' pass verdicts + this task's failing verdicts, all atomic
                // with the run row (record_contract_run_with_verdicts).
                let mut verdicts = std::mem::take(&mut blast_verdicts);
                let mut violations_json: Vec<Value> = Vec::new();
                let mut secret_content_detected = false;
                for violation in &blast.violations {
                    if violation.class == BlastViolationClass::SecretContent {
                        secret_content_detected = true;
                    }
                    verdicts.push(ContractRunVerdictInput {
                        task_id: Some(task.task_id()),
                        verdict_kind: "blast".to_string(),
                        command: None,
                        passed: false,
                        detail: Some(violation.detail.clone()),
                        evidence_id: None,
                    });
                    violations_json.push(json!({
                        "path": violation.path,
                        "class": violation.class.as_str(),
                        "detail": violation.detail,
                    }));
                }
                // patch_content_ref is None on BOTH the run and the offending task, so a
                // secret-bearing post tree is never referenced (R16 fail-closed guard).
                let (run, _recorded) = forge_store::record_contract_run_with_verdicts(
                    &cwd,
                    request_id,
                    build_run_input(
                        &target,
                        &base_commit,
                        &final_baseline,
                        &ack,
                        "blast_violation",
                        3,
                        Some(i64::from(agent.exit_code)),
                        None,
                        &task_states,
                    ),
                    verdicts,
                )?;
                let data = json!({
                    "outcome": "blast_violation",
                    "exit_code": 3,
                    "run_id": run.run_id,
                    // R25: the typed code travels in a SUCCESS envelope (the run recorded
                    // its outcome); main.rs maps data.exit_code=3 to the process exit.
                    "code": "CONTRACT_BLAST_VIOLATION",
                    "contract_id": task.contract_id,
                    "revision": task.revision,
                    "secret_content_detected": secret_content_detected,
                    "violations": violations_json,
                    "facade_allowed": blast.facade_allowed,
                });
                return Ok((Some(run.run_id), data, Vec::new()));
            }
            // Clean blast: record a per-task pass verdict, kept for the final run row.
            blast_verdicts.push(ContractRunVerdictInput {
                task_id: Some(task.task_id()),
                verdict_kind: "blast".to_string(),
                command: None,
                passed: true,
                detail: None,
                evidence_id: None,
            });

            completed_outputs.insert(task.contract_id.clone(), post.clone());
            final_baseline = Some(baseline);
            task_states.push(TaskState::from_agent(
                task.task_id(),
                "completed",
                Some(post),
                &agent,
            ));
        }

        // Resume baseline guard: if every task was replayed from a prior run's
        // recorded output and no fresh task executed, `final_baseline` was never set,
        // so the run would be recorded with `baseline_ref: null` and fail integrate
        // opaquely later (the reconstruction base is missing). Refuse now with an
        // actionable message. In practice a resume always re-runs at least the halted
        // task (only prior-completed tasks are replayed), so this is a degenerate
        // guard, not an expected path.
        if final_baseline.is_none() {
            return Err(ForgeError::ContractNotIntegrable {
                reason: format!(
                    "resume of {} replayed every task without running a fresh one, so no integration baseline was captured. Integrate the original run, or re-run with --fresh",
                    args.resume.as_deref().unwrap_or("<run>")
                ),
            }
            .into());
        }

        // Every task completed AND passed blast (exit 0). The run's patch is the target
        // task's produced tree, GC-rooted via the contract_runs patch_content_ref walk
        // (gc.rs, KTD3). The per-task `blast`-pass verdicts are recorded atomically with
        // the run so the ledger shows blast ran clean on every task (U6/R12).
        let final_post = completed_outputs.get(&target.contract_id).cloned();
        let run_input = build_run_input(
            &target,
            &base_commit,
            &final_baseline,
            &ack,
            "completed",
            0,
            Some(0),
            final_post.clone(),
            &task_states,
        );
        let run = if blast_verdicts.is_empty() {
            // Degenerate (a resume that ran no fresh task is already refused above): no
            // fresh blast verdict to record. Fall back to the plain run recorder rather
            // than tripping the non-empty-verdicts guard.
            forge_store::record_contract_run(&cwd, request_id, run_input)?
        } else {
            forge_store::record_contract_run_with_verdicts(
                &cwd,
                request_id,
                run_input,
                blast_verdicts,
            )?
            .0
        };
        let data = json!({
            "outcome": "completed",
            "exit_code": 0,
            "run_id": run.run_id,
            "contract_id": target.contract_id,
            "revision": target.revision,
            "patch_content_ref": final_post,
        });
        Ok((Some(run.run_id), data, Vec::new()))
    })
}

/// `forge contract integrate <run-or-task-id>` — re-apply a completed run's patch
/// onto the current HEAD as a linked attempt (R27/KTD8). Only when the task's
/// dependencies are accepted into HEAD; a patch that no longer applies (3-way merge
/// conflict) or an incomplete run is a typed `CONTRACT_NOT_INTEGRABLE` refusal.
fn integrate_response(request_id: Option<String>, args: ContractIntegrateArgs) -> ResponseEnvelope {
    command_result("contract integrate", request_id, move |cwd, request_id| {
        let repo_root = forge_store::repository_root_path(&cwd)?;
        let run = forge_store::contract_run_by_ref(&cwd, &args.target)?.ok_or_else(|| {
            ForgeError::ContractNotIntegrable {
                reason: format!("no run found for {:?}", args.target),
            }
        })?;
        if run.outcome != "completed" {
            return Err(ForgeError::ContractNotIntegrable {
                reason: format!(
                    "run {} did not complete (outcome {})",
                    run.run_id, run.outcome
                ),
            }
            .into());
        }
        let post_ref =
            run.patch_content_ref
                .clone()
                .ok_or_else(|| ForgeError::ContractNotIntegrable {
                    reason: format!("run {} has no produced patch", run.run_id),
                })?;

        // Deps gate (KTD8): every declared dependency must be accepted into HEAD.
        let revision = forge_store::contract_revision(&cwd, &run.contract_id, run.revision)?
            .ok_or_else(|| ForgeError::ContractNotFrozen {
                contract_id: run.contract_id.clone(),
                revision: run.revision,
            })?;
        for dep in parse_depends_on(&revision.source_yaml)? {
            if !forge_store::contract_integration_accepted(&cwd, &dep)? {
                return Err(ForgeError::ContractNotIntegrable {
                    reason: format!("dependency {dep} is not accepted into HEAD"),
                }
                .into());
            }
        }

        // The 3-way merge base is the recorded post-dependency baseline (KTD7/KTD8),
        // so dependency changes are never re-attributed to the agent's patch.
        let baseline_ref: String =
            serde_json::from_str::<Value>(run.dependency_stack_json.as_deref().unwrap_or("{}"))
                .ok()
                .and_then(|value| {
                    value
                        .get("baseline_ref")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .ok_or_else(|| ForgeError::ContractNotIntegrable {
                    reason: format!(
                        "run {} has no recorded baseline for reconstruction",
                        run.run_id
                    ),
                })?;

        // Integrate materializes into an ISOLATED new attempt workspace (like
        // `attempt start`), never the user's main worktree — so the DIRTY_WORKTREE
        // guard (which snapshots the effective worktree and requires an active
        // attempt) does not apply here (KTD7's scratch-workspace rationale). accept's
        // HEAD == base_head STALE_BASE invariant remains the HEAD guard (KTD8): the
        // attempt is created on the actual current HEAD below.
        let head_commit = current_base(&cwd)?;
        let head_tree = owner_base_content_ref(&cwd, &head_commit)?;

        // Re-apply as a 3-way merge (base=baseline, ours=patch, theirs=HEAD). A
        // conflict is a typed refusal — never a silent merge (KTD8).
        let store = NativeObjectStore::new(&repo_root);
        let merge = merge_native_content_refs(&store, &baseline_ref, &post_ref, &head_tree)?;
        if !merge.is_clean() {
            return Err(ForgeError::ContractNotIntegrable {
                reason: format!(
                    "patch no longer applies onto HEAD: {} conflicting path(s). Re-run the task fresh against the current base",
                    merge.conflicts.len()
                ),
            }
            .into());
        }
        let merged_ref =
            merge
                .merged_content_ref
                .ok_or_else(|| ForgeError::ContractNotIntegrable {
                    reason: "clean merge produced no content ref".to_string(),
                })?;

        // Create the attempt on the actual HEAD via the existing lifecycle. The
        // synthesized intent encodes contract id@rev + task so a later deps-gate can
        // recognize an accepted integration (KTD8). request_id anchors the integrate
        // op (below), so start_attempt runs unkeyed.
        let task_id = run
            .tasks
            .last()
            .map(|task| task.task_id.clone())
            .unwrap_or_else(|| run.contract_id.clone());
        let intent_text =
            forge_store::contract_integration_intent_text(&run.contract_id, run.revision, &task_id);
        let started =
            forge_store::start_attempt(&cwd, None, intent_text, head_commit.clone(), None)?;
        // Materialize the merged tree into the attempt's workspace and record it.
        materialize_content_ref(&repo_root, Path::new(&started.workspace_path), &merged_ref)?;
        forge_store::record_attempt_workspace_materialized(&cwd, &started.attempt_id, &merged_ref)?;

        let link = forge_store::record_contract_integration(
            &cwd,
            request_id,
            ContractIntegrationRecord {
                run_id: run.run_id.clone(),
                contract_id: run.contract_id.clone(),
                revision: run.revision,
                task_id,
                attempt_id: started.attempt_id.clone(),
                intent_id: started.intent_id.clone(),
            },
        )?;

        let data = json!({
            "run_id": link.run_id,
            "contract_id": link.contract_id,
            "revision": link.revision,
            "task_id": link.task_id,
            "attempt_id": link.attempt_id,
            "intent_id": link.intent_id,
            "base_head": head_commit,
            "content_ref": merged_ref,
        });
        Ok((Some(started.operation_id), data, Vec::new()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_match_crosses_slashes_like_fnmatch() {
        assert!(glob_match("crates/forge-core/src/lib.rs", "crates/**"));
        assert!(glob_match(
            "crates/forge-core/src/lib.rs",
            "crates/forge-core/src/lib.rs"
        ));
        assert!(glob_match("a/b/c.rs", "a/*/c.rs"));
        assert!(!glob_match("a/b/c.txt", "a/*/c.rs"));
    }

    #[test]
    fn module_base_matches_python() {
        assert_eq!(module_base(Path::new("src/foo.rs")), vec!["foo"]);
        assert_eq!(module_base(Path::new("src/foo/mod.rs")), vec!["foo"]);
        assert_eq!(module_base(Path::new("src/foo/bar.rs")), vec!["foo", "bar"]);
        assert!(module_base(Path::new("src/lib.rs")).is_empty());
        assert!(module_base(Path::new("tests/it.rs")).is_empty());
    }

    #[test]
    fn task_instruction_wording_is_the_verbatim_harness_text() {
        // R6: the stop-rule wording must travel verbatim. The constant IS the
        // single harness source (`include_str!`), so it can never drift; assert the
        // load-bearing stop wording is present so a future inline copy stays honest.
        let text = CONTRACT_TASK_INSTRUCTION;
        assert!(
            text.starts_with("\n--- TASK INSTRUCTION ---\n"),
            "unexpected leading framing: {text:?}"
        );
        assert!(text.contains("STOP: write"), "missing STOP directive");
        assert!(text.contains("UNKNOWN.md at the repo root"));
        assert!(text.contains("blocking/assumption/observation"));
        assert!(text.contains("acceptance.fix"));
        assert!(text.contains("acceptance.guard"));
    }

    #[test]
    fn scan_candidates_is_mod_nesting_aware() {
        let text = "fn top() {}\nmod inner {\n    fn nested() {}\n}\nfn last() {}\n";
        let cands = scan_candidates(text, &["base".to_string()]);
        assert!(cands.contains(&"base::top".to_string()));
        assert!(cands.contains(&"base::inner::nested".to_string()));
        assert!(cands.contains(&"base::last".to_string()));
    }
}
