//! CCX native contracts (U8): the read + triage surface of the `forge contract`
//! family — `stops`, `show`, `verdicts` (read-only, R23) and `resolve`
//! (mutating triage, R10/R24). These are the adopter-facing `--json` surfaces a
//! cold-start operator (human or agent) drives the loop from, so the `data`
//! shapes are stable snake_case.
//!
//! The reads route through `command_result` WITHOUT a repo lock (like
//! `contract brief`): they are not in `is_mutating_command`, so no write
//! transaction or lock is taken. `resolve` IS mutating — it freezes the bump
//! revision and re-signs the stop in one store transaction (`command_result`
//! holds the repo lock and threads `--request-id` replay per KTD6).
//!
//! The blocked/runnable status of a contract (R23) reuses the SINGLE dependency
//! closure source, `contract::contract_dependency_closure`, that `contract run`'s
//! Leg-3 refusal walks — so `show` and the refusal can never disagree.

use anyhow::{anyhow, bail, Result};
use forge_protocol::ResponseEnvelope;
use serde_json::{json, Value};

use crate::commands::contract::{
    canonicalize_operand, contract_dependency_closure, lint_contract_file, read_source_string,
};
use crate::{
    command_result, ContractResolveArgs, ContractShowArgs, ContractStopsArgs, ContractVerdictsArgs,
};

/// The recognized stop-kind vocabulary (mirrors `parse_unknown_fields`). A
/// reconstruction `--kind` must be one of these.
const STOP_KINDS: [&str; 3] = ["blocking", "assumption", "observation"];

/// Serialize a stop record and annotate it with its `blocking` status (an open
/// stop blocks reruns of its contract's dependency closure, R10/R23).
fn stop_json(stop: &forge_store::ContractStopRecord) -> Result<Value> {
    let mut value = serde_json::to_value(stop)?;
    if let Some(object) = value.as_object_mut() {
        object.insert("blocking".to_string(), json!(stop.state == "open"));
    }
    Ok(value)
}

/// `forge contract stops [--open] [--contract-id <id>]` — read-only (no repo
/// lock). Lists stop records with the four triageable fields and the malformed
/// flag (R23/AE9 query half).
pub(crate) fn stops_response(
    request_id: Option<String>,
    args: ContractStopsArgs,
) -> ResponseEnvelope {
    command_result("contract stops", request_id, move |cwd, _| {
        let stops = forge_store::contract_stops(&cwd, args.contract_id.as_deref(), args.open)?;
        let stops_json = stops.iter().map(stop_json).collect::<Result<Vec<_>>>()?;
        Ok((
            None,
            json!({
                "open_only": args.open,
                "count": stops_json.len(),
                "stops": stops_json,
            }),
            Vec::new(),
        ))
    })
}

/// `forge contract show <run-or-task-or-contract-ref>` — read-only. A run/task ref
/// shows the run record with per-task outcomes, a tally (stops counted as
/// successes-pending-triage per R9/Leg 2), and per-task excerpt presence. Any other
/// ref is read as a contract id: its current frozen revision and its
/// blocked/runnable status over its OWN dependency closure (R23).
pub(crate) fn show_response(
    request_id: Option<String>,
    args: ContractShowArgs,
) -> ResponseEnvelope {
    command_result("contract show", request_id, move |cwd, _| {
        // A contract id takes priority: a chain task id IS a contract id (a run's
        // per-task rows key on the task's contract), so `show <contract-id>` must
        // resolve to the contract, not a run that happens to reference it as a task.
        // A run id never matches a frozen contract, so `show <run-id>` still shows
        // the run via the fallback below — unambiguous in both directions.
        if let Some(revision) = forge_store::latest_contract_revision(&cwd, &args.target)? {
            return Ok((
                None,
                show_contract_json(&cwd, &args.target, &revision)?,
                Vec::new(),
            ));
        }
        match forge_store::contract_run_by_ref(&cwd, &args.target)? {
            Some(run) => Ok((None, show_run_json(&run)?, Vec::new())),
            None => Err(anyhow!(
                "no frozen contract, run, or task found for {:?}",
                args.target
            )),
        }
    })
}

fn show_run_json(run: &forge_store::ContractRunRecord) -> Result<Value> {
    // Tally: stops count as successes pending triage (R9/Leg 2), never failures.
    let mut completed = 0i64;
    let mut stopped = 0i64;
    let mut failed = 0i64;
    let mut skipped = 0i64;
    let mut pending = 0i64;
    let task_summaries: Vec<Value> = run
        .tasks
        .iter()
        .map(|task| {
            match task.outcome.as_str() {
                "completed" => completed += 1,
                "stopped" => stopped += 1,
                "failed" => failed += 1,
                "skipped" => skipped += 1,
                _ => pending += 1,
            }
            json!({
                "task_id": task.task_id,
                "task_index": task.task_index,
                "outcome": task.outcome,
                "agent_exit_code": task.agent_exit_code,
                "patch_present": task.patch_content_ref.is_some(),
                "stdout_excerpt_present": task.agent_stdout_excerpt.is_some(),
                "stderr_excerpt_present": task.agent_stderr_excerpt.is_some(),
            })
        })
        .collect();
    let tally = json!({
        "total": run.tasks.len(),
        "completed": completed,
        "stopped": stopped,
        "failed": failed,
        "skipped": skipped,
        "pending": pending,
        // A stop is a success pending triage, so it joins `completed` in the
        // success count and is never tallied as a failure (R9).
        "successes_pending_triage": completed + stopped,
    });
    Ok(json!({
        "kind": "run",
        "run": serde_json::to_value(run)?,
        "tally": tally,
        "tasks": task_summaries,
    }))
}

fn show_contract_json(
    cwd: &std::path::Path,
    contract_id: &str,
    revision: &forge_store::ContractRevisionRecord,
) -> Result<Value> {
    // Blocked/runnable status over the contract's OWN dependency closure — the same
    // fixpoint walk `contract run`'s Leg-3 refusal uses (single source).
    let closure = contract_dependency_closure(cwd, [contract_id.to_string()])?;
    let open = forge_store::open_stops_for_contracts(cwd, &closure)?;
    let blocking_stop_ids: Vec<String> = open.iter().map(|stop| stop.stop_id.clone()).collect();
    Ok(json!({
        "kind": "contract",
        "contract_id": contract_id,
        "revision": serde_json::to_value(revision)?,
        "dependency_closure": closure,
        "runnable": blocking_stop_ids.is_empty(),
        "blocked": !blocking_stop_ids.is_empty(),
        "blocking_stop_ids": blocking_stop_ids,
    }))
}

/// `forge contract verdicts <run-or-task-ref>` — read-only. Lists the recorded
/// check verdicts (kind, pass/fail, command) for a run (R23).
pub(crate) fn verdicts_response(
    request_id: Option<String>,
    args: ContractVerdictsArgs,
) -> ResponseEnvelope {
    command_result("contract verdicts", request_id, move |cwd, _| {
        let run = forge_store::contract_run_by_ref(&cwd, &args.target)?
            .ok_or_else(|| anyhow!("no run or task found for {:?}", args.target))?;
        let verdicts = forge_store::contract_run_verdicts(&cwd, &run.run_id)?;
        Ok((
            None,
            json!({
                "run_id": run.run_id,
                "count": verdicts.len(),
                "verdicts": serde_json::to_value(&verdicts)?,
            }),
            Vec::new(),
        ))
    })
}

/// `forge contract resolve <stop-id>` — mutating triage (R10/R24). Two modes:
/// `--revised <yaml>` lints and freezes the revised YAML as the bump revision and
/// links it; `--reject --rationale <text>` bumps the revision recording the
/// rationale WITHOUT changing contract content. Both freeze a new revision and
/// resolve the stop atomically (one store txn, R18 replay via `command_result`).
/// Malformed stops accept `--what-needed/--why-unanswered/--kind/--evidence` to
/// reconstruct the four fields inline before resolving (R8/R25); supplied values
/// are redacted and re-signed by the store.
pub(crate) fn resolve_response(
    request_id: Option<String>,
    args: ContractResolveArgs,
) -> ResponseEnvelope {
    command_result("contract resolve", request_id, move |cwd, request_id| {
        if args.revised.is_none() && !args.reject {
            bail!(
                "resolve requires exactly one of --revised <yaml> or --reject --rationale <text>"
            );
        }
        // Validate a supplied reconstruction kind against the stop vocabulary; the
        // store recomputes `malformed` on field presence trusting this check.
        if let Some(kind) = args.kind.as_deref() {
            if !STOP_KINDS.contains(&kind) {
                bail!("--kind must be one of blocking|assumption|observation (got {kind:?})");
            }
        }

        // The stop must exist so we can resolve the SAME contract it belongs to.
        let stop = forge_store::contract_stop(&cwd, &args.stop_id)?
            .ok_or_else(|| anyhow!("contract stop {:?} not found", args.stop_id))?;
        if stop.state != "open" {
            bail!(
                "contract stop {:?} is not open (already resolved)",
                args.stop_id
            );
        }
        // Reconstruction backfills a BEST-EFFORT malformed ingest ONLY (U8 review
        // addendum): refuse the four field flags against a well-formed stop so an
        // agent-authored stop's fields can never be silently rewritten and re-signed.
        let has_reconstruction = args.what_needed.is_some()
            || args.why_unanswered.is_some()
            || args.kind.is_some()
            || args.evidence.is_some();
        if has_reconstruction && !stop.malformed {
            bail!(
                "reconstruction flags (--what-needed/--why-unanswered/--kind/--evidence) apply only to a malformed stop; stop {:?} is well-formed",
                args.stop_id
            );
        }

        // Determine the bump revision's source bytes and resolution kind.
        let (resolution_kind, source_yaml) = if let Some(revised_path) = &args.revised {
            // Revision bump: lint the revised YAML, require it clean, and require it
            // to name the SAME contract as the stop before freezing its exact bytes.
            let path = canonicalize_operand(&cwd, revised_path)?;
            let repo_root = forge_store::repository_root_path(&cwd)?;
            let outcome = lint_contract_file(&path, &repo_root)?;
            outcome.ensure_lint_clean()?;
            if outcome.contract_id != stop.contract_id {
                bail!(
                    "revised contract id {:?} does not match the stop's contract {:?}",
                    outcome.contract_id,
                    stop.contract_id
                );
            }
            ("revision", read_source_string(&path)?)
        } else {
            // Explicit rejection: freeze the CURRENT frozen bytes verbatim so content
            // is unchanged (R10); the rationale (clap-required) records the decision.
            let current = forge_store::latest_contract_revision(&cwd, &stop.contract_id)?
                .ok_or_else(|| {
                    anyhow!(
                        "cannot reject: contract {:?} has no frozen revision",
                        stop.contract_id
                    )
                })?;
            ("rejection", current.source_yaml)
        };

        let reconstruction = forge_store::StopFieldReconstruction {
            what_needed: args.what_needed.clone(),
            why_unanswered: args.why_unanswered.clone(),
            kind: args.kind.clone(),
            evidence: args.evidence.clone(),
        };
        let (revision, resolved) = forge_store::resolve_contract_stop(
            &cwd,
            request_id,
            forge_store::ResolveContractStopInput {
                stop_id: args.stop_id.clone(),
                resolution_kind: resolution_kind.to_string(),
                resolution_rationale: args.rationale.clone(),
                source_yaml,
                reconstruction,
            },
        )?;

        Ok((
            None,
            json!({
                "resolution_kind": resolution_kind,
                "resolved_stop": stop_json(&resolved)?,
                "revision": serde_json::to_value(&revision)?,
            }),
            Vec::new(),
        ))
    })
}
