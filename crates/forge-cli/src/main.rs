mod args;
mod commands;
mod review;
mod schema;

use clap::{error::ErrorKind, Parser};
use forge_protocol::ResponseStatus;
use serde_json::json;
use std::env;
use std::process::ExitCode;

pub(crate) use args::*;
use commands::core::*;
pub(crate) use commands::core::{
    command_result, current_base, ensure_clean_for_sync_import_materialize, ensure_clean_worktree,
    error_to_object, owner_base_content_ref, resolve_actor, restore_effective_worktree,
    secret_export_warnings, sync_manifest_head_content_ref,
};
pub(crate) use forge_store::ForgeError;

fn main() -> ExitCode {
    let raw_args: Vec<String> = env::args().collect();
    let json_mode = raw_args.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return ExitCode::SUCCESS;
        }
        Err(error) if json_mode => {
            let response = parser_error_response(&raw_args, error);
            println!("{}", serde_json::to_string_pretty(&response).unwrap());
            return ExitCode::from(2);
        }
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(2);
        }
    };
    let request_id = cli.request_id.clone();
    let response = match cli.command {
        Command::Init(args) => init_response(request_id, args),
        Command::Start(args) => start_response(request_id, args),
        Command::Attempt(args) => attempt_response(request_id, args),
        Command::Intent(args) => intent_response(request_id, args),
        Command::Save(args) => save_response(request_id, args),
        Command::Restore(args) if !args.yes => structured_error(
            "restore",
            request_id,
            "CONFIRMATION_REQUIRED",
            "restore requires --yes",
            json!({ "snapshot_id": args.snapshot_id }),
        ),
        Command::Restore(args) => restore_response(request_id, args),
        Command::Run(args) => run_response(request_id, args),
        Command::Propose(args) => propose_response(request_id, args),
        Command::Check(args) => check_response(request_id, args),
        Command::Accept(args) => accept_response(request_id, args),
        Command::Reject(args) => reject_response(request_id, args),
        Command::Show(args) => show_response(request_id, args),
        Command::Proposal(args) => proposal_response(request_id, args),
        Command::Review(args) => review::review_response(request_id, args),
        Command::Compare(args) => compare_response(request_id, "compare", args),
        Command::Diff(args) => diff_response(request_id, args),
        Command::Merge(args) => merge_response(request_id, args),
        Command::Conflict(args) => conflict_response(request_id, args),
        Command::Log(args) => log_response(request_id, args),
        Command::Blame(args) => blame_response(request_id, args),
        Command::Checkout(args) => checkout_response(request_id, args),
        Command::Undo => undo_response(request_id),
        Command::Trust(args) => trust_response(request_id, args),
        Command::Visibility(args) => visibility_response(request_id, args),
        Command::Embargo(args) => embargo_response(request_id, args),
        Command::Contract(args) => commands::contract::contract_response(request_id, args),
        Command::Key(args) => key_response(request_id, args),
        Command::Org(args) => org_response(request_id, args),
        Command::Doctor => doctor_response(request_id),
        Command::Gc(args) if !args.dry_run && (!args.yes || args.plan_digest.is_none()) => {
            structured_error(
                "gc",
                request_id,
                "CONFIRMATION_REQUIRED",
                "gc deletion requires --yes and --plan-digest from a prior dry-run",
                json!({}),
            )
        }
        Command::Gc(args) => gc_response(request_id, args),
        Command::Sync(args) => commands::sync::sync_response(request_id, args),
        Command::Export(args) => commands::export::export_response(request_id, args),
        Command::Schema => schema_response(request_id),
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response).unwrap());
    } else {
        print_human(&response);
    }

    if response.status == ResponseStatus::Success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
