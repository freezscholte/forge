use clap::{Args, Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

use crate::review;

#[derive(Debug, Parser)]
#[command(name = "forge", version, about = "Local agent change-control loop")]
pub(crate) struct Cli {
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[arg(long, global = true)]
    pub(crate) request_id: Option<String>,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    Init(InitArgs),
    Start(IntentArgs),
    Attempt(AttemptArgs),
    /// List intents or show one intent's declared gate spec + linked attempts.
    Intent(IntentCommandArgs),
    Save(AttemptScopedArgs),
    Restore(RestoreArgs),
    Run(RunArgs),
    Propose(ProposeArgs),
    Check(ProposalScopedArgs),
    Accept(AcceptArgs),
    Reject(ProposalScopedArgs),
    Show(AttemptScopedArgs),
    Proposal(ProposalArgs),
    /// Build a read-only local review surface for one proposal.
    Review(review::ReviewArgs),
    /// Compare competing attempts (per intent) on verified evidence + rank them.
    Compare(CompareArgs),
    /// Diff native or git content refs, or the working tree against a native snapshot.
    Diff(DiffArgs),
    /// Merge a proposal against the current native head.
    Merge(MergeArgs),
    /// Inspect persisted conflict-as-data records.
    Conflict(ConflictArgs),
    /// Walk the native commit history (tip→genesis) and the evidence that justified it.
    Log(LogArgs),
    /// Attribute each line of a committed file to the native commit that last changed it.
    Blame(BlameArgs),
    /// Materialize a past commit's tree into the worktree (does not move the base anchor).
    Checkout(CheckoutArgs),
    /// Undo the last save, restoring the prior snapshot (recorded in the op-log).
    Undo,
    /// Inspect or update local trust policy gates.
    Trust(TrustArgs),
    /// Inspect or update local visibility policy and work-package grants.
    Visibility(VisibilityArgs),
    /// Manage embargoed security-fix workflow state and controlled release.
    Embargo(EmbargoArgs),
    /// Lint and freeze `ccx.contract.v1` task contracts into signed ledger revisions.
    Contract(ContractArgs),
    /// Inspect or rotate the local Ed25519 signing key.
    Key(KeyArgs),
    /// Inspect or initialize local org governance.
    Org(OrgArgs),
    Doctor,
    Gc(GcArgs),
    /// Export or inspect a Forge-native sync protocol bundle manifest.
    Sync(SyncArgs),
    Export(ExportArgs),
    /// Emit the versioned machine contract (schema_version, command + error registry).
    Schema,
}

#[derive(Debug, Args)]
pub(crate) struct CompareArgs {
    /// Compare attempts under this intent only. Omit to compare every intent that has
    /// an attempt (each as its own ranked group).
    #[arg(long)]
    pub(crate) intent: Option<String>,
    /// Compare attempts under this attempt's intent.
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    /// Two attempt ids to additionally produce a file/hunk content diff between their
    /// proposals: `--diff <attempt_a> <attempt_b>`.
    #[arg(long, num_args = 2, value_names = ["ATTEMPT_A", "ATTEMPT_B"])]
    pub(crate) diff: Option<Vec<String>>,
}

#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// Content ref for the old side (`forge-tree:...` or `git-tree:...`). Omit with --working.
    #[arg(long)]
    pub(crate) from: Option<String>,
    /// Content ref for the new/base side (`forge-tree:...` or `git-tree:...`).
    #[arg(long)]
    pub(crate) to: String,
    /// Diff the current working tree against --to.
    #[arg(long)]
    pub(crate) working: bool,
    /// Enable rename detection, optionally overriding the similarity threshold (default 50).
    #[arg(long, num_args = 0..=1, default_missing_value = "50")]
    pub(crate) find_renames: Option<u8>,
    /// Disable rename detection.
    #[arg(long)]
    pub(crate) no_renames: bool,
}

#[derive(Debug, Args)]
pub(crate) struct MergeArgs {
    /// Proposal id whose base/theirs tree should merge with the current repo head.
    #[arg(long)]
    pub(crate) proposal: String,
}

#[derive(Debug, Args)]
pub(crate) struct ConflictArgs {
    #[command(subcommand)]
    pub(crate) command: ConflictCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConflictCommand {
    List,
    Show {
        conflict_set_id: String,
        /// Emit gated, ranked native resolution suggestions. Suggestions are advisory only;
        /// use `conflict resolve --tree <ref>` to apply one explicitly.
        #[arg(long)]
        suggest: bool,
    },
    Resolve {
        conflict_set_id: String,
        #[arg(long)]
        tree: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    /// Show only commits recorded under this intent ("show every change under this intent").
    #[arg(long)]
    pub(crate) intent: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct BlameArgs {
    /// Repo-relative path of the committed file whose lines to attribute.
    pub(crate) path: String,
    /// Attribute at this native commit id (`f1:commit:sha256:...`) instead of the ledger tip.
    #[arg(long)]
    pub(crate) at: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct CheckoutArgs {
    /// The native commit id (`f1:commit:sha256:...`) whose tree to materialize.
    pub(crate) commit_id: String,
}

#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    #[arg(long, value_parser = ["git", "native"])]
    pub(crate) content_backend: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct IntentArgs {
    pub(crate) intent: Option<String>,
    /// A required check gate, given as the command that must pass on the proposed
    /// snapshot (e.g. --require "cargo test"). Repeatable; all gates must pass for
    /// `check` to be green and `accept` to proceed (NER-135). The value is
    /// whitespace-tokenized into program + args.
    #[arg(long)]
    pub(crate) require: Vec<String>,
    /// A structured required gate (NER-136): like --require, but the command's parsed
    /// outcome must also report zero failures (e.g. --require-tests-pass "cargo test"
    /// fails the gate if the parsed test-failure count is non-zero, even on exit 0).
    #[arg(long)]
    pub(crate) require_tests_pass: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AttemptScopedArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ProposeArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    /// Optional human summary echoed in the proposal response for agent workflows.
    #[arg(long)]
    pub(crate) summary: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ProposalScopedArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    #[arg(long)]
    pub(crate) proposal: Option<String>,
    /// Who is making this decision (NER-136 actor model). Falls back to `FORGE_ACTOR`,
    /// then `"unknown"`.
    #[arg(long)]
    pub(crate) actor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AcceptArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    #[arg(long)]
    pub(crate) proposal: Option<String>,
    /// Accept even when the proposal's check is not passing (NER-135). Default is to
    /// require a passing check; this bypass emits a warnings[] entry. NOTE: this is a
    /// policy bypass only — it never bypasses an `EVIDENCE_TAMPERED` integrity failure.
    #[arg(long)]
    pub(crate) allow_unverified: bool,
    /// Who is accepting (NER-136 actor model). Falls back to `FORGE_ACTOR`, then
    /// `"unknown"`.
    #[arg(long)]
    pub(crate) actor: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct AttemptArgs {
    #[command(subcommand)]
    pub(crate) command: AttemptCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AttemptCommand {
    /// Start a new attempt for an existing intent.
    ///
    /// The reported workspace_path (.forge/worktrees/<attempt-id>) is
    /// materialized state, not an editing surface; the repo root worktree
    /// (after `attempt attach`) is where edits belong.
    Start(AttemptStartArgs),
    List,
    Show {
        attempt_id: String,
    },
    /// Attach the repo root worktree to an attempt and materialize its tree there.
    ///
    /// The attempt's .forge/worktrees/<attempt-id> workspace dir is
    /// materialized state, not an editing surface; the repo root worktree
    /// (after attach) is where edits belong.
    Attach {
        attempt_id: String,
        /// Proceed even when the attempt's workspace dir (.forge/worktrees/<attempt>)
        /// has drifted from its recorded materialized content, discarding the drifted
        /// workspace edits (NER-382). Without this flag such an attach refuses with
        /// WORKSPACE_DRIFT. Discards workspace-dir drift ONLY — it never bypasses the
        /// repo-root DIRTY_WORKTREE or ATTEMPT_WORKTREE_MISMATCH protections.
        #[arg(long)]
        discard_workspace_changes: bool,
    },
    /// Compare competing attempts (per intent) on verified evidence + rank them.
    Compare(CompareArgs),
}

#[derive(Debug, Args)]
pub(crate) struct IntentCommandArgs {
    #[command(subcommand)]
    pub(crate) command: IntentCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum IntentCommand {
    /// List every intent with its title, derived status, gate spec, and attempt ids.
    List,
    /// Show one intent's title/text, derived status, declared gate spec, and attempt ids.
    Show { intent_id: String },
}

#[derive(Debug, Args)]
pub(crate) struct AttemptStartArgs {
    #[arg(long)]
    pub(crate) intent: String,
}

#[derive(Debug, Args)]
pub(crate) struct ProposalArgs {
    #[command(subcommand)]
    pub(crate) command: ProposalCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ProposalCommand {
    List(AttemptScopedArgs),
}

#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    pub(crate) snapshot_id: String,
    #[arg(long)]
    pub(crate) yes: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    /// Who is running this command (NER-136 actor model). Falls back to the
    /// `FORGE_ACTOR` env var, then `"unknown"`. Attribution, not authentication.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    #[arg(long, default_value_t = forge_evidence::DEFAULT_TIMEOUT_MS)]
    pub(crate) timeout_ms: u64,
    #[arg(last = true)]
    pub(crate) command: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct GcArgs {
    #[arg(long)]
    pub(crate) dry_run: bool,
    #[arg(long)]
    pub(crate) yes: bool,
    #[arg(long)]
    pub(crate) plan_digest: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    #[command(subcommand)]
    pub(crate) command: ExportCommand,
}

#[derive(Debug, Args)]
pub(crate) struct SyncArgs {
    #[command(subcommand)]
    pub(crate) command: SyncCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SyncCommand {
    /// Export a versioned v1 sync manifest for this repository.
    Export(SyncExportArgs),
    /// Inspect a previously exported sync manifest.
    Inspect(SyncInspectArgs),
    /// Import a previously exported native sync bundle into this repository.
    Import(SyncImportArgs),
    /// Clone a native sync bundle into an empty directory and materialize its HEAD.
    Clone(SyncCloneArgs),
    /// Fetch a fast-forward native delta from another local Forge repository.
    Fetch(SyncPeerArgs),
    /// Fetch and materialize a fast-forward native delta from another local Forge repository.
    Pull(SyncPeerArgs),
    /// Push a fast-forward native delta into another local Forge repository.
    Push(SyncPeerArgs),
    /// Internal transport endpoint used by remote peers.
    #[command(hide = true)]
    Serve(SyncServeArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SyncExportArgs {
    #[arg(long)]
    pub(crate) output: std::path::PathBuf,
    /// Emit only native objects and ledger rows absent from this prior bundle.
    #[arg(long)]
    pub(crate) since: Option<std::path::PathBuf>,
    /// Export a recipient-scoped projected manifest instead of a full manifest.
    #[arg(long)]
    pub(crate) recipient: Option<String>,
    /// Projection capability for recipient-scoped exports.
    #[arg(long, default_value = "sync_materialize")]
    pub(crate) capability: String,
}

#[derive(Debug, Args)]
pub(crate) struct SyncInspectArgs {
    pub(crate) path: std::path::PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct SyncImportArgs {
    pub(crate) path: std::path::PathBuf,
    /// Restore the imported native HEAD tree into the current worktree after applying the bundle.
    #[arg(long)]
    pub(crate) materialize: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SyncCloneArgs {
    pub(crate) path: std::path::PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct SyncPeerArgs {
    /// Peer repository locator. Supports a local path today; file:// URLs are
    /// accepted as URL-shaped local remotes so later ssh/https transport can
    /// extend the same argument without changing the command surface.
    pub(crate) remote: OsString,
}

#[derive(Debug, Args)]
pub(crate) struct SyncServeArgs {
    #[command(subcommand)]
    pub(crate) command: SyncServeCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SyncServeCommand {
    /// Export a transport manifest through the normal JSON envelope.
    Export(SyncServeExportArgs),
    /// Receive a pushed transport manifest through the normal JSON envelope.
    Receive(SyncServeReceiveArgs),
}

#[derive(Debug, Args)]
pub(crate) struct SyncServeExportArgs {
    /// Read the incremental base manifest from stdin.
    #[arg(long)]
    pub(crate) stdin_since: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SyncServeReceiveArgs {
    /// Read the pushed manifest from stdin.
    #[arg(long)]
    pub(crate) stdin_manifest: bool,
    /// Source label recorded in remote sync metadata.
    #[arg(long)]
    pub(crate) remote_label: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ExportCommand {
    Branch(ExportBranchArgs),
    PrBody(ProposalScopedArgs),
    /// Verify a published branch's provenance trailer recomputes from the local ledger.
    VerifyBranch(VerifyBranchArgs),
}

#[derive(Debug, Args)]
pub(crate) struct VerifyBranchArgs {
    pub(crate) name: String,
}

#[derive(Debug, Args)]
pub(crate) struct ExportBranchArgs {
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    #[arg(long)]
    pub(crate) proposal: Option<String>,
    /// Who is publishing (NER-136 actor model). Falls back to `FORGE_ACTOR`, then
    /// `"unknown"`.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    pub(crate) name: String,
}

#[derive(Debug, Args)]
pub(crate) struct TrustArgs {
    #[command(subcommand)]
    pub(crate) command: TrustCommand,
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityArgs {
    #[command(subcommand)]
    pub(crate) command: VisibilityCommand,
}

#[derive(Debug, Args)]
pub(crate) struct EmbargoArgs {
    #[command(subcommand)]
    pub(crate) command: EmbargoCommand,
}

#[derive(Debug, Args)]
pub(crate) struct ContractArgs {
    #[command(subcommand)]
    pub(crate) command: ContractCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ContractCommand {
    /// Lint a `ccx.contract.v1` YAML file against the six rule families (read-only).
    Lint(ContractLintArgs),
    /// Lint then record a signed, immutable frozen revision of a contract YAML file.
    Freeze(ContractFreezeArgs),
    /// Emit the byte-stable brief for a frozen contract revision (read-only).
    Brief(ContractBriefArgs),
    /// Run the agent against a frozen contract (or dependency-ordered chain) in a
    /// native scratch workspace, recording the run and halting on `UNKNOWN.md`.
    Run(ContractRunArgs),
    /// Re-apply a completed run's patch onto the current HEAD as a linked attempt.
    Integrate(ContractIntegrateArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ContractRunArgs {
    /// The frozen contract id(s) to run. Multiple ids require `--chain`; they are
    /// topologically ordered by `depends_on` before execution.
    #[arg(required = true)]
    pub(crate) contract_ids: Vec<String>,
    /// Permit multiple contract ids (a dependency-ordered chain).
    #[arg(long)]
    pub(crate) chain: bool,
    /// The opaque agent command executed once per task via `sh -c` in the scratch
    /// workspace. Explicit-flag-only in v1: there is NO repo-config fallback (a
    /// repo-shipped command source is a supply-chain surface).
    #[arg(long)]
    pub(crate) agent_cmd: String,
    /// Per-id acknowledgement of an out-of-chain dependency: `--dep <id>=<run-or-task-id>`.
    /// Each out-of-chain `depends_on` id must be named exactly once (R20).
    #[arg(long = "dep")]
    pub(crate) dep: Vec<String>,
    /// Resume a prior halted run (by run id or a completed task id) from its halted
    /// task, replaying the recorded completed-task outputs instead of re-executing
    /// their agents (KTD9). Resume is always explicit: WITHOUT this flag every run
    /// is a fresh full-chain run, even when a resumable halted run exists
    /// (least-surprise default). Refuses with CONTRACT_NOT_INTEGRABLE when the
    /// recorded state no longer applies (base moved, patch object gone).
    #[arg(long)]
    pub(crate) resume: Option<String>,
    /// Force a fresh full-chain run, ignoring `--resume` if both are given.
    /// Redundant on its own (fresh is already the default without `--resume`).
    #[arg(long)]
    pub(crate) fresh: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ContractIntegrateArgs {
    /// A completed run id or a completed task id to integrate onto HEAD.
    pub(crate) target: String,
}

#[derive(Debug, Args)]
pub(crate) struct ContractLintArgs {
    /// Path to the contract YAML file (or a `_global-policy.yaml`). Canonicalized at the boundary.
    pub(crate) path: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct ContractFreezeArgs {
    /// Path to the contract YAML file (or a `_global-policy.yaml`). Canonicalized at the boundary.
    pub(crate) path: PathBuf,
}

#[derive(Debug, Args)]
pub(crate) struct ContractBriefArgs {
    /// The frozen contract id to brief (for example `ccx-demo`).
    pub(crate) contract_id: String,
    /// Emit a specific frozen revision instead of the latest.
    #[arg(long)]
    pub(crate) revision: Option<i64>,
    /// Write the brief to this path instead of stdout. Canonicalized at the boundary.
    #[arg(long)]
    pub(crate) out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct KeyArgs {
    #[command(subcommand)]
    pub(crate) command: KeyCommand,
}

#[derive(Debug, Args)]
pub(crate) struct OrgArgs {
    #[command(subcommand)]
    pub(crate) command: OrgCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum KeyCommand {
    /// Show the current local signing key fingerprint.
    Status,
    /// Rotate the current local signing key, preserving old public keys in signatures.
    Rotate,
}

#[derive(Debug, Subcommand)]
pub(crate) enum OrgCommand {
    /// Show the current org governance profile.
    Status,
    /// Enable org governance and bind the first owner to the local signing key.
    Init(OrgInitArgs),
    /// Bind or inspect local org encryption keys.
    Encryption(OrgEncryptionArgs),
    /// Check whether a principal can decrypt private content for a work package.
    DecryptAuthority(OrgDecryptAuthorityArgs),
}

#[derive(Debug, Args)]
pub(crate) struct OrgInitArgs {
    /// Human-readable actor alias for the bootstrap owner.
    #[arg(long)]
    pub(crate) actor: String,
    /// Optional audit reason for enabling org governance.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct OrgEncryptionArgs {
    #[command(subcommand)]
    pub(crate) command: OrgEncryptionCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum OrgEncryptionCommand {
    /// Bind this machine's local age recipient to an org principal.
    BindLocal(OrgEncryptionBindLocalArgs),
}

#[derive(Debug, Args)]
pub(crate) struct OrgEncryptionBindLocalArgs {
    /// Org principal id that owns this local encryption recipient.
    #[arg(long)]
    pub(crate) principal_id: String,
    /// Authority principal id. Defaults to --principal-id.
    #[arg(long)]
    pub(crate) authority_id: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct OrgDecryptAuthorityArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Org principal id that should have decrypt authority.
    #[arg(long)]
    pub(crate) principal_id: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TrustCommand {
    /// Show or update the local minimum trust policy.
    Policy(TrustPolicyArgs),
    /// Record a hosted-runner attestation for a proposal's current evidence.
    Attest(TrustAttestArgs),
}

#[derive(Debug, Args)]
pub(crate) struct TrustAttestArgs {
    #[command(subcommand)]
    pub(crate) command: TrustAttestCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TrustAttestCommand {
    /// Sign the proposal's evidence subjects with a hosted-runner key.
    HostedRunner(HostedRunnerAttestArgs),
    /// Sign the proposal's evidence subjects with a third-party issuer key.
    ThirdParty(ThirdPartyAttestArgs),
}

#[derive(Debug, Args)]
pub(crate) struct HostedRunnerAttestArgs {
    /// Scope attestation to this attempt. Omit when one attempt is active.
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    /// Scope attestation to this proposal. Omit when the attempt has one proposal.
    #[arg(long)]
    pub(crate) proposal: Option<String>,
    /// Ed25519 PKCS#8 private key used by the hosted runner.
    #[arg(long)]
    pub(crate) key: PathBuf,
    /// Human-readable hosted runner issuer, e.g. a CI workflow or runner pool.
    #[arg(long, default_value = "hosted-runner")]
    pub(crate) issuer: String,
}

#[derive(Debug, Args)]
pub(crate) struct ThirdPartyAttestArgs {
    /// Scope attestation to this attempt. Omit when one attempt is active.
    #[arg(long)]
    pub(crate) attempt: Option<String>,
    /// Scope attestation to this proposal. Omit when the attempt has one proposal.
    #[arg(long)]
    pub(crate) proposal: Option<String>,
    /// Ed25519 PKCS#8 private key used by the third-party issuer.
    #[arg(long)]
    pub(crate) key: PathBuf,
    /// Human-readable third-party issuer, e.g. an external transparency log or CA.
    #[arg(long, default_value = "third-party")]
    pub(crate) issuer: String,
}

#[derive(Debug, Args)]
pub(crate) struct TrustPolicyArgs {
    /// Minimum trust required for `forge accept`.
    #[arg(long, value_parser = [
        "self_reported",
        "locally_observed",
        "locally_signed",
        "hosted_runner_observed",
        "hosted_runner_signed",
        "third_party_attested",
    ])]
    pub(crate) accept: Option<String>,
    /// Minimum trust required for `forge export branch`.
    #[arg(long, value_parser = [
        "self_reported",
        "locally_observed",
        "locally_signed",
        "hosted_runner_observed",
        "hosted_runner_signed",
        "third_party_attested",
    ])]
    pub(crate) export: Option<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum VisibilityCommand {
    /// Show the default work-package visibility policy.
    Policy,
    /// Set one work package's visibility label.
    Set(VisibilitySetArgs),
    /// Grant one capability to a recipient for a work package.
    Grant(VisibilityGrantArgs),
    /// Revoke one capability from a recipient for a work package.
    Revoke(VisibilityGrantArgs),
    /// Check a recipient/capability projection decision for a work package.
    Check(VisibilityCheckArgs),
    /// Manage exact private path labels for a work package.
    Path(VisibilityPathArgs),
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityWorkPackageArgs {
    /// Work-package kind: intent, attempt, or proposal.
    #[arg(long, value_parser = ["intent", "attempt", "proposal"])]
    pub(crate) kind: String,
    /// Work-package id for the selected kind.
    #[arg(long)]
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct VisibilitySetArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Visibility label: private, team, public, or embargoed.
    #[arg(long, value_parser = ["private", "team", "public", "embargoed"])]
    pub(crate) visibility: String,
    /// Actor recorded in visibility audit.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityGrantArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Recipient identifier for this local v1 grant.
    #[arg(long)]
    pub(crate) recipient: String,
    /// Capability: see_stub, inspect_content, inspect_evidence, sync_materialize, or publish_reveal.
    #[arg(long, value_parser = [
        "see_stub",
        "inspect_content",
        "inspect_evidence",
        "sync_materialize",
        "publish_reveal",
    ])]
    pub(crate) capability: String,
    /// Actor recorded in visibility audit.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityCheckArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Recipient identifier for this local v1 projection decision.
    #[arg(long)]
    pub(crate) recipient: String,
    /// Capability: see_stub, inspect_content, inspect_evidence, sync_materialize, or publish_reveal.
    #[arg(long, value_parser = [
        "see_stub",
        "inspect_content",
        "inspect_evidence",
        "sync_materialize",
        "publish_reveal",
    ])]
    pub(crate) capability: String,
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityPathArgs {
    #[command(subcommand)]
    pub(crate) command: VisibilityPathCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum VisibilityPathCommand {
    /// Set one exact private path label for a work package.
    Set(VisibilityPathSetArgs),
}

#[derive(Debug, Args)]
pub(crate) struct VisibilityPathSetArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Exact repo-relative private path. Globs and parent traversal are rejected.
    #[arg(long)]
    pub(crate) path: String,
    /// Visibility label for this path: private, team, or embargoed. Public is rejected for private path labels.
    #[arg(long, value_parser = ["private", "team", "public", "embargoed"], default_value = "private")]
    pub(crate) visibility: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum EmbargoCommand {
    /// Mark a work package as governed by the embargo workflow.
    Mark(EmbargoActorArgs),
    /// Grant an embargo-scoped visibility capability.
    Grant(EmbargoGrantArgs),
    /// Revoke an embargo-scoped visibility capability.
    Revoke(EmbargoGrantArgs),
    /// Authorize and export a recipient-scoped embargo release bundle.
    Release(EmbargoReleaseArgs),
    /// Reveal a public-safe projection after accepted/released embargo state.
    Reveal(EmbargoRevealArgs),
    /// Mark a revealed embargo workflow as published.
    Publish(EmbargoActorArgs),
    /// Close an active embargo workflow without publication.
    Close(EmbargoActorArgs),
}

#[derive(Debug, Args)]
pub(crate) struct EmbargoActorArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Actor recorded in the embargo event stream.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct EmbargoGrantArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Recipient identifier for this local v1 grant.
    #[arg(long)]
    pub(crate) recipient: String,
    /// Capability: see_stub, inspect_content, inspect_evidence, sync_materialize, or publish_reveal.
    #[arg(long, value_parser = [
        "see_stub",
        "inspect_content",
        "inspect_evidence",
        "sync_materialize",
        "publish_reveal",
    ])]
    pub(crate) capability: String,
    /// Actor recorded in the embargo event stream.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct EmbargoReleaseArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Recipient identifier that must hold sync_materialize.
    #[arg(long)]
    pub(crate) recipient: String,
    /// Output path for the embargo-release sync bundle.
    #[arg(long)]
    pub(crate) output: PathBuf,
    /// Content class included in the release envelope. Repeatable.
    #[arg(long = "content-class")]
    pub(crate) content_classes: Vec<String>,
    /// Actor recorded in the embargo event stream.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct EmbargoRevealArgs {
    #[command(flatten)]
    pub(crate) work_package: VisibilityWorkPackageArgs,
    /// Public projection mode for the eventual publication boundary.
    #[arg(long, value_parser = ["provenance-only", "sanitized-source", "full-source"])]
    pub(crate) mode: String,
    /// Public-safe actor reference, distinct from private actor identity.
    #[arg(long)]
    pub(crate) public_actor_ref: Option<String>,
    /// Actor recorded in the embargo event stream.
    #[arg(long)]
    pub(crate) actor: Option<String>,
    /// Optional audit reason.
    #[arg(long)]
    pub(crate) reason: Option<String>,
}
