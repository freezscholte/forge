//! CCX native contracts (U7): the `forge contract verify` fix/guard verifier.
//!
//! This ports `tools/ccx/verify-task.sh`'s semantics onto native primitives: given
//! a COMPLETED run, rebuild its exact post-state base in a fresh scratch workspace,
//! then run the frozen contract's acceptance `fix` set followed by its `guard` set,
//! recording a signed per-command verdict row for every entry plus one aggregate
//! verdict (R13/KTD4). The load-bearing invariants carried over verbatim:
//!
//! - **Guards ALWAYS run, even when a fix failed** (verify-task.sh's
//!   record-completeness rule): the record must show the guard outcomes regardless,
//!   so a `task works but broke something pre-existing` (exit 4) is mechanically
//!   distinguishable from `not done` (exit 2).
//! - **Fail-closed standalone eval-sink gate** (R15): EVERY command is checked
//!   against the single-source-of-truth grammar `forge_store::check_acceptance_command`
//!   BEFORE anything executes. A non-conforming command is `CONTRACT_GRAMMAR_VIOLATION`
//!   and nothing runs — verify does not rely on the frozen contract having been
//!   lint-clean (verify-task.sh gates `--dump-acceptance` the same way, so a
//!   standalone invocation without the runner's lint preflight is still safe).
//! - **Rebuilt base, never the user worktree** (KTD7): the run's `patch_content_ref`
//!   is the full post-run tree (base + per-id dependency overlays + the agent's
//!   changes), so materializing it reconstructs the exact tree the acceptance
//!   commands must run against. Commands run with `cwd` = the scratch root.
//!
//! Every command executes through `forge-evidence`'s `capture_with_timeout` (argv,
//! not a shell — the grammar refuses metacharacters, so a whitespace split is safe),
//! so output excerpts inherit `EXCERPT_LIMIT` + secret redaction before they land on
//! a signed verdict row (R16). Exit-code mapping (R14/R25): all green → 0 (passed),
//! any fix failed → 2 (fix_failed, `CONTRACT_FIX_FAILED`), fix green but any guard
//! failed → 4 (guard_regressed, `CONTRACT_GUARD_REGRESSED`). The envelope status is
//! always SUCCESS — the verdicts recorded their outcome — and `main` maps
//! `data.exit_code` to the process exit, exactly like `contract run`.

use anyhow::{anyhow, Result};
use forge_content_native::materialize_content_ref;
use forge_evidence::capture_with_timeout;
use forge_protocol::ResponseEnvelope;
use forge_store::{AcceptanceCommandCheck, ContractRunVerdictInput, ContractVerifyOutcome};
use serde_json::{json, Value};
use std::path::Path;

use super::contract::{parse_acceptance_shape, AcceptanceShape};
use crate::{command_result, ContractVerifyArgs, ForgeError};

/// Per-command capture timeout. A cargo command on the tiny rebuilt tree finishes in
/// well under this; the ceiling only guards a pathological hang. Longer than the
/// evidence-capture default (30s) so a cold-cache `cargo build`/`cargo test` on CI
/// is not killed mid-compile and mis-recorded as a failure.
const VERIFY_COMMAND_TIMEOUT_MS: u64 = 180_000;

/// `forge contract verify <run-or-task-ref>` — mutating (it writes verdict rows), so
/// it routes through `command_result`: repo lock + request-id replay. A replay of the
/// same request id returns the recorded result WITHOUT re-executing the acceptance
/// commands (the pre-flight replay short-circuits before this closure runs, KTD6).
///
/// Lock note: like `contract run`, verify holds the repo advisory lock across the
/// whole command INCLUDING the acceptance-command executions below — the plan's
/// single-run-at-a-time assumption (concurrent chain runs deferred). It is not in
/// `requires_repo_lock`'s carve-out list, so the lock is held; keep it that way.
pub(crate) fn verify_response(
    request_id: Option<String>,
    args: ContractVerifyArgs,
) -> ResponseEnvelope {
    command_result("contract verify", request_id, move |cwd, request_id| {
        let repo_root = forge_store::repository_root_path(&cwd)?;

        // 1. Resolve the run. Verify only makes sense against a COMPLETED run that
        // produced a patch; a stopped/failed/blast-violation run (or one whose patch
        // object is missing) is a typed refusal — nothing is materialized or executed.
        let run = forge_store::contract_run_by_ref(&cwd, &args.target)?.ok_or_else(|| {
            ForgeError::ContractNotIntegrable {
                reason: format!("no run found for {:?}", args.target),
            }
        })?;
        if run.outcome != "completed" {
            return Err(ForgeError::ContractNotIntegrable {
                reason: format!(
                    "verify requires a completed run; run {} has outcome {}",
                    run.run_id, run.outcome
                ),
            }
            .into());
        }
        let post_ref =
            run.patch_content_ref
                .clone()
                .ok_or_else(|| ForgeError::ContractNotIntegrable {
                    reason: format!("run {} has no produced patch to verify", run.run_id),
                })?;

        // 2. Read the acceptance sets from the frozen revision (default: the run's
        // recorded revision; `--revision` overrides). A missing revision is the typed
        // CONTRACT_NOT_FROZEN refusal (R2).
        let revision = args.revision.unwrap_or(run.revision);
        let record =
            forge_store::contract_revision(&cwd, &run.contract_id, revision)?.ok_or_else(|| {
                ForgeError::ContractNotFrozen {
                    contract_id: run.contract_id.clone(),
                    revision,
                }
            })?;
        let (fix, guard) = parse_acceptance(&record.source_yaml)?;

        // 3. FAIL-CLOSED eval-sink gate (R15): grammar-check EVERY command BEFORE any
        // execution. A single non-conforming command refuses the whole verify with
        // CONTRACT_GRAMMAR_VIOLATION and nothing runs — verify never trusts that the
        // frozen contract was lint-clean.
        for cmd in fix.iter().chain(guard.iter()) {
            if !matches!(
                forge_store::check_acceptance_command(cmd),
                AcceptanceCommandCheck::Ok
            ) {
                return Err(ForgeError::ContractGrammarViolation {
                    command: cmd.clone(),
                }
                .into());
            }
        }

        // 4. Rebuild the exact post-state base in a fresh scratch workspace (KTD7):
        // the run's patch_content_ref is the full post-run tree, so materializing it
        // reconstructs base + per-id dependency overlays + the agent's changes. The
        // acceptance commands run with cwd = the scratch root, never the user worktree.
        let scratch = tempfile::tempdir()?;
        materialize_content_ref(&repo_root, scratch.path(), &post_ref)?;

        // The task the verdicts attribute to: the run's target task (its last per-task
        // row), falling back to the contract id.
        let task_id = run
            .tasks
            .last()
            .map(|task| task.task_id.clone())
            .unwrap_or_else(|| run.contract_id.clone());

        // 5. Execute fix then guard. Guards ALWAYS run, even when a fix failed
        // (verify-task.sh record-completeness) — so both sets are executed
        // unconditionally and every command gets a verdict row.
        let mut verdicts: Vec<ContractRunVerdictInput> = Vec::new();
        let mut fix_json: Vec<Value> = Vec::new();
        let mut guard_json: Vec<Value> = Vec::new();
        let fix_failed = run_set(
            scratch.path(),
            "fix",
            &task_id,
            &fix,
            &mut verdicts,
            &mut fix_json,
        )?;
        let guard_failed = run_set(
            scratch.path(),
            "guard",
            &task_id,
            &guard,
            &mut verdicts,
            &mut guard_json,
        )?;

        // 6. Outcome mapping (R14/R25). Fix failure dominates (exit 2); a guard
        // regression on an all-green fix set is exit 4; otherwise exit 0.
        let (outcome, exit_code, code) = if fix_failed {
            (
                ContractVerifyOutcome::FixFailed,
                2u64,
                Some("CONTRACT_FIX_FAILED"),
            )
        } else if guard_failed {
            (
                ContractVerifyOutcome::GuardRegressed,
                4,
                Some("CONTRACT_GUARD_REGRESSED"),
            )
        } else {
            (ContractVerifyOutcome::Passed, 0, None)
        };

        // Aggregate verdict on the run: the single roll-up of fix ∧ guard (KTD4). This
        // guarantees at least one verdict row exists even for an empty acceptance set,
        // so the store's non-empty-batch guard always holds.
        verdicts.push(ContractRunVerdictInput {
            task_id: Some(task_id.clone()),
            verdict_kind: "aggregate".to_string(),
            command: None,
            passed: !fix_failed && !guard_failed,
            detail: Some(format!(
                "outcome={} fix={} guard={}",
                outcome.as_str(),
                fix.len(),
                guard.len()
            )),
            evidence_id: None,
        });

        // 7. Record all verdicts atomically under the "contract verify" op so a
        // same-request-id replay folds to this result (KTD6).
        let recorded =
            forge_store::record_contract_verify_verdicts(&cwd, request_id, &run.run_id, verdicts)?;
        let verdict_ids: Vec<String> = recorded.iter().map(|v| v.verdict_id.clone()).collect();

        let mut data = json!({
            "outcome": outcome.as_str(),
            "exit_code": exit_code,
            "run_id": run.run_id,
            "contract_id": run.contract_id,
            "revision": revision,
            "fix": fix_json,
            "guard": guard_json,
            "verdict_ids": verdict_ids,
        });
        if let Some(code) = code {
            // R25: the typed code travels in a SUCCESS envelope's data (the verdicts
            // recorded their outcome); main.rs maps data.exit_code to the process exit.
            data["code"] = json!(code);
        }
        Ok((Some(run.run_id), data, Vec::new()))
    })
}

/// Execute one acceptance set (`fix` or `guard`) in `workspace`, appending a verdict
/// row and a JSON summary per command. Returns whether ANY command in the set failed.
/// Every command runs (no short-circuit) so the record is complete (R13). Each command
/// is already grammar-checked, so the whitespace split into argv is safe (no shell
/// metacharacters can survive the gate) and `capture_with_timeout` runs it directly —
/// output excerpts inherit EXCERPT_LIMIT + secret redaction (R16).
fn run_set(
    workspace: &Path,
    kind: &str,
    task_id: &str,
    commands: &[String],
    verdicts: &mut Vec<ContractRunVerdictInput>,
    summary: &mut Vec<Value>,
) -> Result<bool> {
    let mut any_failed = false;
    for cmd in commands {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            continue;
        }
        let captured = capture_with_timeout(workspace, &argv, VERIFY_COMMAND_TIMEOUT_MS)?;
        let passed = captured.exit_code == 0 && !captured.timed_out;
        if !passed {
            any_failed = true;
        }
        let duration_ms = captured.ended_at_ms - captured.started_at_ms;
        // The redacted stderr excerpt is the most useful failure context; fall back to
        // stdout. Both are already redacted + capped by the evidence capture (R16).
        let excerpt = if !captured.stderr_excerpt.trim().is_empty() {
            captured.stderr_excerpt.clone()
        } else {
            captured.stdout_excerpt.clone()
        };
        let detail = if passed {
            format!("exit=0 duration_ms={duration_ms}")
        } else if captured.timed_out {
            format!("timed_out duration_ms={duration_ms}\n{excerpt}")
        } else {
            format!(
                "exit={} duration_ms={duration_ms}\n{excerpt}",
                captured.exit_code
            )
        };
        verdicts.push(ContractRunVerdictInput {
            task_id: Some(task_id.to_string()),
            verdict_kind: kind.to_string(),
            command: Some(cmd.clone()),
            passed,
            detail: Some(detail),
            evidence_id: None,
        });
        summary.push(json!({
            "command": cmd,
            "passed": passed,
            "exit_code": captured.exit_code,
            "timed_out": captured.timed_out,
            "duration_ms": duration_ms,
        }));
    }
    Ok(any_failed)
}

/// Parse the `acceptance.fix` and `acceptance.guard` command lists from a frozen
/// revision's verbatim YAML (R1 stored bytes), reusing the linter's single-source
/// [`AcceptanceShape`] parser (U7-review consolidation nit) so the two surfaces can
/// never drift. The YAML is parsed into a serde_json `Value` — the same type the
/// linter feeds `parse_acceptance_shape` — then classified. A non-mapping,
/// non-null `acceptance` fails closed (verify never trusts that the frozen contract
/// was lint-clean); absent/null yields empty sets. Non-string entries inside
/// `fix`/`guard` are dropped by the shared `string_list`, which does not weaken the
/// eval-sink gate: every SURVIVING command is grammar-checked in step 3 before it
/// can run, and a dropped non-string entry is not a command.
fn parse_acceptance(source_yaml: &str) -> Result<(Vec<String>, Vec<String>)> {
    let contract: Value = serde_yaml::from_str(source_yaml)
        .map_err(|err| anyhow!("frozen contract is not valid YAML: {err}"))?;
    match parse_acceptance_shape(&contract) {
        AcceptanceShape::Sets { fix, guard } => Ok((fix, guard)),
        AcceptanceShape::Empty => Ok((Vec::new(), Vec::new())),
        AcceptanceShape::FlatList(_) | AcceptanceShape::Invalid => Err(anyhow!(
            "acceptance must be a mapping with fix/guard for ccx.contract.v1"
        )),
    }
}
