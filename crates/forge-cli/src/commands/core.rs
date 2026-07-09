use anyhow::{Context, Result};
use clap::error::ErrorKind;
use forge_content::{
    classify_content_ref, ContentBackend, ContentRefKind, SnapshotContent, FORGE_TREE_PREFIX,
};
use forge_protocol::{
    ErrorObject, ResponseEnvelope, ResponseStatus, RetryMetadata, RETRY_BACKOFF_MS,
};
use forge_store::ForgeError;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args::*;
use crate::schema;

pub(crate) fn parser_error_response(args: &[String], error: clap::Error) -> ResponseEnvelope {
    let code = match error.kind() {
        ErrorKind::UnknownArgument | ErrorKind::InvalidSubcommand => "UNKNOWN_ARGUMENT",
        ErrorKind::MissingRequiredArgument | ErrorKind::MissingSubcommand => "MISSING_ARGUMENT",
        _ => "USAGE_ERROR",
    };
    structured_error(
        command_from_args(args),
        request_id_from_args(args),
        code,
        error.to_string(),
        json!({ "kind": format!("{:?}", error.kind()) }),
    )
}

pub(crate) fn command_from_args(args: &[String]) -> String {
    let mut positional = Vec::new();
    let mut skip_next = false;
    for arg in args.iter().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "--json" => {}
            "--request-id" => skip_next = true,
            value if value.starts_with("--request-id=") => {}
            value if value.starts_with('-') => {}
            value => positional.push(value.to_string()),
        }
    }

    match positional.as_slice() {
        [] => "forge".to_string(),
        [command] => command.clone(),
        [command, subcommand, ..]
            if matches!(
                command.as_str(),
                "export" | "attempt" | "proposal" | "sync" | "intent" | "org" | "embargo"
            ) =>
        {
            format!("{command} {subcommand}")
        }
        [command, subcommand, ..] if matches!(command.as_str(), "conflict") => {
            format!("{command} {subcommand}")
        }
        [command, ..] => command.clone(),
    }
}

pub(crate) fn request_id_from_args(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--request-id" {
            return iter.next().cloned();
        }
        if let Some(value) = arg.strip_prefix("--request-id=") {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn start_response(request_id: Option<String>, args: IntentArgs) -> ResponseEnvelope {
    command_result("start", request_id, |cwd, request_id| {
        // Fail fast (NER-259) before any persistence: a `--require-tests-pass`
        // (structured) gate reads the parsed `failed` count, so a program with no
        // structured parser makes the gate structurally unsatisfiable (it could never
        // resolve to pass). Tokenize each entry with the SAME `split_whitespace` rule the
        // gate builder uses (`check_spec_json_from_requires`/`parse_gate`,
        // forge-store/src/lib.rs) — first token = program, rest = args, whitespace-only
        // entries dropped (no first token), so we never error on an entry the builder
        // silently skips. Plain `--require` (exit-code) gates accept any program and are
        // intentionally NOT validated here. Returning before persistence means no
        // attempt/intent/worktree is created (a failed-operation row is still recorded by
        // `command_result`'s mutating-error path — fail-fast, not side-effect-free).
        for raw in &args.require_tests_pass {
            let mut tokens = raw.split_whitespace();
            let Some(program) = tokens.next() else {
                continue;
            };
            let gate_args: Vec<String> = tokens.map(str::to_string).collect();
            if !forge_evidence::parsers::has_structured_parser(program, &gate_args) {
                return Err(ForgeError::UnsupportedStructuredGate {
                    program: program.to_string(),
                    gate: raw.clone(),
                }
                .into());
            }
        }
        let base_head = current_base(&cwd)?;
        // Persist declared check gates on the intent (NER-135); competing attempts
        // under this intent inherit the same bar. None => default mode.
        let check_spec_json =
            forge_store::check_spec_json_from_requires(&args.require, &args.require_tests_pass);
        let started = forge_store::start_attempt(
            &cwd,
            request_id,
            args.intent
                .unwrap_or_else(|| "local agent attempt".to_string()),
            base_head.clone(),
            check_spec_json,
        )?;
        let content_ref = owner_base_content_ref(&cwd, &base_head)?;
        materialize_attempt_workspace(&cwd, &started.attempt_id, &content_ref)?;
        Ok((
            Some(started.operation_id.clone()),
            serde_json::to_value(started)?,
            Vec::new(),
        ))
    })
}

pub(crate) fn attempt_response(request_id: Option<String>, args: AttemptArgs) -> ResponseEnvelope {
    match args.command {
        AttemptCommand::Start(args) => {
            command_result("attempt start", request_id, |cwd, request_id| {
                let base_head = current_base(&cwd)?;
                let started = forge_store::start_attempt_for_intent(
                    &cwd,
                    request_id,
                    args.intent,
                    base_head.clone(),
                )?;
                let content_ref = owner_base_content_ref(&cwd, &base_head)?;
                materialize_attempt_workspace(&cwd, &started.attempt_id, &content_ref)?;
                Ok((
                    Some(started.operation_id.clone()),
                    serde_json::to_value(started)?,
                    Vec::new(),
                ))
            })
        }
        AttemptCommand::List => command_result("attempt list", request_id, |cwd, _| {
            Ok((
                None,
                json!({ "attempts": forge_store::list_attempts(&cwd)? }),
                Vec::new(),
            ))
        }),
        AttemptCommand::Show { attempt_id } => {
            command_result("attempt show", request_id, |cwd, _| {
                Ok((
                    None,
                    serde_json::to_value(forge_store::show_attempt(&cwd, &attempt_id)?)?,
                    Vec::new(),
                ))
            })
        }
        AttemptCommand::Attach {
            attempt_id,
            discard_workspace_changes,
        } => {
            command_result("attempt attach", request_id, |cwd, request_id| {
                // NER-134: worktree/base materialization goes through `ContentBackend`,
                // not `forge_content_git::` directly, so git-worktree semantics stay out
                // of core lifecycle code (PRD §23.4). Bind the configured backend once.
                let target_base_head = forge_store::attempt_base_head(&cwd, &attempt_id)?;
                let current_content = snapshot_effective_worktree(&cwd)?;
                let resolved_current = forge_store::resolve_attempt(&cwd, None).ok();
                let latest_content_ref = match resolved_current {
                    Some(resolved) => forge_store::latest_snapshot_content_ref(
                        &cwd,
                        Some(&resolved.attempt.attempt_id),
                    )?
                    .or_else(|| owner_base_content_ref(&cwd, &resolved.attempt.base_head).ok()),
                    None => {
                        let head = current_base(&cwd)?;
                        Some(owner_base_content_ref(&cwd, &head)?)
                    }
                };
                if latest_content_ref.as_deref() != Some(current_content.content_ref.as_str()) {
                    return Err(ForgeError::DirtyWorktree {
                        paths: current_content.changed_paths.clone(),
                    }
                    .into());
                }
                // NER-382: refuse (WORKSPACE_DRIFT) BEFORE any materialization write when
                // the target attempt's workspace dir no longer equals its recorded
                // materialized content — re-materializing below would silently discard
                // those workspace edits. Checked AFTER the switching-baseline dirty-check
                // above, and only skipped by the explicit flag, so
                // --discard-workspace-changes discards workspace-dir drift ONLY and can
                // never bypass DIRTY_WORKTREE.
                if !discard_workspace_changes {
                    forge_store::verify_attempt_workspace_undrifted(&cwd, &attempt_id)?;
                }
                let content_ref = match forge_store::attempt_materialization_ref(&cwd, &attempt_id)?
                {
                    Some(content_ref) => content_ref,
                    None => owner_base_content_ref(&cwd, &target_base_head)?,
                };
                // Restore routes by the ref's own prefix: a `git-tree:` base ref is
                // materialized by the git backend even in a native repo (intentional
                // until the Phase 7 native walker; see ContentBackend::base_content_ref).
                restore_effective_worktree(&cwd, &content_ref)?;
                materialize_attempt_workspace(&cwd, &attempt_id, &content_ref)?;
                let attached =
                    forge_store::attach_attempt(&cwd, request_id, &attempt_id, &content_ref)?;
                Ok((
                    Some(attached.operation_id.clone()),
                    json!({
                        "attempt_id": attempt_id,
                        "content_ref": content_ref,
                        "current_view_id": attached.view_id
                    }),
                    Vec::new(),
                ))
            })
        }
        AttemptCommand::Compare(args) => compare_response(request_id, "attempt compare", args),
    }
}

/// `forge intent list` / `forge intent show <id>` (NER-257) — read-only views of an
/// intent's declared gate spec + linked attempts, sourced from store accessors (no SQL
/// in the CLI). `show` of an unknown id surfaces the typed `UnknownIntent`
/// (`UNKNOWN_INTENT`). Gate program/args are already secret-redacted by the store.
pub(crate) fn intent_response(
    request_id: Option<String>,
    args: IntentCommandArgs,
) -> ResponseEnvelope {
    match args.command {
        IntentCommand::List => command_result("intent list", request_id, |cwd, _| {
            Ok((
                None,
                json!({ "intents": forge_store::intents_list(&cwd)? }),
                Vec::new(),
            ))
        }),
        IntentCommand::Show { intent_id } => command_result("intent show", request_id, |cwd, _| {
            Ok((
                None,
                serde_json::to_value(forge_store::intent_detail(&cwd, &intent_id)?)?,
                Vec::new(),
            ))
        }),
    }
}

/// `forge compare` / `forge attempt compare` — the read-only compare/rank surface
/// (NER-137). Both forms share this handler. Returns the per-intent grouped, ranked
/// comparison; with `--diff <a> <b>` it additionally attaches the file/hunk content
/// diff between the two attempts' proposals. Native refs use the native diff engine;
/// git refs keep the git interop adapter. Read-only: no operation_id, no lock.
/// Secret-risk changed paths are already dropped by the store; any dropped paths in
/// the pairwise diff surface as warnings.
pub(crate) fn compare_response(
    request_id: Option<String>,
    command: &'static str,
    args: CompareArgs,
) -> ResponseEnvelope {
    command_result(command, request_id, |cwd, _| {
        let selector = forge_store::CompareSelector {
            intent_id: args.intent.clone(),
            attempt_id: args.attempt.clone(),
        };
        let comparison = forge_store::compare_attempts(&cwd, selector)?;
        let mut data = serde_json::to_value(&comparison)?;
        let mut warnings = Vec::new();
        if let Some(pair) = &args.diff {
            // clap enforces exactly two values for --diff.
            let ref_a = forge_store::attempt_proposal_content_ref(&cwd, &pair[0])?;
            let ref_b = forge_store::attempt_proposal_content_ref(&cwd, &pair[1])?;
            let tree_diff = diff_content_refs(&cwd, &ref_a, &ref_b, native_diff_options(true))?;
            collect_diff_warnings(&tree_diff, &mut warnings);
            data["diff"] = serde_json::to_value(&tree_diff)?;
        }
        Ok((None, data, warnings))
    })
}

pub(crate) fn diff_response(request_id: Option<String>, args: DiffArgs) -> ResponseEnvelope {
    command_result("diff", request_id, |cwd, _| {
        let options = forge_content_native::DiffOptions {
            include_hunks: true,
            detect_renames: !args.no_renames,
            rename_threshold: args.find_renames.unwrap_or(50),
            rename_limit: 1000,
        };
        let tree_diff = if args.working {
            if args.from.is_some() {
                anyhow::bail!("--working cannot be combined with --from");
            }
            let context = forge_store::open_repository(&cwd)?;
            let store = forge_content_native::NativeObjectStore::new(&context.root_path);
            forge_content_native::diff_working_vs_tree(
                &store,
                &context.worktree_path,
                &args.to,
                &options,
            )?
        } else {
            let from = args
                .from
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("diff requires --from unless --working is set"))?;
            diff_content_refs(&cwd, from, &args.to, options)?
        };
        let mut warnings = Vec::new();
        collect_diff_warnings(&tree_diff, &mut warnings);
        Ok((None, serde_json::to_value(&tree_diff)?, warnings))
    })
}

pub(crate) fn merge_response(request_id: Option<String>, args: MergeArgs) -> ResponseEnvelope {
    command_result("merge", request_id, |cwd, request_id| {
        let proposal = forge_store::proposal_for_merge(&cwd, &args.proposal)?;
        let base_content_ref = owner_base_content_ref(&cwd, &proposal.base_head)?;
        let ours_head = current_base(&cwd)?;
        let ours_content_ref = owner_base_content_ref(&cwd, &ours_head)?;
        let theirs_content_ref = proposal.content_ref.clone();
        let ref_kinds = [
            classify_content_ref(&base_content_ref),
            classify_content_ref(&ours_content_ref),
            classify_content_ref(&theirs_content_ref),
        ];
        if !ref_kinds
            .iter()
            .all(|kind| matches!(kind, ContentRefKind::ForgeTree(_)))
        {
            return Err(forge_store::ForgeError::UnsupportedContentBackend {
                command: "merge".to_string(),
                required: "native".to_string(),
                actual: content_backend_label(&ref_kinds).to_string(),
            }
            .into());
        }
        let store = forge_content_native::NativeObjectStore::new(&cwd);
        let result = forge_content_native::merge_native_content_refs(
            &store,
            &base_content_ref,
            &ours_content_ref,
            &theirs_content_ref,
        )?;
        if let Some(merged_content_ref) = result.merged_content_ref {
            ensure_clean_worktree(&cwd, &merged_content_ref)?;
            restore_effective_worktree(&cwd, &merged_content_ref)?;
            let record = forge_store::record_merge_success(
                &cwd,
                request_id,
                "merge",
                &proposal,
                &forge_store::MergeSuccessInput {
                    base_head: proposal.base_head.clone(),
                    ours_head: ours_head.clone(),
                    base_content_ref,
                    ours_content_ref,
                    theirs_content_ref,
                    merged_content_ref,
                },
            )?;
            Ok((
                Some(record.operation_id.clone()),
                json!({
                    "merged": true,
                    "proposal_id": record.proposal_id,
                    "proposal_revision_id": record.proposal_revision_id,
                    "snapshot_id": record.snapshot_id,
                    "base_content_ref": record.base_content_ref,
                    "ours_content_ref": record.ours_content_ref,
                    "theirs_content_ref": record.theirs_content_ref,
                    "merged_content_ref": record.merged_content_ref,
                    "operation_id": record.operation_id,
                }),
                secret_export_warnings(&result.dropped_secret_paths),
            ))
        } else {
            let input = forge_store::MergeConflictInput {
                context: "native_merge".to_string(),
                proposal_id: Some(proposal.proposal_id.clone()),
                base_head: Some(proposal.base_head.clone()),
                ours_head: Some(ours_head),
                base_content_ref,
                ours_content_ref,
                theirs_content_ref,
                conflicts: result.conflicts,
            };
            let record = forge_store::record_merge_conflict(&cwd, request_id, "merge", &input)?;
            Ok((
                Some(record.operation_id.clone()),
                json!({
                    "merged": false,
                    "proposal_id": proposal.proposal_id,
                    "proposal_revision_id": proposal.proposal_revision_id,
                    "conflict_set_id": record.conflict_set_id,
                    "operation_id": record.operation_id,
                }),
                secret_export_warnings(&result.dropped_secret_paths),
            ))
        }
    })
}

pub(crate) fn conflict_response(
    request_id: Option<String>,
    args: ConflictArgs,
) -> ResponseEnvelope {
    match args.command {
        ConflictCommand::List => command_result("conflict list", request_id, |cwd, _| {
            Ok((
                None,
                serde_json::to_value(forge_store::conflict_list(&cwd)?)?,
                Vec::new(),
            ))
        }),
        ConflictCommand::Show {
            conflict_set_id,
            suggest,
        } => command_result("conflict show", request_id, |cwd, _| {
            Ok((
                None,
                serde_json::to_value(forge_store::conflict_show(&cwd, &conflict_set_id, suggest)?)?,
                Vec::new(),
            ))
        }),
        ConflictCommand::Resolve {
            conflict_set_id,
            tree,
        } => command_result("conflict resolve", request_id, |cwd, request_id| {
            forge_store::preflight_conflict_resolution(&cwd, &conflict_set_id, &tree)?;
            ensure_clean_worktree(&cwd, &tree)?;
            restore_effective_worktree(&cwd, &tree)?;
            let record =
                forge_store::resolve_conflict_with_tree(&cwd, request_id, &conflict_set_id, &tree)?;
            Ok((
                Some(record.operation_id.clone()),
                serde_json::to_value(record)?,
                Vec::new(),
            ))
        }),
    }
}

pub(crate) fn native_diff_options(include_hunks: bool) -> forge_content_native::DiffOptions {
    forge_content_native::DiffOptions {
        include_hunks,
        ..forge_content_native::DiffOptions::default()
    }
}

pub(crate) fn diff_content_refs(
    repo_root: &Path,
    ref_a: &str,
    ref_b: &str,
    options: forge_content_native::DiffOptions,
) -> Result<forge_content::TreeDiff> {
    match (classify_content_ref(ref_a), classify_content_ref(ref_b)) {
        (ContentRefKind::ForgeTree(_), ContentRefKind::ForgeTree(_)) => {
            let store = forge_content_native::NativeObjectStore::new(repo_root);
            forge_content_native::diff_native_content_refs(&store, ref_a, ref_b, &options)
        }
        _ => forge_export_git::diff_trees(repo_root, ref_a, ref_b, options.include_hunks),
    }
}

pub(crate) fn collect_diff_warnings(
    tree_diff: &forge_content::TreeDiff,
    warnings: &mut Vec<String>,
) {
    warnings.extend(secret_export_warnings(&tree_diff.dropped_secret_paths));
    warnings.extend(
        tree_diff
            .warnings
            .iter()
            .map(|warning| warning.message.clone()),
    );
    // Surface hunk truncation at the envelope level so an agent that only reads
    // warnings[] (not data.diff.files[].truncated) knows the diff is incomplete.
    for file in &tree_diff.files {
        if file.truncated {
            warnings.push(format!(
                "diff hunk truncated for {} (body exceeded the per-file cap)",
                file.path
            ));
        }
    }
}

fn native_git_drift_warnings(cwd: &Path, changed_paths: &[String]) -> Vec<String> {
    if changed_paths.is_empty() {
        return Vec::new();
    }

    let Ok(context) = forge_store::open_repository(cwd) else {
        return Vec::new();
    };
    if context.content_backend != "native" || context.worktree_path != context.root_path {
        return Vec::new();
    }

    let Some(git_changed_paths) = git_status_changed_paths(&context.root_path) else {
        return Vec::new();
    };
    let git_clean_paths: Vec<&str> = changed_paths
        .iter()
        .map(String::as_str)
        .filter(|path| !git_changed_paths.contains(*path))
        .collect();
    if git_clean_paths.is_empty() {
        return Vec::new();
    }

    let preview = git_clean_paths
        .iter()
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    let more = git_clean_paths.len().saturating_sub(5);
    let more_suffix = if more == 0 {
        String::new()
    } else {
        format!(" (+{more} more)")
    };
    vec![format!(
        "{} changed path(s) are clean in Git but changed relative to Forge native base: {}{}. This usually means Forge native history and Git HEAD have drifted; review these paths before accepting.",
        git_clean_paths.len(),
        preview,
        more_suffix
    )]
}

fn git_status_changed_paths(repo_root: &Path) -> Option<HashSet<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut paths = HashSet::new();
    let mut entries = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = String::from_utf8_lossy(&entry[3..]).into_owned();
        paths.insert(path);
        if status.contains(&b'R') || status.contains(&b'C') {
            if let Some(source_path) = entries.next() {
                paths.insert(String::from_utf8_lossy(source_path).into_owned());
            }
        }
    }
    Some(paths)
}

pub(crate) fn save_response(
    request_id: Option<String>,
    args: AttemptScopedArgs,
) -> ResponseEnvelope {
    command_result("save", request_id, |cwd, request_id| {
        // NER-134: verify the worktree binding BEFORE snapshotting, so a mismatch fails
        // fast without writing orphan content objects. `save_snapshot` re-checks
        // authoritatively on the write path; this returns the resolved attempt id, which
        // we pass back as an explicit selector.
        let resolved_attempt = forge_store::verify_save_target(&cwd, args.attempt.as_deref())?;
        let private_paths =
            forge_store::local_private_path_exclusions(&cwd, "attempt", resolved_attempt.as_str())?;
        let content = snapshot_effective_worktree_excluding(&cwd, &private_paths)?;
        // Crash boundary (NER-132 U6, debug-only): objects are now durably fsynced
        // but no content_ref row is committed. A crash here must never leave a
        // committed ref pointing at a missing object — the objects are present, the
        // ref is absent.
        forge_content::maybe_crash("after_object_fsync_before_db_commit");
        let private_overlays = forge_store::capture_local_private_overlays(
            &cwd,
            "attempt",
            resolved_attempt.as_str(),
        )?;
        let saved = forge_store::save_snapshot_with_private_overlays(
            &cwd,
            request_id,
            Some(resolved_attempt.as_str()),
            content.content_ref,
            content.changed_paths,
            private_overlays,
        )?;
        // Crash boundary (NER-132 U6, debug-only): the content_ref is committed and
        // durable (synchronous=NORMAL fsyncs the WAL on commit) even if the WAL is
        // not yet checkpointed. On reopen, WAL recovery must show the committed ref
        // AND its durably-retained object.
        forge_content::maybe_crash("after_db_commit_before_checkpoint");
        let warnings = native_git_drift_warnings(&cwd, &saved.changed_paths);
        Ok((
            Some(saved.operation_id.clone()),
            serde_json::to_value(saved)?,
            warnings,
        ))
    })
}

/// Refuse a dirty worktree BEFORE a materializing nav command (restore/checkout/undo) clobbers
/// it (NER-143 R1/R2): the single definition shared by those three chained-nav commands. (Note
/// `attempt attach` also materializes but uses its OWN switching-baseline dirty-check — it
/// compares against the attempt being switched *to*, not the expected/target model here — so it
/// deliberately does not route through this helper.)
///
/// Passes iff the worktree holds the content it is EXPECTED to hold
/// (`current_state.expected_content_ref`, set by the last materializing op; fallback to the
/// latest saved snapshot for a pre-007 / pre-first-materialize repo) **OR** it already holds the
/// `target` we are about to materialize. The OR-target clause is the crash-safety hinge (DR-F1):
/// after a materialize-then-crash before the record txn commits, the worktree holds `target`
/// while `expected_content_ref` is still the prior ref — re-running the same nav command then
/// passes via `worktree == target`, re-materialize is a no-op, and the record txn sets expected.
/// A genuine unsaved edit matches NEITHER and is refused (the safety property is preserved).
pub(crate) fn ensure_clean_worktree(cwd: &Path, target_content_ref: &str) -> Result<()> {
    let current = snapshot_effective_worktree(cwd)?;
    let expected = match forge_store::expected_content_ref(cwd)? {
        Some(expected) => Some(expected),
        None => forge_store::latest_snapshot_content_ref(cwd, None)?,
    };
    let matches_expected = expected.as_deref() == Some(current.content_ref.as_str());
    let matches_target = current.content_ref == target_content_ref;
    if matches_expected || matches_target {
        Ok(())
    } else {
        Err(ForgeError::DirtyWorktree {
            paths: current.changed_paths,
        }
        .into())
    }
}

pub(crate) fn ensure_worktree_matches_expected(cwd: &Path) -> Result<()> {
    let expected = match forge_store::expected_content_ref(cwd)? {
        Some(expected) => expected,
        None => match forge_store::latest_snapshot_content_ref(cwd, None)? {
            Some(content_ref) => content_ref,
            None => {
                let attempt = forge_store::resolve_attempt(cwd, None)?.attempt;
                owner_base_content_ref(cwd, &attempt.base_head)?
            }
        },
    };
    let current = snapshot_effective_worktree(cwd)?;
    if current.content_ref == expected
        || heal_expected_ref_to_current_native_head(cwd, &current.content_ref)?
    {
        Ok(())
    } else {
        Err(ForgeError::DirtyWorktree {
            paths: current.changed_paths,
        }
        .into())
    }
}

pub(crate) fn heal_expected_ref_to_current_native_head(
    cwd: &Path,
    current_content_ref: &str,
) -> Result<bool> {
    let Ok(head) = current_base(cwd) else {
        return Ok(false);
    };
    let Ok(head_content_ref) = native_commit_content_ref(cwd, &head) else {
        return Ok(false);
    };
    if head_content_ref == current_content_ref {
        forge_store::set_materialized_expected_content_ref(cwd, current_content_ref)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) fn ensure_clean_for_sync_import_materialize(cwd: &Path) -> Result<()> {
    let expected = match forge_store::expected_content_ref(cwd)? {
        Some(expected) => expected,
        None => {
            let head = current_base(cwd)?;
            native_commit_content_ref(cwd, &head)?
        }
    };
    let current = snapshot_effective_worktree(cwd)?;
    if current.content_ref == expected {
        Ok(())
    } else {
        Err(ForgeError::DirtyWorktree {
            paths: current.changed_paths,
        }
        .into())
    }
}

pub(crate) fn native_commit_content_ref(cwd: &Path, commit_id: &str) -> Result<String> {
    let context = forge_store::open_repository(cwd)?;
    let id = forge_content_native::ObjectId::parse(commit_id)?;
    let store = forge_content_native::NativeObjectStore::new(&context.root_path);
    let commit = store.read_commit(&id)?;
    Ok(format!("{FORGE_TREE_PREFIX}{}", commit.tree))
}

pub(crate) fn sync_manifest_head_content_ref(
    cwd: &Path,
    manifest: &forge_sync::SyncManifest,
) -> Result<Option<String>> {
    let Some(head) = manifest.native_head.as_deref() else {
        return Ok(None);
    };
    match native_commit_content_ref(cwd, head) {
        Ok(content_ref) => Ok(Some(content_ref)),
        Err(_) => forge_sync::manifest_head_content_ref(manifest),
    }
}

pub(crate) fn restore_response(request_id: Option<String>, args: RestoreArgs) -> ResponseEnvelope {
    command_result("restore", request_id, |cwd, request_id| {
        let content_ref = forge_store::snapshot_content_ref(&cwd, &args.snapshot_id)?;
        ensure_clean_worktree(&cwd, &content_ref)?;
        // NER-134 Piece 1b: refuse to materialize a snapshot that belongs to an attempt
        // other than the one the worktree is bound to — otherwise restore is a second
        // cross-attempt contamination vector (it would clobber the worktree with another
        // attempt's content while `attached_attempt_id` stays put, and a later
        // `save --attempt <bound>` would record the wrong content). Checked BEFORE
        // materialization so the worktree is never clobbered on the error path.
        let bound_attempt = forge_store::resolve_attempt(&cwd, None)?.attempt.attempt_id;
        let snapshot_attempt = forge_store::snapshot_owner_attempt_id(&cwd, &args.snapshot_id)?;
        if snapshot_attempt != bound_attempt {
            return Err(ForgeError::AttemptWorktreeMismatch {
                requested_attempt: snapshot_attempt,
                attached_attempt: bound_attempt,
            }
            .into());
        }
        restore_effective_worktree(&cwd, &content_ref)?;
        let restored =
            forge_store::record_restore(&cwd, request_id, &args.snapshot_id, &content_ref)?;
        Ok((
            Some(restored.operation_id.clone()),
            json!({
                "snapshot_id": args.snapshot_id,
                "content_ref": content_ref,
                "current_view_id": restored.view_id
            }),
            Vec::new(),
        ))
    })
}

pub(crate) fn run_response(request_id: Option<String>, args: RunArgs) -> ResponseEnvelope {
    command_result("run", request_id, |cwd, request_id| {
        if args.command.is_empty() {
            anyhow::bail!("missing command after --");
        }
        let attempt = forge_store::resolve_attempt(&cwd, args.attempt.as_deref())?.attempt;
        if !forge_store::local_private_path_labels(&cwd, "attempt", &attempt.attempt_id)?.is_empty()
        {
            return Err(forge_store::ForgeError::PrivateContentInvalid {
                reason: "private_tainted_evidence_unsupported".to_string(),
            }
            .into());
        }
        ensure_worktree_matches_expected(&cwd)?;
        let worktree = forge_store::effective_worktree_path(&cwd)?;
        let captured =
            forge_evidence::capture_with_timeout(&worktree, &args.command, args.timeout_ms)?;
        // Surface each secret redaction the capture applied as a warnings[] entry
        // (NER-136 §U4), grouped by detector kind with a count.
        let warnings = redaction_warnings(&captured.redactions);
        let recorded = forge_store::record_evidence(
            &cwd,
            request_id,
            args.attempt.as_deref(),
            forge_store::EvidenceInput {
                command: captured.command,
                args: captured.args,
                cwd: captured.cwd,
                exit_code: captured.exit_code,
                started_at_ms: captured.started_at_ms,
                ended_at_ms: captured.ended_at_ms,
                stdout_excerpt: captured.stdout_excerpt,
                stderr_excerpt: captured.stderr_excerpt,
                stdout_truncated: captured.stdout_truncated,
                stderr_truncated: captured.stderr_truncated,
                timed_out: captured.timed_out,
                sensitivity: captured.sensitivity,
                visibility: captured.visibility,
                trust: captured.trust,
                actor: resolve_actor(args.actor.as_deref()),
                structured_json: captured.structured_json,
            },
        )?;
        Ok((
            Some(recorded.operation_id.clone()),
            serde_json::to_value(recorded)?,
            warnings,
        ))
    })
}

pub(crate) fn propose_response(request_id: Option<String>, args: ProposeArgs) -> ResponseEnvelope {
    command_result("propose", request_id, |cwd, request_id| {
        let proposal = forge_store::propose(
            &cwd,
            request_id,
            args.attempt.as_deref(),
            args.summary.as_deref(),
        )?;
        let warnings = native_git_drift_warnings(&cwd, &proposal.changed_paths);
        Ok((
            Some(proposal.operation_id.clone()),
            serde_json::to_value(proposal)?,
            warnings,
        ))
    })
}

pub(crate) fn check_response(
    request_id: Option<String>,
    args: ProposalScopedArgs,
) -> ResponseEnvelope {
    command_result("check", request_id, |cwd, request_id| {
        // The pass/fail verdict is derived inside record_check's IMMEDIATE txn from
        // the evidence row it binds (NER-132 U2), so there is no CLI-layer show()
        // read for a concurrent, lock-free `run` to race.
        let check = forge_store::record_check(
            &cwd,
            request_id,
            args.attempt.as_deref(),
            args.proposal.as_deref(),
        )?;
        Ok((
            Some(check.operation_id.clone()),
            serde_json::to_value(check)?,
            Vec::new(),
        ))
    })
}

pub(crate) fn accept_response(request_id: Option<String>, args: AcceptArgs) -> ResponseEnvelope {
    command_result("accept", request_id, |cwd, request_id| {
        let proposal = forge_store::exportable_proposal(
            &cwd,
            args.attempt.as_deref(),
            args.proposal.as_deref(),
        )?;
        let current_head = current_base(&cwd)?;
        if current_head != proposal.base_head {
            if forge_store::resolved_merge_ours_head(
                &cwd,
                &proposal.proposal_id,
                &proposal.content_ref,
            )?
            .as_deref()
                == Some(current_head.as_str())
            {
                // The proposal was explicitly resolved from a native merge against this
                // head. `decide` writes the two-parent commit from the stored merge
                // metadata, so this is not a stale-base bypass.
            } else {
                let base_content_ref = owner_base_content_ref(&cwd, &proposal.base_head)?;
                let ours_content_ref = owner_base_content_ref(&cwd, &current_head)?;
                return Err(forge_store::StaleBaseConflict {
                    input: forge_store::StaleBaseConflictInput {
                        context: "stale_base_accept".to_string(),
                        expected_head: proposal.base_head.clone(),
                        actual_head: current_head,
                        base_content_ref,
                        ours_content_ref,
                        theirs_content_ref: proposal.content_ref.clone(),
                        changed_paths: proposal.changed_paths.clone(),
                    },
                }
                .into());
            }
        }
        // Evidence gate (NER-135 R6): enforced in-txn inside `decide` unless
        // --allow-unverified. On bypass, surface the non-passing status as a warning.
        forge_store::enforce_trust_policy(
            &cwd,
            forge_store::TrustPolicyAction::Accept,
            &proposal.proposal_revision_id,
        )?;
        let record = forge_store::decide(
            &cwd,
            request_id,
            args.attempt.as_deref(),
            args.proposal.as_deref(),
            "accepted",
            !args.allow_unverified,
            &resolve_actor(args.actor.as_deref()),
        )?;
        let mut warnings = Vec::new();
        if args.allow_unverified {
            if let Some(status) = record.check_status.as_deref() {
                if status != "passed" {
                    warnings.push(format!(
                        "accepted without a passing check (--allow-unverified): status={status}"
                    ));
                }
            }
        }
        Ok((
            Some(record.operation_id.clone()),
            serde_json::to_value(record)?,
            warnings,
        ))
    })
}

pub(crate) fn reject_response(
    request_id: Option<String>,
    args: ProposalScopedArgs,
) -> ResponseEnvelope {
    command_result("reject", request_id, |cwd, request_id| {
        // Reject is never gated on evidence (enforce_check = false).
        let record = forge_store::decide(
            &cwd,
            request_id,
            args.attempt.as_deref(),
            args.proposal.as_deref(),
            "rejected",
            false,
            &resolve_actor(args.actor.as_deref()),
        )?;
        Ok((
            Some(record.operation_id.clone()),
            serde_json::to_value(record)?,
            Vec::new(),
        ))
    })
}

pub(crate) fn show_response(
    request_id: Option<String>,
    args: AttemptScopedArgs,
) -> ResponseEnvelope {
    command_result("show", request_id, |cwd, _request_id| {
        let show = forge_store::show(&cwd, args.attempt.as_deref())?;
        Ok((None, serde_json::to_value(show)?, Vec::new()))
    })
}

pub(crate) fn proposal_response(
    request_id: Option<String>,
    args: ProposalArgs,
) -> ResponseEnvelope {
    match args.command {
        ProposalCommand::List(args) => command_result("proposal list", request_id, |cwd, _| {
            Ok((
                None,
                json!({ "proposals": forge_store::list_proposals(&cwd, args.attempt.as_deref())? }),
                Vec::new(),
            ))
        }),
    }
}

pub(crate) fn checkout_response(
    request_id: Option<String>,
    args: CheckoutArgs,
) -> ResponseEnvelope {
    command_result("checkout", request_id, |cwd, request_id| {
        // Resolve + validate the target FIRST (NOT_FOUND for a typo vs NATIVE_HISTORY_CORRUPT
        // for a ledger-referenced-but-missing commit), before touching the worktree.
        let content_ref = forge_store::checkout_target_content_ref(&cwd, &args.commit_id)?;
        // Refuse a dirty worktree BEFORE materializing (shared helper): the irreversible
        // clobber must not run if there are unsaved changes to lose.
        ensure_clean_worktree(&cwd, &content_ref)?;
        // Materialize the historical tree (policy-excluded; symlink-aware + R15 via U9).
        restore_effective_worktree(&cwd, &content_ref)?;
        // Record the checkout in the op-log (so `undo` can reverse it and gc keeps the target
        // reachable). Checkout does NOT move the base anchor — surfaced as base_unchanged so an
        // agent is not misled into expecting git's HEAD-moving checkout semantics.
        let record = forge_store::record_checkout(&cwd, request_id, &args.commit_id, &content_ref)?;
        Ok((
            Some(record.operation_id.clone()),
            json!({
                "commit_id": args.commit_id,
                "content_ref": content_ref,
                "base_unchanged": true,
                "current_view_id": record.view_id
            }),
            Vec::new(),
        ))
    })
}

pub(crate) fn undo_response(request_id: Option<String>) -> ResponseEnvelope {
    command_result("undo", request_id, |cwd, request_id| {
        // Resolve the prior snapshot to restore (clear "nothing to undo" if none).
        let target = forge_store::undo_target(&cwd)?;
        // Refuse a dirty worktree BEFORE materializing (shared helper): undo must not clobber
        // unsaved edits.
        ensure_clean_worktree(&cwd, &target.content_ref)?;
        // Restore the prior snapshot (policy-excluded, crash-atomic, symlink-aware + R15).
        restore_effective_worktree(&cwd, &target.content_ref)?;
        // Record the undo as a forward op-log operation (never deletes a decisions/op row).
        let record = forge_store::record_undo(
            &cwd,
            request_id,
            &target.undone_operation_id,
            &target.restored_snapshot_id,
            &target.content_ref,
        )?;
        Ok((
            Some(record.operation_id.clone()),
            json!({
                "undone_operation_id": target.undone_operation_id,
                "restored_snapshot_id": target.restored_snapshot_id,
                "content_ref": target.content_ref,
                "current_view_id": record.view_id
            }),
            Vec::new(),
        ))
    })
}

pub(crate) fn log_response(request_id: Option<String>, args: LogArgs) -> ResponseEnvelope {
    // Read-only: "log" is not a mutating command, so command_result takes no lock and runs
    // no reconcile — `native_log` resolves the authoritative tip from the ledger directly,
    // tolerating a not-yet-reconciled HEAD.
    command_result("log", request_id, |cwd, _request_id| {
        let commits = forge_store::native_log(&cwd, args.intent.as_deref())?;
        Ok((None, json!({ "commits": commits }), Vec::new()))
    })
}

pub(crate) fn blame_response(request_id: Option<String>, args: BlameArgs) -> ResponseEnvelope {
    // Read-only like "log": no lock, no reconcile. The tip is resolved from the LEDGER
    // (`native_history_tip`, the same resolution `native_log` uses) — never from the
    // ref-store HEAD, which lags the ledger by design (HEAD-lags-never-leads) and is
    // only reconciled by mutating commands. Blame therefore attributes correctly even
    // in the post-accept window where HEAD has not been reconciled yet.
    command_result("blame", request_id, |cwd, _request_id| {
        let repo_root = forge_store::repository_root_path(&cwd)?;
        let store = forge_content_native::NativeObjectStore::new(&repo_root);
        // NER-387: `--at <commit_id>` selects an explicit historical tip; without it the
        // tip comes from the ledger exactly as before. Rejection mirrors `forge checkout`'s
        // commit-id convention (`checkout_target_content_ref`): a non-parseable /
        // non-commit / absent id is a path-free "unknown commit" user error that
        // `error_to_object` maps to COMMAND_FAILED — no new error code.
        let tip = match args.at.as_deref() {
            Some(at) => {
                let id = match forge_content_native::ObjectId::parse(at) {
                    Ok(id) if matches!(id.kind(), Ok(forge_content_native::ObjectKind::Commit)) => {
                        id
                    }
                    _ => anyhow::bail!("unknown commit: not a native commit id in this repository"),
                };
                if store.read_commit(&id).is_err() {
                    // Parity with `forge checkout`: a store-missing id that the ledger
                    // still references is genuine corruption (NATIVE_HISTORY_CORRUPT), not
                    // a user typo. The shared classifier keeps both paths identical.
                    return Err(forge_store::classify_missing_commit(&cwd, at)?);
                }
                id
            }
            None => forge_store::native_history_tip(&cwd)?.ok_or_else(|| {
                anyhow::Error::from(ForgeError::NoNativeHistory {
                    path: args.path.clone(),
                })
            })?,
        };
        // Map the walk's typed content-native errors onto the ForgeError taxonomy by
        // downcast (never by message text): forge-content-native cannot construct a
        // ForgeError — the dependency runs the other way — so it raises its own
        // ProvenanceError classifier and the bridging happens here (NER-386).
        let map_provenance_error = |error: anyhow::Error| -> anyhow::Error {
            use forge_content_native::provenance::{ProvenanceCorruptionKind, ProvenanceError};
            match error.downcast_ref::<ProvenanceError>() {
                Some(ProvenanceError::BinaryBlob) => ForgeError::BinaryBlob {
                    path: args.path.clone(),
                }
                .into(),
                Some(ProvenanceError::PathMissingAtTip { tip }) => ForgeError::PathNotFound {
                    path: args.path.clone(),
                    tip: tip.clone(),
                }
                .into(),
                Some(ProvenanceError::StoreCorrupt {
                    kind,
                    commit_id,
                    missing_id,
                }) => ForgeError::NativeHistoryCorrupt {
                    kind: match kind {
                        ProvenanceCorruptionKind::DanglingTipCommit => {
                            forge_store::NativeHistoryCorruptKind::DanglingCommitId
                        }
                        ProvenanceCorruptionKind::DanglingParent => {
                            forge_store::NativeHistoryCorruptKind::DanglingParent
                        }
                        ProvenanceCorruptionKind::DanglingTree => {
                            forge_store::NativeHistoryCorruptKind::DanglingTree
                        }
                    },
                    commit_id: commit_id.clone(),
                    related_id: missing_id.clone(),
                }
                .into(),
                // A walk-invariant violation is a should-never-happen inconsistency with
                // no dedicated code; its Display is path-free by construction, so it may
                // fall through to COMMAND_FAILED without leaking the queried path.
                Some(ProvenanceError::WalkInconsistent { .. }) | None => error,
            }
        };
        let provenance =
            forge_content_native::provenance::path_provenance_at(&store, &tip, &args.path)
                .map_err(map_provenance_error)?;
        let attributions =
            forge_content_native::provenance::attribute_lines_at(&store, &tip, &args.path)
                .map_err(map_provenance_error)?;
        let by_commit: std::collections::HashMap<
            &str,
            &forge_content_native::provenance::PathProvenanceEntry,
        > = provenance
            .iter()
            .map(|e| (e.commit_id.as_str(), e))
            .collect();
        // Ledger enrichment (NER-362 slice 4): one `provenance_detail` read per
        // distinct (intent, revision, decision) tuple — O(distinct commits), not
        // O(lines). Ids the ledger doesn't know degrade to None fields inside the
        // lookup; commits with no intent_id skip the lookup entirely. Joins only
        // on ids the payload already carries — the tip resolved above is not
        // re-resolved or altered here.
        type EnrichmentKey = (String, Option<String>, Option<String>);
        let mut details: std::collections::HashMap<EnrichmentKey, forge_store::ProvenanceDetail> =
            std::collections::HashMap::new();
        for entry in &provenance {
            let Some(intent_id) = entry.intent_id.as_deref() else {
                continue;
            };
            let key = (
                intent_id.to_string(),
                entry.proposal_revision_id.clone(),
                entry.decision_id.clone(),
            );
            if let std::collections::hash_map::Entry::Vacant(slot) = details.entry(key) {
                let detail = forge_store::provenance_detail(
                    &cwd,
                    intent_id,
                    entry.proposal_revision_id.as_deref(),
                    entry.decision_id.as_deref(),
                )?;
                slot.insert(detail);
            }
        }
        let detail_for = |entry: &forge_content_native::provenance::PathProvenanceEntry| {
            let intent_id = entry.intent_id.as_deref()?;
            details.get(&(
                intent_id.to_string(),
                entry.proposal_revision_id.clone(),
                entry.decision_id.clone(),
            ))
        };
        let lines: Vec<Value> = attributions
            .iter()
            .map(|line| {
                let entry = by_commit.get(line.commit_id.as_str());
                let detail = entry.and_then(|e| detail_for(e));
                json!({
                    "line_number": line.line_number,
                    "content": line.content,
                    "commit_id": line.commit_id,
                    "intent_id": entry.and_then(|e| e.intent_id.clone()),
                    "proposal_revision_id": entry.and_then(|e| e.proposal_revision_id.clone()),
                    "decision_id": entry.and_then(|e| e.decision_id.clone()),
                    "actor": entry.and_then(|e| e.actor.clone()),
                    "authored_time": entry.and_then(|e| e.authored_time),
                    "intent_title": detail.and_then(|d| d.intent_title.clone()),
                    "decision_status": detail.and_then(|d| d.decision_status.clone()),
                    "check_status": detail.and_then(|d| d.check_status.clone()),
                })
            })
            .collect();
        Ok((
            None,
            json!({
                "path": args.path,
                "tip_commit_id": tip.to_string(),
                "lines": lines,
            }),
            Vec::new(),
        ))
    })
}

pub(crate) fn doctor_response(request_id: Option<String>) -> ResponseEnvelope {
    command_result("doctor", request_id, |cwd, _request_id| {
        let report = forge_store::doctor(&cwd)?;
        let warnings = report.warnings.clone();
        Ok((None, serde_json::to_value(report)?, warnings))
    })
}

pub(crate) fn trust_response(request_id: Option<String>, args: TrustArgs) -> ResponseEnvelope {
    match args.command {
        TrustCommand::Policy(args) => command_result("trust policy", request_id, |cwd, _| {
            let policy = if args.accept.is_some() || args.export.is_some() {
                forge_store::set_trust_policy(&cwd, args.accept.as_deref(), args.export.as_deref())?
            } else {
                forge_store::trust_policy(&cwd)?
            };
            Ok((None, serde_json::to_value(policy)?, Vec::new()))
        }),
        TrustCommand::Attest(args) => match args.command {
            TrustAttestCommand::HostedRunner(args) => {
                command_result("trust attest hosted-runner", request_id, |cwd, _| {
                    let attestation = forge_store::attest_hosted_runner(
                        &cwd,
                        args.attempt.as_deref(),
                        args.proposal.as_deref(),
                        &args.key,
                        &args.issuer,
                    )?;
                    Ok((None, serde_json::to_value(attestation)?, Vec::new()))
                })
            }
            TrustAttestCommand::ThirdParty(args) => {
                command_result("trust attest third-party", request_id, |cwd, _| {
                    let attestation = forge_store::attest_third_party(
                        &cwd,
                        args.attempt.as_deref(),
                        args.proposal.as_deref(),
                        &args.key,
                        &args.issuer,
                    )?;
                    Ok((None, serde_json::to_value(attestation)?, Vec::new()))
                })
            }
        },
    }
}

pub(crate) fn visibility_response(
    request_id: Option<String>,
    args: VisibilityArgs,
) -> ResponseEnvelope {
    match args.command {
        VisibilityCommand::Policy => command_result("visibility policy", request_id, |cwd, _| {
            let policy = forge_store::visibility_policy(&cwd)?;
            Ok((None, serde_json::to_value(policy)?, Vec::new()))
        }),
        VisibilityCommand::Set(args) => command_result("visibility set", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let record = forge_store::set_work_package_visibility(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &args.visibility,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(record)?, Vec::new()))
        }),
        VisibilityCommand::Grant(args) => {
            command_result("visibility grant", request_id, |cwd, _| {
                let actor = resolve_actor(args.actor.as_deref());
                let grant = forge_store::grant_visibility_capability(
                    &cwd,
                    &args.work_package.kind,
                    &args.work_package.id,
                    &args.recipient,
                    &args.capability,
                    &actor,
                    args.reason.as_deref(),
                )?;
                Ok((None, serde_json::to_value(grant)?, Vec::new()))
            })
        }
        VisibilityCommand::Revoke(args) => {
            command_result("visibility revoke", request_id, |cwd, _| {
                let actor = resolve_actor(args.actor.as_deref());
                let grant = forge_store::revoke_visibility_capability(
                    &cwd,
                    &args.work_package.kind,
                    &args.work_package.id,
                    &args.recipient,
                    &args.capability,
                    &actor,
                    args.reason.as_deref(),
                )?;
                Ok((None, serde_json::to_value(grant)?, Vec::new()))
            })
        }
        VisibilityCommand::Check(args) => {
            command_result("visibility check", request_id, |cwd, _| {
                let decision = forge_store::projection_decision(
                    &cwd,
                    &args.work_package.kind,
                    &args.work_package.id,
                    &args.recipient,
                    &args.capability,
                )?;
                Ok((None, serde_json::to_value(decision)?, Vec::new()))
            })
        }
        VisibilityCommand::Path(args) => match args.command {
            VisibilityPathCommand::Set(args) => {
                command_result("visibility path set", request_id, |cwd, _| {
                    let label = forge_store::set_local_private_path_label(
                        &cwd,
                        &args.work_package.kind,
                        &args.work_package.id,
                        &args.path,
                        &args.visibility,
                    )?;
                    Ok((None, serde_json::to_value(label)?, Vec::new()))
                })
            }
        },
    }
}

pub(crate) fn public_projection_mode_value(mode: &str) -> &'static str {
    match mode {
        "provenance-only" => "provenance_only",
        "sanitized-source" => "sanitized_source",
        "full-source" => "full_source",
        _ => unreachable!("clap value_parser restricts public projection modes"),
    }
}

pub(crate) fn embargo_response(request_id: Option<String>, args: EmbargoArgs) -> ResponseEnvelope {
    match args.command {
        EmbargoCommand::Mark(args) => command_result("embargo mark", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::mark_embargo_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
        EmbargoCommand::Grant(args) => command_result("embargo grant", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::grant_embargo_capability(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &args.recipient,
                &args.capability,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
        EmbargoCommand::Revoke(args) => command_result("embargo revoke", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::revoke_embargo_capability(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &args.recipient,
                &args.capability,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
        EmbargoCommand::Release(args) => command_result("embargo release", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let release_plan = forge_store::prepare_embargo_release_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &args.recipient,
                &actor,
                &args.content_classes,
                args.reason.as_deref(),
            )?;
            ensure_release_output_available(&args.output)?;
            let pending_output =
                embargo_release_pending_output(&args.output, &release_plan.release_event_id);
            let _pending_cleanup = PendingReleaseOutput::new(pending_output.clone());
            let report = forge_sync::export_manifest_embargo_release(
                &cwd,
                &pending_output,
                &release_plan.recipient,
                &args.work_package.kind,
                &args.work_package.id,
                release_plan.policy_revision,
                &release_plan.release_event_id,
                release_plan.generated_at_ms,
                release_plan.content_classes.clone(),
                release_plan.generated_at_ms,
                release_plan.revocation_warning.clone(),
            )?;
            let bundle_digest =
                report.projection.bundle_digest.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("embargo release export missing bundle digest")
                })?;
            let release = forge_store::finish_embargo_release_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &args.recipient,
                &actor,
                &release_plan.content_classes,
                &release_plan.release_event_id,
                release_plan.policy_revision,
                release_plan.generated_at_ms,
                bundle_digest,
                args.reason.as_deref(),
            )?;
            forge_sync::publish_manifest_file_atomic_new(&pending_output, &args.output)?;
            let _ = _pending_cleanup.disarm();
            let mut report = report;
            report.output_path = args.output.display().to_string();
            Ok((
                None,
                json!({
                    "release": release,
                    "report": report,
                }),
                Vec::new(),
            ))
        }),
        EmbargoCommand::Reveal(args) => command_result("embargo reveal", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::reveal_embargo_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &actor,
                public_projection_mode_value(&args.mode),
                args.public_actor_ref.as_deref(),
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
        EmbargoCommand::Publish(args) => command_result("embargo publish", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::publish_embargo_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
        EmbargoCommand::Close(args) => command_result("embargo close", request_id, |cwd, _| {
            let actor = resolve_actor(args.actor.as_deref());
            let result = forge_store::close_embargo_workflow(
                &cwd,
                &args.work_package.kind,
                &args.work_package.id,
                &actor,
                args.reason.as_deref(),
            )?;
            Ok((None, serde_json::to_value(result)?, Vec::new()))
        }),
    }
}

struct PendingReleaseOutput {
    path: Option<PathBuf>,
}

impl PendingReleaseOutput {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(mut self) -> PathBuf {
        self.path.take().expect("pending output path")
    }
}

impl Drop for PendingReleaseOutput {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn ensure_release_output_available(output_path: &Path) -> Result<()> {
    if output_path.exists() {
        anyhow::bail!(
            "sync export output already exists: {}",
            output_path.display()
        );
    }
    Ok(())
}

pub(crate) fn embargo_release_pending_output(
    output_path: &Path,
    release_event_id: &str,
) -> PathBuf {
    let file_name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("embargo-release.json");
    output_path.with_file_name(format!(
        ".{file_name}.pending.{release_event_id}.{}",
        std::process::id()
    ))
}

pub(crate) fn key_response(request_id: Option<String>, args: KeyArgs) -> ResponseEnvelope {
    match args.command {
        KeyCommand::Status => command_result("key status", request_id, |cwd, _| {
            let status = forge_store::local_key_status(&cwd)?;
            Ok((None, serde_json::to_value(status)?, Vec::new()))
        }),
        KeyCommand::Rotate => command_result("key rotate", request_id, |cwd, _| {
            let rotation = forge_store::rotate_local_key(&cwd)?;
            Ok((None, serde_json::to_value(rotation)?, Vec::new()))
        }),
    }
}

pub(crate) fn org_response(request_id: Option<String>, args: OrgArgs) -> ResponseEnvelope {
    match args.command {
        OrgCommand::Status => command_result("org status", request_id, |cwd, _| {
            let status = forge_store::org_status(&cwd)?;
            Ok((None, serde_json::to_value(status)?, Vec::new()))
        }),
        OrgCommand::Init(args) => command_result("org init", request_id, |cwd, request_id| {
            let bootstrap = forge_store::init_org_governance(
                &cwd,
                request_id,
                &args.actor,
                args.reason.as_deref(),
            )?;
            Ok((
                Some(bootstrap.operation_id.clone()),
                serde_json::to_value(bootstrap)?,
                Vec::new(),
            ))
        }),
        OrgCommand::Encryption(args) => match args.command {
            OrgEncryptionCommand::BindLocal(args) => {
                command_result("org encryption bind-local", request_id, |cwd, _| {
                    let recipient = forge_store::local_encryption_recipient(&cwd)?;
                    let authority = args.authority_id.as_deref().unwrap_or(&args.principal_id);
                    let binding = forge_store::bind_org_encryption_key(
                        &cwd,
                        &args.principal_id,
                        &recipient,
                        authority,
                        args.reason.as_deref(),
                    )?;
                    Ok((None, serde_json::to_value(binding)?, Vec::new()))
                })
            }
        },
        OrgCommand::DecryptAuthority(args) => {
            command_result("org decrypt-authority", request_id, |cwd, _| {
                let authority = forge_store::private_decrypt_authority(
                    &cwd,
                    &args.work_package.kind,
                    &args.work_package.id,
                    &args.principal_id,
                )?;
                Ok((None, serde_json::to_value(authority)?, Vec::new()))
            })
        }
    }
}

pub(crate) fn gc_response(request_id: Option<String>, args: GcArgs) -> ResponseEnvelope {
    command_result("gc", request_id, |cwd, _request_id| {
        let report = if args.dry_run {
            forge_store::gc_dry_run(&cwd)?
        } else {
            forge_store::gc_delete(&cwd, args.plan_digest.as_deref().unwrap_or_default())?
        };
        Ok((None, serde_json::to_value(report)?, Vec::new()))
    })
}

/// Build the replay response for an already-recorded `(repo, request_id)`
/// operation, preserving the command-aware and status-aware contract: a
/// different command is a `REQUEST_ID_CONFLICT`, a recorded failure replays the
/// failure, and a recorded success replays `idempotent_replay: true`. Shared by
/// the pre-flight check and the in-transaction `RequestIdReplay` path so both
/// give byte-identical replays.
pub(crate) fn replay_response(
    command: &'static str,
    request_id: Option<String>,
    existing: forge_store::RequestIdOperation,
) -> ResponseEnvelope {
    if existing.command != command {
        return ResponseEnvelope::error(
            command,
            request_id,
            None,
            ErrorObject::new(
                "REQUEST_ID_CONFLICT",
                format!("request id already used for command {}", existing.command),
            ),
        );
    }
    if existing.status == "failed" {
        // The code is read directly from the stored `error_json` (recorded by
        // `record_failed_operation`), never re-derived from the message — the
        // substring ladder is gone. Older rows without a stored code fall back to
        // COMMAND_FAILED.
        let error_json = existing.error_json.unwrap_or_default();
        let message = error_json
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "request id replayed failed operation".to_string());
        let code = error_json
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("COMMAND_FAILED")
            .to_string();
        // Re-attach the stored `details` so a replayed failure carries the SAME
        // structured payload as the first response (FIX C). Old rows recorded before
        // details were persisted lack the key — default to an empty object.
        let details = error_json
            .get("details")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        return ResponseEnvelope::error(
            command,
            request_id,
            Some(existing.operation_id),
            ErrorObject::new(code, message).with_details(details),
        );
    }
    let request_id_value = request_id.as_deref().unwrap_or_default().to_string();
    let mut data = json!({ "idempotent_replay": true, "request_id": request_id_value });
    if matches!(command, "sync fetch" | "sync push")
        && matches!(existing.kind.as_deref(), Some("sync_fetch" | "sync_push"))
    {
        if let Some(replay_data) = existing
            .state
            .as_ref()
            .and_then(|state| state.get("replay_data"))
            .and_then(Value::as_object)
        {
            if let Some(object) = data.as_object_mut() {
                for (key, value) in replay_data {
                    object.insert(key.clone(), value.clone());
                }
                object.insert(
                    "operation_id".to_string(),
                    json!(existing.operation_id.clone()),
                );
            }
        }
    }
    if command == "sync pull" && existing.kind.as_deref() == Some("sync_pull_materialized") {
        if let Some(state) = existing.state.as_ref() {
            if let Some(object) = data.as_object_mut() {
                if let Some(state_object) = state.as_object() {
                    for (key, value) in state_object {
                        if key != "lifecycle" {
                            object.insert(key.clone(), value.clone());
                        }
                    }
                }
                object.insert(
                    "materialized_operation_id".to_string(),
                    json!(existing.operation_id.clone()),
                );
                if let Some(view_id) = existing.view_id.as_ref() {
                    object.insert("materialized_view_id".to_string(), json!(view_id));
                }
            }
        }
    }
    // NER-255: lifecycle commands an agent realistically retries after a crash carry
    // their original success `data` payload in the op view state under `replay_data`
    // (persisted in the SAME txn that recorded the op). Merge it back so a replay
    // returns the ORIGINAL ids (snapshot_id / content_ref / proposal_id / …) instead of
    // just {idempotent_replay, request_id}. Gated on BOTH command AND the op kind, so a
    // pre-change row (no `replay_data`) cleanly falls back to today's minimal payload.
    //
    // `accept`/`reject` are intentionally NOT handled here: `decide` records its op under
    // the decision verb ("accepted"/"rejected"), so the `existing.command != command`
    // check above already returns REQUEST_ID_CONFLICT for those — preserving the
    // documented behavior asserted by native_accept_replay_same_request_id_writes_no_second_commit.
    if let Some(expected_kind) = match command {
        "save" => Some("snapshot_saved"),
        "propose" => Some("proposal_created"),
        "start" | "attempt start" => Some("attempt_started"),
        "org init" => Some("org_initialized"),
        _ => None,
    } {
        if existing.kind.as_deref() == Some(expected_kind) {
            if let Some(replay_data) = existing
                .state
                .as_ref()
                .and_then(|state| state.get("replay_data"))
                .and_then(Value::as_object)
            {
                if let Some(object) = data.as_object_mut() {
                    for (key, value) in replay_data {
                        object.insert(key.clone(), value.clone());
                    }
                    // NER-382 replay gap: `attempt_started` rows recorded before the
                    // workspace_role upgrade carry workspace_path in replay_data but no
                    // workspace_role. Inject the constant so a replayed payload matches
                    // the schema promise (workspace_path is always role-qualified).
                    if replay_data.contains_key("workspace_path")
                        && !replay_data.contains_key("workspace_role")
                    {
                        object.insert(
                            "workspace_role".to_string(),
                            json!(forge_store::WORKSPACE_ROLE_MATERIALIZATION_TARGET),
                        );
                    }
                    object.insert(
                        "operation_id".to_string(),
                        json!(existing.operation_id.clone()),
                    );
                    // Re-assert the replay contract flags AFTER the merge so they cannot be
                    // clobbered by a stored `replay_data` key collision. Today's payloads
                    // carry neither key, but the merge has no allow/deny-list, so a future
                    // change that folds `request_id` (or, worse, `idempotent_replay`) into
                    // `replay_data` would otherwise silently corrupt the flag a retrying
                    // agent relies on (NER-255 adversarial review).
                    object.insert("idempotent_replay".to_string(), json!(true));
                    object.insert("request_id".to_string(), json!(request_id_value));
                }
            }
        }
    }
    if matches!(command, "sync fetch" | "sync pull" | "sync push")
        && existing
            .kind
            .as_deref()
            .is_some_and(forge_store::is_sync_merged_op_kind)
    {
        if let Some(state) = existing.state.as_ref() {
            if let Some(object) = data.as_object_mut() {
                object.insert("merged".to_string(), json!(true));
                object.insert(
                    "operation_id".to_string(),
                    json!(existing.operation_id.clone()),
                );
                for key in [
                    "protocol_version",
                    "direction",
                    "remote_path",
                    "merged_content_ref",
                    "materialized",
                    "imported_native_objects",
                    "imported_ledger_rows",
                ] {
                    if let Some(value) = state.get(key) {
                        object.insert(key.to_string(), value.clone());
                    }
                }
                if let Some(value) = state.get("commit_id") {
                    object.insert("merge_commit_id".to_string(), value.clone());
                }
                if let Some(value) = state.get("ours_native_head") {
                    object.insert("base_native_head".to_string(), value.clone());
                    object.insert("receiver_native_head".to_string(), value.clone());
                }
                if let Some(value) = state.get("base_native_head") {
                    object.insert("common_ancestor_native_head".to_string(), value.clone());
                }
                if let Some(value) = state.get("theirs_native_head") {
                    object.insert("source_native_head".to_string(), value.clone());
                }
            }
        }
    }
    ResponseEnvelope::success(command, request_id, Some(existing.operation_id), data)
}

pub(crate) fn reassert_materialized_replay(
    cwd: &Path,
    existing: &forge_store::RequestIdOperation,
) -> anyhow::Result<()> {
    if existing.command != "sync pull" {
        return Ok(());
    }
    let Some(state) = existing.state.as_ref() else {
        return Ok(());
    };
    if !state
        .get("materialized")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let Some(content_ref) = state
        .get("merged_content_ref")
        .or_else(|| state.get("materialized_content_ref"))
        .or_else(|| state.get("content_ref"))
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let expected = forge_store::expected_content_ref(cwd)?;
    if expected.as_deref() == Some(content_ref) {
        return Ok(());
    }
    let current = snapshot_effective_worktree(cwd).context("snapshot replayed sync pull")?;
    if current.content_ref != content_ref {
        let context = forge_store::open_repository(cwd)?;
        if context.current_operation_id != existing.operation_id {
            return Ok(());
        }
        if expected.as_deref() != Some(current.content_ref.as_str()) {
            return Err(ForgeError::DirtyWorktree {
                paths: current.changed_paths,
            }
            .into());
        }
        restore_effective_worktree(cwd, content_ref)
            .context("restore replayed sync pull materialized content")?;
    }
    forge_store::set_materialized_expected_content_ref(cwd, content_ref)
        .context("record replayed sync pull materialized content")?;
    Ok(())
}

pub(crate) fn reassert_materialized_replay_locked(
    cwd: &Path,
    command: &str,
    existing: &forge_store::RequestIdOperation,
) -> anyhow::Result<()> {
    if existing.command != "sync pull" {
        return Ok(());
    }
    let _replay_lock = if !requires_repo_lock(command) {
        Some(forge_store::acquire_repo_lock(cwd)?)
    } else {
        None
    };
    reassert_materialized_replay(cwd, existing)
}

/// Format dropped secret-risk export paths as top-level `warnings[]` entries
/// (NER-133 U6), shared by every export egress surface so the message is uniform.
pub(crate) fn secret_export_warnings(excluded: &[String]) -> Vec<String> {
    excluded
        .iter()
        .map(|path| format!("excluded secret-risk path from export: {path}"))
        .collect()
}

pub(crate) fn command_result<F>(
    command: &'static str,
    request_id: Option<String>,
    f: F,
) -> ResponseEnvelope
where
    F: FnOnce(
        std::path::PathBuf,
        Option<String>,
    ) -> anyhow::Result<(Option<String>, Value, Vec<String>)>,
{
    let cwd = match env::current_dir().map_err(anyhow::Error::from) {
        Ok(cwd) => cwd,
        Err(error) => {
            return ResponseEnvelope::error(
                command,
                request_id,
                None,
                ErrorObject::new("COMMAND_FAILED", error.to_string()),
            )
        }
    };

    // Bring the schema to this binary's head BEFORE any per-command lock and
    // BEFORE the pre-flight replay check (NER-133 U4). `migrate` takes the repo
    // lock transiently only when a migration is pending and releases it before
    // returning, so it never nests inside the per-command lock or the `run` child
    // exec. A forward-versioned DB (HEAD+1) returns `UnknownSchemaVersion` here,
    // mapped to `SCHEMA_VERSION_UNSUPPORTED` and returned immediately — this MUST
    // short-circuit before `record_failed_operation` so the binary never writes
    // into a schema it is explicitly refusing.
    if let Err(error) = forge_store::migrate(&cwd) {
        let (error_object, retry) = error_to_object(command, &error);
        return ResponseEnvelope::error_with(command, request_id, None, error_object, retry);
    }

    // Hold the repo-level advisory write lock across the whole critical section
    // (determining reads + the write), so this command's CLI-layer reads — e.g.
    // `accept`'s `current_head`/`base_head` compare — are atomic against other
    // forge writers (NER-132). Acquired once, here; never nested. `run`, `init`,
    // and path-peer sync are excluded (see `requires_repo_lock`). Peer sync takes
    // both repository locks in canonical order inside its own critical section.
    // Remote `sync serve export` is read-only, but still takes the same lock so
    // it cannot emit a mixed DB/object-store manifest while a writer is active.
    // A contention timeout surfaces as the retryable `LOCK_TIMEOUT` code via the
    // typed `LockTimeout` downcast.
    let _repo_lock = if locks_repo_for_command(command) {
        match forge_store::acquire_repo_lock(&cwd) {
            Ok(guard) => guard,
            Err(error) => {
                let (error_object, retry) = error_to_object(command, &error);
                return ResponseEnvelope::error_with(
                    command,
                    request_id,
                    None,
                    error_object,
                    retry,
                );
            }
        }
    } else {
        None
    };

    // NER-138 slice 3: heal a torn native-history commit whose ref-store HEAD advance was
    // lost to a crash BEFORE the base anchor is read, and BEFORE the preflight-replay
    // short-circuit — so a same-`request_id` replay of a torn accept or sync merge still
    // advances HEAD. Path-peer sync commands take both repository locks inside the command
    // body; this preflight-only reconcile holds the local repo lock briefly and drops it
    // before the sync body can acquire canonical peer locks.
    if reconciles_native_head_before_replay(command) {
        let _sync_reconcile_lock = if !locks_repo_for_command(command) {
            match forge_store::acquire_repo_lock(&cwd) {
                Ok(guard) => guard,
                Err(error) => {
                    let (error_object, retry) = error_to_object(command, &error);
                    return ResponseEnvelope::error_with(
                        command,
                        request_id,
                        None,
                        error_object,
                        retry,
                    );
                }
            }
        } else {
            None
        };
        if let Err(error) = forge_store::reconcile_native_head(&cwd) {
            let (error_object, retry) = error_to_object(command, &error);
            return ResponseEnvelope::error_with(command, request_id, None, error_object, retry);
        }
    }

    // Pre-flight replay check: a sequential same-`request_id` retry replays the
    // original result without opening a write transaction. The concurrent race
    // (two retries that both pass this check before either commits) is closed by
    // the in-transaction `replay_guard` (U5), surfaced below as `RequestIdReplay`.
    if is_mutating_command(command) {
        if let Some(existing_request_id) = request_id.as_deref() {
            if let Some(existing) = forge_store::operation_for_request(&cwd, existing_request_id)
                .ok()
                .flatten()
            {
                if let Err(error) = reassert_materialized_replay_locked(&cwd, command, &existing) {
                    let (error_object, retry) = error_to_object(command, &error);
                    return ResponseEnvelope::error_with(
                        command,
                        request_id,
                        None,
                        error_object,
                        retry,
                    );
                }
                return replay_response(command, request_id, existing);
            }
        }
    }

    let warning_cwd = cwd.clone();
    let result = f(cwd, request_id.clone());

    match result {
        Ok((operation_id, data, warnings)) => {
            let mut envelope = ResponseEnvelope::success(command, request_id, operation_id, data);
            envelope.warnings = warnings;
            if is_mutating_command(command) {
                if let Some(warning) = storage_budget_warning(&warning_cwd) {
                    envelope.warnings.push(warning);
                }
            }
            envelope
        }
        Err(error) => {
            // A concurrent same-`request_id` writer won the race: the in-txn
            // `replay_guard` rolled this attempt back. Replay the committed
            // operation instead of reporting a failure (U5, option a).
            if let Some(replay) = error.downcast_ref::<forge_store::RequestIdReplay>() {
                let existing = replay.operation.clone();
                if let Err(error) =
                    reassert_materialized_replay_locked(&warning_cwd, command, &existing)
                {
                    let (error_object, retry) = error_to_object(command, &error);
                    return ResponseEnvelope::error_with(
                        command,
                        request_id,
                        None,
                        error_object,
                        retry,
                    );
                }
                return replay_response(command, request_id, existing);
            }
            let (error_object, retry) = error_to_object(command, &error);
            // Transient errors (the singleton CAS `CONFLICT`, `LOCK_TIMEOUT`) must
            // NOT be persisted under the `--request-id` — a later retry of the same
            // id should re-execute, not replay a sticky failure (R7). Deterministic
            // domain failures keep the status-aware replay contract.
            let failed_operation_id = if let Some(stale_conflict) =
                error.downcast_ref::<forge_store::StaleBaseConflict>()
            {
                env::current_dir().ok().and_then(|cwd| {
                    forge_store::record_failed_operation_with_conflict(
                        &cwd,
                        request_id.clone(),
                        command,
                        &error_object.code,
                        &error_object.message,
                        error_object.details.clone(),
                        &stale_conflict.input,
                    )
                    .ok()
                    .map(|op| op.operation_id)
                })
            } else if is_mutating_command(command) && !is_transient_error(&error) {
                env::current_dir().ok().and_then(|cwd| {
                    forge_store::record_failed_operation(
                        &cwd,
                        request_id.clone(),
                        command,
                        &error_object.code,
                        &error_object.message,
                        // Carry the typed error's details so a replay reproduces them
                        // (FIX C). `error_object.details` is the empty object for
                        // untyped failures.
                        error_object.details.clone(),
                    )
                    .ok()
                    .map(|op| op.operation_id)
                })
            } else {
                None
            };
            ResponseEnvelope::error_with(
                command,
                request_id,
                failed_operation_id,
                error_object,
                retry,
            )
        }
    }
}

pub(crate) fn storage_budget_warning(cwd: &Path) -> Option<String> {
    let status = forge_store::storage_budget_status(cwd).ok()?;
    if !status.over_budget {
        return None;
    }
    Some(format!(
        "storage budget exceeded: used_bytes={} limit_bytes={} over_by_bytes={}",
        status.used_bytes, status.limit_bytes, status.over_by_bytes
    ))
}

pub(crate) fn init_response(request_id: Option<String>, args: InitArgs) -> ResponseEnvelope {
    let content_backend = args
        .content_backend
        .or_else(|| env::var("FORGE_CONTENT_BACKEND").ok())
        .unwrap_or_else(|| "git".to_string());
    match env::current_dir()
        .map_err(anyhow::Error::from)
        .and_then(|cwd| forge_store::init_repository(&cwd, request_id.clone(), content_backend))
    {
        Ok(repository) => ResponseEnvelope::success(
            "init",
            request_id,
            Some(repository.current_operation_id.clone()),
            serde_json::to_value(repository).unwrap(),
        ),
        Err(error) => {
            // init does not route through command_result, so map its errors here.
            // A contention timeout on the U5 init lock is the retryable LOCK_TIMEOUT;
            // a genuine "not a git repo" still maps to NOT_A_GIT_REPOSITORY (init's
            // un-masked classification, preserved through the typed mapping).
            let (error_object, retry) = error_to_object("init", &error);
            ResponseEnvelope::error_with("init", request_id, None, error_object, retry)
        }
    }
}

/// Emit the static `forge.cli.v0` machine contract. Deliberately does NOT route
/// through `command_result`: the contract is static and must work without a
/// repository (no `migrate`, no lock, no cwd dependency).
pub(crate) fn schema_response(request_id: Option<String>) -> ResponseEnvelope {
    ResponseEnvelope::success("schema", request_id, None, schema::contract())
}

/// Summarize the hardened redactor's per-occurrence kinds into one `warnings[]`
/// entry per detector class with a count (NER-136 §U4), so a leak that was redacted
/// before persistence is visible to the caller without re-emitting the secret.
pub(crate) fn redaction_warnings(redactions: &[forge_content::RedactionKind]) -> Vec<String> {
    use forge_content::RedactionKind;
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for kind in redactions {
        let label = match kind {
            RedactionKind::KeyValue => "key=value secret",
            RedactionKind::HighEntropyToken => "high-entropy token",
            RedactionKind::JsonSecret => "JSON-embedded secret",
            RedactionKind::PemPrivateKey => "PEM private key",
            RedactionKind::CredentialUrl => "credential URL password",
            RedactionKind::LocalPath => "local repository path",
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(label, count)| {
            format!("redacted {count} {label}(s) from captured output before persistence")
        })
        .collect()
}

/// Resolve the acting identity for the NER-136 actor model: the `--actor` flag, else
/// the `FORGE_ACTOR` environment variable, else `"unknown"`. This is *attribution*,
/// not authentication — the string is whatever the caller declares; Phase 5 protects
/// its integrity (it is folded into the tamper-evident digest), not its authenticity.
pub(crate) fn resolve_actor(flag: Option<&str>) -> String {
    flag.map(str::to_string)
        .or_else(|| env::var("FORGE_ACTOR").ok())
        .filter(|actor| !actor.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn selected_backend(cwd: &std::path::Path) -> anyhow::Result<Box<dyn ContentBackend>> {
    match forge_store::repository_content_backend(cwd)?.as_str() {
        "git" => Ok(Box::new(forge_content_git::GitContentBackend)),
        "native" => Ok(Box::new(forge_content_native::NativeContentBackend)),
        other => anyhow::bail!("unsupported content backend {other}"),
    }
}

pub(crate) fn snapshot_effective_worktree(cwd: &Path) -> anyhow::Result<SnapshotContent> {
    snapshot_effective_worktree_excluding(cwd, &[])
}

pub(crate) fn snapshot_effective_worktree_excluding(
    cwd: &Path,
    excluded_paths: &[String],
) -> anyhow::Result<SnapshotContent> {
    let context = forge_store::open_repository(cwd)?;
    match context.content_backend.as_str() {
        "git" => {
            if !excluded_paths.is_empty() {
                anyhow::bail!(ForgeError::UnsupportedContentBackend {
                    command: "save private path".to_string(),
                    required: "native".to_string(),
                    actual: "git".to_string(),
                });
            }
            forge_content_git::GitContentBackend.snapshot_worktree(&context.root_path)
        }
        "native" => forge_content_native::snapshot_worktree_into_store_excluding(
            &context.root_path,
            &context.worktree_path,
            excluded_paths,
        ),
        other => anyhow::bail!("unsupported content backend {other}"),
    }
}

pub(crate) fn restore_effective_worktree(cwd: &Path, content_ref: &str) -> anyhow::Result<()> {
    let context = forge_store::open_repository(cwd)?;
    match classify_content_ref(content_ref) {
        ContentRefKind::GitTree(_) => {
            forge_content_git::GitContentBackend.restore_snapshot(&context.root_path, content_ref)
        }
        ContentRefKind::ForgeTree(_) => forge_content_native::restore_content_ref_to_worktree(
            &context.root_path,
            &context.worktree_path,
            content_ref,
        ),
        ContentRefKind::Unsupported => anyhow::bail!("unsupported content ref"),
    }
}

pub(crate) fn current_base(cwd: &Path) -> anyhow::Result<String> {
    let context = forge_store::open_repository(cwd)?;
    selected_backend(cwd)?.current_base(&context.root_path)
}

pub(crate) fn owner_base_content_ref(cwd: &Path, base: &str) -> anyhow::Result<String> {
    let context = forge_store::open_repository(cwd)?;
    selected_backend(cwd)?.base_content_ref(&context.root_path, base)
}

pub(crate) fn materialize_attempt_workspace(
    cwd: &Path,
    attempt_id: &str,
    content_ref: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let _worktree_lock = forge_store::acquire_worktree_lock(cwd, attempt_id)?;
    let workspace = forge_store::ensure_attempt_workspace_marker(cwd, attempt_id)?;
    match classify_content_ref(content_ref) {
        ContentRefKind::ForgeTree(_) => {
            let repo_root = forge_store::repository_root_path(cwd)?;
            forge_content_native::restore_content_ref_to_worktree(
                &repo_root,
                &workspace,
                content_ref,
            )?;
            forge_store::record_attempt_workspace_materialized(cwd, attempt_id, content_ref)?;
        }
        ContentRefKind::GitTree(_) => {}
        ContentRefKind::Unsupported => anyhow::bail!("unsupported content ref"),
    }
    Ok(workspace)
}

pub(crate) fn content_backend_label(kinds: &[ContentRefKind<'_>]) -> &'static str {
    let has_forge = kinds
        .iter()
        .any(|kind| matches!(kind, ContentRefKind::ForgeTree(_)));
    let has_git = kinds
        .iter()
        .any(|kind| matches!(kind, ContentRefKind::GitTree(_)));
    let has_unsupported = kinds
        .iter()
        .any(|kind| matches!(kind, ContentRefKind::Unsupported));
    match (has_forge, has_git, has_unsupported) {
        (true, false, false) => "native",
        (false, true, false) => "git",
        (false, false, true) => "unsupported",
        _ => "mixed",
    }
}

pub(crate) fn structured_error(
    command: impl Into<String>,
    request_id: Option<String>,
    code: impl Into<String>,
    message: impl Into<String>,
    details: Value,
) -> ResponseEnvelope {
    ResponseEnvelope::error(
        command,
        request_id,
        None,
        ErrorObject::new(code, message).with_details(details),
    )
}

pub(crate) fn print_human(response: &ResponseEnvelope) {
    if response.status == ResponseStatus::Success {
        match response.command.as_str() {
            "init" => {
                let root = response
                    .data
                    .get("root_path")
                    .and_then(Value::as_str)
                    .unwrap_or("<unknown>");
                let operation = response.operation_id.as_deref().unwrap_or("<unknown>");
                println!("Initialized Forge repository at {root}");
                println!("Current operation: {operation}");
            }
            "export pr-body" => {
                if let Some(body) = response.data.get("body").and_then(Value::as_str) {
                    print!("{body}");
                }
            }
            "schema" => {
                println!("{}", serde_json::to_string_pretty(&response.data).unwrap());
            }
            "blame" => {
                let lines = response
                    .data
                    .get("lines")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                // Commit legend (NER-362 slice 4): one line per distinct blamed
                // commit, newest first (authored_time descending; the stable sort
                // keeps first-appearance order for ties and unknown times last).
                let mut seen = std::collections::HashSet::new();
                let mut legend: Vec<(&str, Option<i64>, &str)> = Vec::new();
                for line in lines {
                    let commit = line.get("commit_id").and_then(Value::as_str).unwrap_or("");
                    if seen.insert(commit) {
                        let title = line
                            .get("intent_title")
                            .and_then(Value::as_str)
                            .unwrap_or("-");
                        let authored = line.get("authored_time").and_then(Value::as_i64);
                        legend.push((commit, authored, title));
                    }
                }
                // `f1:commit:sha256:<hex>` → the first 12 hex chars of the digest.
                let short_digest = |commit: &str| {
                    let digest = commit.rsplit(':').next().unwrap_or(commit);
                    digest[..digest.len().min(12)].to_string()
                };
                legend.sort_by(|a, b| b.1.cmp(&a.1));
                for (commit, _, title) in &legend {
                    let short = short_digest(commit);
                    println!("{short} {title}");
                }
                for line in lines {
                    let commit = line.get("commit_id").and_then(Value::as_str).unwrap_or("");
                    let short = short_digest(commit);
                    let intent = line.get("intent_id").and_then(Value::as_str).unwrap_or("-");
                    let number = line.get("line_number").and_then(Value::as_u64).unwrap_or(0);
                    let content = line.get("content").and_then(Value::as_str).unwrap_or("");
                    println!("{short} {intent} {number} {content}");
                }
            }
            command => println!("{command} succeeded"),
        }
    } else if let Some(error) = response.errors.first() {
        eprintln!("forge {} failed: {}", response.command, error.message);
    }
}

/// Map a recovered error to its agent-visible `(ErrorObject, RetryMetadata)`.
///
/// No code is string-derived: a typed [`ForgeError`] supplies its own `code`,
/// `details`, and retry classification; the standalone [`forge_store::LockTimeout`]
/// sentinel maps to the retryable `LOCK_TIMEOUT`; everything else is an
/// untyped failure — `COMMAND_FAILED`, or `NOT_A_GIT_REPOSITORY` for a genuine
/// not-a-git-repo at `init` (the only place that classification is meaningful).
pub(crate) fn error_to_object(
    command: &str,
    error: &anyhow::Error,
) -> (ErrorObject, RetryMetadata) {
    let message = error.to_string();
    if let Some(stale_conflict) = error.downcast_ref::<forge_store::StaleBaseConflict>() {
        let forge_error = stale_conflict.forge_error();
        let retry = if forge_error.retryable() {
            RetryMetadata::retryable(forge_error.after_ms())
        } else {
            RetryMetadata::no()
        };
        return (
            ErrorObject::new(forge_error.code(), message).with_details(forge_error.details()),
            retry,
        );
    }
    if let Some(forge_error) = error.downcast_ref::<ForgeError>() {
        let retry = if forge_error.retryable() {
            RetryMetadata::retryable(forge_error.after_ms())
        } else {
            RetryMetadata::no()
        };
        return (
            ErrorObject::new(forge_error.code(), message).with_details(forge_error.details()),
            retry,
        );
    }
    if let Some(lock_timeout) = error.downcast_ref::<forge_store::LockTimeout>() {
        return (
            ErrorObject::new("LOCK_TIMEOUT", message)
                .with_details(json!({ "waited_ms": lock_timeout.waited_ms })),
            RetryMetadata::retryable(Some(RETRY_BACKOFF_MS)),
        );
    }
    let code = if command == "init" {
        "NOT_A_GIT_REPOSITORY"
    } else {
        "COMMAND_FAILED"
    };
    (ErrorObject::new(code, message), RetryMetadata::no())
}

/// Whether an error must NOT be persisted under its `--request-id`, so a retry
/// re-executes instead of replaying a sticky failure (R7). True for the transient
/// CAS (`CurrentStateChanged`) and a `LockTimeout`; false for deterministic
/// domain failures, which keep the status-aware replay contract.
pub(crate) fn is_transient_error(error: &anyhow::Error) -> bool {
    if let Some(forge_error) = error.downcast_ref::<ForgeError>() {
        return forge_error.retryable();
    }
    error.downcast_ref::<forge_store::LockTimeout>().is_some()
}

pub(crate) fn is_mutating_command(command: &str) -> bool {
    matches!(
        command,
        "init"
            | "start"
            | "attempt start"
            | "attempt attach"
            | "save"
            | "restore"
            | "run"
            | "propose"
            | "check"
            | "merge"
            | "conflict resolve"
            | "accept"
            | "reject"
            | "export branch"
            | "checkout"
            | "undo"
            | "trust policy"
            | "visibility set"
            | "visibility path set"
            | "visibility grant"
            | "visibility revoke"
            | "embargo mark"
            | "embargo grant"
            | "embargo revoke"
            | "embargo release"
            | "embargo reveal"
            | "embargo publish"
            | "embargo close"
            | "key status"
            | "key rotate"
            | "org init"
            | "org encryption bind-local"
            | "gc"
            | "sync import"
            | "sync clone"
            | "sync fetch"
            | "sync pull"
            | "sync push"
            | "sync serve receive"
    )
}

/// Whether `command_result` should hold the repo-level advisory write lock across
/// this command's critical section (NER-132 U2). Excludes `run` — it executes its
/// child inside the closure and must not hold the lock (PRD §10.6) — and `init`,
/// which acquires the lock itself inside `init_repository` (it does not route
/// through `command_result`). Path-peer sync commands acquire both participating
/// repo locks in canonical root-path order inside `peer_manifests`, avoiding
/// opposite-direction lock inversion while keeping the same envelope/replay
/// behavior. The lock is acquired exactly once per command, never nested, per the
/// std file-locking re-entrancy caveat.
pub(crate) fn requires_repo_lock(command: &str) -> bool {
    is_mutating_command(command)
        && !matches!(
            command,
            "run" | "init" | "sync fetch" | "sync pull" | "sync push"
        )
}

pub(crate) fn locks_repo_for_command(command: &str) -> bool {
    requires_repo_lock(command) || matches!(command, "sync serve export" | "sync serve receive")
}

pub(crate) fn reconciles_native_head_before_replay(command: &str) -> bool {
    locks_repo_for_command(command) || matches!(command, "sync fetch" | "sync pull" | "sync push")
}
