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
use forge_protocol::ResponseEnvelope;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::{
    command_result, ContractArgs, ContractCommand, ContractFreezeArgs, ContractLintArgs, ForgeError,
};

/// Reserved ledger contract id for the repo-level global policy file.
const GLOBAL_POLICY_ID: &str = "_global-policy";

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
    }
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
fn glob_match(path: &str, pattern: &str) -> bool {
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
    fn scan_candidates_is_mod_nesting_aware() {
        let text = "fn top() {}\nmod inner {\n    fn nested() {}\n}\nfn last() {}\n";
        let cands = scan_candidates(text, &["base".to_string()]);
        assert!(cands.contains(&"base::top".to_string()));
        assert!(cands.contains(&"base::inner::nested".to_string()));
        assert!(cands.contains(&"base::last".to_string()));
    }
}
