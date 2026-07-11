use anyhow::{anyhow, bail, Context, Result};
use forge_core::{new_id, now_ms, OperationId, OperationStatus, RepositoryId, ViewId, ViewKind};
use forge_private::{EncryptedPayload, EncryptionRecipient};
use rusqlite::{
    params, Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod attempts;
mod compare;
mod conflict;
mod contract;
mod contract_run;
mod doctor;
mod embargo;
mod error;
mod evidence;
mod gc;
mod integrity;
mod intents;
mod internal;
mod merge;
mod migrations;
mod private_overlay;
mod proposals;
mod provenance;
mod publication;
mod repo_lock;
mod repository;
mod show;
mod signing;
mod snapshots;
mod storage;
mod sync;
#[cfg(test)]
mod tests;
mod trust;
mod visibility;
pub use attempts::{
    attach_attempt, attempt_base_head, attempt_materialization_ref, attempt_workspace_path,
    ensure_attempt_workspace_marker, list_attempts, record_attempt_workspace_materialized,
    resolve_attempt, show_attempt, start_attempt, start_attempt_for_intent,
    verify_attempt_workspace_undrifted, verify_save_target, AttemptRecord, AttemptShowRecord,
    AttemptSummary, ResolvedAttempt, StartAttempt, WORKSPACE_ROLE_MATERIALIZATION_TARGET,
};
pub(crate) use attempts::{
    attempt_by_id, resolve_attempt_in_context, verify_worktree_binding, WorkspaceMarker,
};
pub(crate) use compare::attempts_for_intent;
pub use compare::{
    compare_attempts, AttemptCompareRow, AttemptComparison, CompareSelector, ComparedProposal,
    IntentComparison, StructuredMetrics,
};
#[cfg(test)]
pub(crate) use compare::{
    rank_compare_rows, INTEGRITY_LEGACY, INTEGRITY_NO_EVIDENCE, INTEGRITY_TAMPERED,
    INTEGRITY_VERIFIED,
};
pub(crate) use conflict::record_merge_conflict_inner;
pub use conflict::{
    conflict_list, conflict_show, preflight_conflict_resolution, record_conflict_set,
    record_failed_operation_with_conflict, record_merge_conflict, resolve_conflict_with_tree,
    ConflictListRecord, ConflictResolutionRecord, ConflictResolutionSuggestion,
    ConflictResolutionSuggestionProvenance, ConflictSetSummary, ConflictShowRecord,
    MergeConflictInput, MergeConflictRecord, PathConflictSummary, StaleBaseConflict,
    StaleBaseConflictInput,
};
pub use contract::{
    acceptance_command_is_safe, check_acceptance_command, contract_brief, contract_revision,
    contract_run, contract_run_verdicts, contract_stops, freeze_contract_revision,
    latest_contract_revision, open_contract_stop, record_contract_run,
    record_contract_run_verdicts, record_contract_run_with_stop, resolve_contract_stop,
    AcceptanceCommandCheck, ContractBriefNeighbor, ContractBriefRecord, ContractRevisionRecord,
    ContractRunOutcome, ContractRunRecord, ContractRunTaskInput, ContractRunTaskRecord,
    ContractRunVerdictInput, ContractRunVerdictRecord, ContractStopRecord, ContractVerifyOutcome,
    FreezeContractRevisionInput, OpenContractStopInput, RecordContractRunInput,
    GLOBAL_POLICY_CONTRACT_ID, SUBJECT_KIND_CONTRACT, SUBJECT_KIND_CONTRACT_RUN,
    SUBJECT_KIND_CONTRACT_STOP, SUBJECT_KIND_CONTRACT_VERDICT,
};
pub use contract_run::{
    contract_integration_accepted, contract_integration_intent_text, contract_run_by_ref,
    open_stops_for_contracts, record_contract_integration, ContractIntegrationRecord,
    CONTRACT_INTENT_PREFIX,
};
pub use doctor::{
    doctor, DoctorReport, LedgerViewFinding, LedgerViewFindingKind, NativeHistoryFinding,
    SignatureFinding, SignatureFindingKind, SignatureKeySummary, TamperedRow,
};
pub use embargo::{
    close_embargo_workflow, ensure_embargo_publishable, finish_embargo_release_workflow,
    grant_embargo_capability, mark_embargo_workflow, prepare_embargo_release_workflow,
    publish_embargo_workflow, reveal_embargo_workflow, revoke_embargo_capability,
    EmbargoReleasePlan, EmbargoReleaseRecord, EmbargoWorkflowEventRecord, EmbargoWorkflowRecord,
    EmbargoWorkflowResult,
};
pub(crate) use embargo::{
    embargo_workflow_on, embargo_workflow_required, record_embargo_accept_on,
};
pub use error::{error_registry, ErrorCodeSpec, ForgeError, NativeHistoryCorruptKind, TamperKind};
pub use evidence::{record_evidence, EvidenceInput, EvidenceRecord, EvidenceSummary};
pub(crate) use gc::ledger_commit_roots;
pub use gc::{gc_delete, gc_dry_run, GcDryRunReport};
pub(crate) use intents::intent_detail_on;
pub use intents::{intent_detail, intents_list, IntentDetail, IntentGate, IntentSummary};
#[cfg(test)]
pub(crate) use internal::is_retryable_busy;
pub(crate) use internal::{
    ensure_work_package_exists, insert_operation_view, insert_operation_view_chained,
    op_content_hash, open_connection, replay_guard, with_immediate_retry,
    CAPABILITY_INSPECT_CONTENT, CAPABILITY_INSPECT_EVIDENCE, CAPABILITY_PUBLISH_REVEAL,
    CAPABILITY_SEE_STUB, CAPABILITY_SYNC_MATERIALIZE, DEFAULT_EMBARGO_RELEASE_CONTENT_CLASSES,
    EMBARGO_RELEASE_REVOCATION_WARNING, EMBARGO_STATE_ACCEPTED_UNDER_EMBARGO, EMBARGO_STATE_ACTIVE,
    EMBARGO_STATE_CLOSED, EMBARGO_STATE_PUBLISHED, EMBARGO_STATE_RELEASED_UNDER_EMBARGO,
    EMBARGO_STATE_REVEALED, PUBLIC_PROJECTION_FULL_SOURCE, PUBLIC_PROJECTION_PROVENANCE_ONLY,
    PUBLIC_PROJECTION_SANITIZED_SOURCE, VISIBILITY_EMBARGOED, VISIBILITY_PRIVATE,
    VISIBILITY_PUBLIC, VISIBILITY_TEAM,
};
pub use internal::{
    record_failed_operation, OperationViewInput, OperationViewResult, RequestIdReplay,
};
pub use merge::{record_merge_success, MergeSuccessInput, MergeSuccessRecord};
pub use private_overlay::{
    bind_org_encryption_key, capture_local_private_overlays,
    encrypt_private_payload_to_local_store, install_materialized_private_overlays,
    keyed_private_path_hash, local_encryption_recipient, local_private_path_exclusions,
    local_private_path_labels, prepare_materialized_private_overlay, private_decrypt_authority,
    private_overlay_transports_for_snapshots, record_encrypted_private_payload,
    record_private_path_label, scoped_private_path_hash, set_local_private_path_label,
    EncryptedPrivatePayloadInput, EncryptedPrivatePayloadRecord, LocalEncryptedPrivateObject,
    LocalPrivatePathLabel, MaterializedPrivateOverlay, OrgEncryptionKeyBindingRecord,
    PrivateDecryptAuthority, PrivateOverlayMaterializeInput, PrivateOverlayTransportRecord,
    PrivatePathLabelRecord, SaveSnapshotPrivateOverlayInput,
};
pub(crate) use private_overlay::{insert_encrypted_private_payload_on, validate_private_hash};
pub use proposals::{
    attempt_proposal_content_ref, check_spec_json_from_requires, decide, latest_decision,
    list_proposals, pr_body, pr_body_for, proposal_for_merge, proposal_review, propose,
    record_check, resolved_merge_ours_head, verify_decision_integrity, CheckRecord, CheckSummary,
    DecisionRecord, ProposalMetadata, ProposalRecord, ProposalReview, ProposalSummary,
    ResolvedProposal, ReviewAttemptContext, ReviewChangedPath, ReviewDiff, ReviewEmbargo,
    ReviewEvidenceAudit, ReviewFactor, ReviewLifecycle, ReviewReadiness, ReviewTerminalHandoff,
    ReviewVisibility,
};
pub(crate) use proposals::{
    decision_high_water, evaluate_check_on, evidence_facts_on, evidence_high_water,
    intent_check_spec, latest_check_for_attempt, latest_decision_for_attempt,
    latest_decision_for_proposal_revision, latest_evidence_for_attempt,
    latest_proposal_for_attempt, proposal_by_id, proposal_by_id_on, proposal_metadata_for_attempt,
    redact_gate_result, resolve_proposal, verify_evidence_integrity, IntegrityStatus,
};
pub use provenance::{provenance_detail, ProvenanceDetail};
pub(crate) use publication::latest_publication_for_proposal_revision;
pub use publication::{
    accepted_commit_id_for_revision, build_publication_trailer, decision_for_proposal_revision,
    exportable_proposal, publication_exists_for_branch, record_publication, render_trailer_message,
    PublicationRecord, PublicationTrailer,
};
pub use repo_lock::{LockTimeout, RepoLock};
pub use repository::{
    acquire_repo_lock, acquire_repository_lock, acquire_worktree_lock, effective_worktree_path,
    init_repository, migrate, open_repository, operation_for_request, repository_content_backend,
    repository_root_path, signing_key_fingerprint_for_public_key, InitRepository,
    RepositoryContext, RequestIdOperation,
};
pub use show::{show, ShowRecord};
pub use snapshots::{
    checkout_target_content_ref, classify_missing_commit, expected_content_ref,
    latest_snapshot_content_ref, native_history_tip, native_log, reconcile_native_head,
    record_checkout, record_restore, record_undo, save_snapshot,
    save_snapshot_with_private_overlays, set_materialized_expected_content_ref,
    snapshot_content_ref, snapshot_owner_attempt_id, undo_target, CommitView, SnapshotRecord,
    SnapshotSummary, UndoTarget,
};
pub(crate) use snapshots::{
    latest_snapshot_for_attempt, latest_snapshot_on, native_tip, set_context_expected_content_ref,
    NativeVisitState,
};
pub use storage::{
    storage_accounting, storage_budget_status, StorageAccounting, StorageBudgetStatus,
    StorageCategoryAccounting, StoragePolicy,
};
pub use sync::{
    is_sync_merged_op_kind, prepare_native_sync_clone, record_projected_sync_clone_initialized,
    record_sync_import_materialized, record_sync_merge_commit, record_sync_merge_conflict,
    record_sync_pull_materialized, record_sync_request_marker, set_sync_clone_expected_content_ref,
    SyncCloneRepository, SyncMergeCommitInput, SyncMergeCommitResult, SyncPullMaterializedInput,
};
pub use trust::{
    attest_hosted_runner, attest_third_party, enforce_trust_policy, init_org_governance,
    local_key_status, org_status, rotate_local_key, set_trust_policy, trust_policy,
    HostedRunnerAttestation, LocalKeyRotation, LocalKeyStatus, OrgBootstrap, OrgStatus,
    ThirdPartyAttestation, TrustPolicy, TrustPolicyAction,
};
pub(crate) use trust::{
    embargo_authority_on, ensure_active_org_principal, ensure_active_org_role, org_status_on,
    trust_policy_on, trust_rank, TRUST_LOCALLY_SIGNED,
};
pub(crate) use visibility::{
    effective_work_package_visibility_on, has_active_visibility_grant, insert_visibility_audit,
    insert_work_package_visibility, projection_decision_on, upsert_work_package_visibility,
    validate_public_projection_mode, validate_visibility_capability, validate_visibility_label,
    validate_work_package_kind, visibility_policy_on,
};
pub use visibility::{
    grant_visibility_capability, projection_decision, revoke_visibility_capability,
    set_work_package_visibility, visibility_policy, ProjectionDecision, VisibilityAuditRecord,
    VisibilityGrantRecord, VisibilityPolicy, WorkPackageVisibilityRecord,
};
