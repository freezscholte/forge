# Phase 9 Release Audit

Date: 2026-07-05
Audited candidate: `main` at `d480e94` after PR #122 merged. The final
immutable audited commit is the `v0.1.0-rc10` tag after release-prep docs are
committed and tagged.

This document maps the Phase 9 roadmap exit criteria to current executable
evidence. It is intentionally stricter than a status note: an item is marked
proven only when a named test, script, or CI gate exercises the requirement.

## Verification Gate

The release gate is:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

The gate script also runs without `rtk` when `rtk` is not installed, which keeps
the public release check usable from a plain shell:

```bash
bash scripts/dogfood-release-gate.sh
```

Latest local run while preparing `v0.1.0-rc10` passed:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`: 604 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 5.9.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

PR #82 merged the public release metadata cleanup before the original audit
refresh. PR #88 added public issue templates, PR #94 addressed the first
Forge CLI dogfood feedback, PR #95 fixed macOS `/private/var` path-alias
redaction before the rc3 audit refresh, PR #97 fixed the second dogfood
feedback pass before the rc4 audit refresh, and PR #99 added permissioned sync
projections before the rc5 audit refresh. PR #101 adds the first organization
identity and key-governance bootstrap slice before the rc6 audit refresh. PR
#103 adds encrypted private content overlays before the rc7 audit refresh. PR
#105 adds embargoed security-fix workflows before the rc8 audit refresh. PR
#106 adds the local read-only proposal review surface before the rc9 audit
refresh. PR #122 adds native/Git drift warnings before this rc10 audit refresh.

## External Dogfood Validation

After publishing `v0.1.0-rc5`, the release candidate was installed from the
published Git tag at `ba241050` into an isolated PATH and run against the
`forge-dogfood` checkout.

Baseline app checks:

- `npm run typecheck`: passed
- `npm test`: passed
- `npm run build`: passed
- `npm run lint`: failed before the dogfood fix because ESLint scanned
  Forge-managed attempt worktrees and the TypeScript parser saw multiple
  candidate tsconfig roots

The lint/tooling friction was then fixed through the rc5 Forge lifecycle:

- `forge --version`: `forge 0.1.0`
- `forge schema --json`: passed
- `forge doctor --json`: passed on schema version 18
- `forge start --json --require "npm run typecheck" --require "npm test" --require "npm run build" --require "npm run lint" ...`: passed
- `forge save --json`: changed `docs/DOGFOOD_PLAN.md` and `eslint.config.js`
- `forge run --json -- npm run typecheck`: passed
- `forge run --json -- npm test`: passed, with local checkout path redacted from
  persisted output
- `forge run --json -- npm run build`: passed
- `forge run --json -- npm run lint`: passed
- `forge propose --json --summary ...`: passed
- `forge check --json`: passed all four required gates
- `forge accept --json`: accepted proposal
  `proposal_019ef8cfa5e6728391725ce8a0086aef` as native commit
  `f1:commit:sha256:377a30002adb0dda9b342dcef1291a8eec6f7ee56d01e7c548d2e79179f9edfe`

This generic lifecycle dogfood pass did not expose an rc5 Forge lifecycle
blocker. It did expose a repeatable JavaScript tooling requirement for projects
using Forge worktrees: broad lint/test tools must exclude `.forge/**`, and typed
ESLint configs should pin their parser root.

## Feature-Specific Projection Dogfood

Follow-up dogfood explicitly exercised the rc5 permissioned projection feature
in the `forge-dogfood` checkout.

Policy and export-side checks:

- `forge visibility set --kind proposal --id proposal_019ef8cfa5e6728391725ce8a0086aef --visibility private`: passed
- `forge visibility check --recipient rc5-outsider --capability sync_materialize`: passed with `allowed: false` and `disclosure: hidden`
- `forge visibility grant --recipient rc5-release-auditor --capability sync_materialize`: passed
- `forge visibility check --recipient rc5-release-auditor --capability sync_materialize`: passed with `allowed: true` and `disclosure: full`
- `forge sync export --recipient rc5-outsider --capability sync_materialize`: passed, producing a projected bundle with `native_head: null`
- `forge sync export --recipient rc5-release-auditor --capability sync_materialize`: passed, producing a projected bundle with native head `f1:commit:sha256:377a30002adb0dda9b342dcef1291a8eec6f7ee56d01e7c548d2e79179f9edfe`
- full, non-projected `forge sync export` followed by `forge sync import --materialize` into a fresh receiver: passed

Receiver-side projected bundle checks:

- allowed projected `forge sync import --materialize`: failed with `COMMAND_FAILED: apply sync bundle`
- allowed projected `forge sync import` without materialization: failed with `COMMAND_FAILED: apply sync bundle`
- allowed projected `forge sync clone`: failed with `COMMAND_FAILED: clone sync bundle`

Conclusion: rc5 proves visibility policy decisions and recipient-scoped export
metadata/count behavior, but does not prove projected receiver import or
materialization. This is a release-candidate limitation and should be fixed
before the next RC that claims end-to-end permissioned projections. Tracking:
NER-363.

## Feature-Specific Organization Identity Dogfood

Follow-up dogfood explicitly exercised the rc6 organization identity foundation
in the `forge-dogfood` checkout and in a fresh temporary repository.

Existing dogfood repository checks:

- `forge --json org status`: migrated the repository to schema version 19 and
  reported organization governance disabled with zero principal/key/role counts.
- `forge --json visibility policy`: continued to work before organization
  activation.
- `forge --json key status`: continued to work before organization activation.
- `forge --json --request-id dogfood-org-init org init --actor skolte --reason
  "NER-357 dogfood bootstrap"`: created one owner principal, one signing-key
  binding, one owner role binding, one `org_init` audit row, and one
  `org init`/`org_initialized` operation.
- Replaying `dogfood-org-init` with changed actor/reason returned
  `idempotent_replay: true` and preserved the original bootstrap result.
- A second bootstrap without the replay id failed closed with
  `ORG_ALREADY_ENABLED`.
- `forge --json org status` after activation reported enabled governance with
  principal/key/role counts of 1/1/1.

Fresh repository checks:

- Blank actor bootstrap failed with `ORG_AUTHORITY_REQUIRED`.
- The failed blank-actor attempt left organization profile rows unmodified.
- A valid request-id bootstrap created the owner identity and replayed
  idempotently.
- `forge schema` exposed `org status`, `org init`, and the organization
  governance error codes.

Conclusion: rc6 proves the local durable organization identity bootstrap path,
request-id replay, schema discoverability, and closed failure for missing
authority. It does not yet prove broad organization policy enforcement,
multi-admin management, authority rotation/revocation, hosted identity, or
cross-organization certificate authority behavior.

## Feature-Specific Encrypted Private Content Dogfood

Follow-up dogfood explicitly exercised the rc7 encrypted private content
overlay path in temporary copies of the `forge-dogfood` checkout, using the
candidate binary from `main` at `2711761`.

Source repository checks:

- `forge --json init --content-backend native`: initialized a native Forge
  repository from the dogfood app source.
- `forge --json org init --actor dogfood-owner`: bootstrapped the local owner
  principal.
- `forge --json org encryption bind-local --principal-id <owner>`: bound the
  owner principal to the local age recipient.
- `forge --json start "NER-356 dogfood private extension"`: created a dogfood
  attempt.
- `forge --json visibility path set --kind attempt --id <attempt> --path
  src/private-extension.ts --visibility private`: labeled a dogfood source file
  as private.
- `forge --json visibility grant --kind attempt --id <attempt> --recipient
  <owner> --capability sync_materialize`: granted authorized materialization.
- `forge --json save`, `forge --json propose`, and `forge --json accept
  --allow-unverified`: saved and accepted the work while private-tainted
  evidence remained intentionally unsupported.

Projection checks:

- Unauthorized `forge sync export --recipient outsider@example.test` produced a
  generic `forge-sync.v2` bundle with `private_content.capable: false` and no
  private path, private sentinel, `.forge/private/objects` path, or private
  payload count.
- Authorized `forge sync export --recipient <owner>` produced a bundle with
  `private_content.capable: true`, one encrypted private overlay, no plaintext
  sentinel, and no source private object-store path.

Receiver checks:

- A fresh native target repository copied only the recipient private key,
  imported the authorized bundle with `forge sync import <bundle>
  --materialize`, and restored both the public dogfood file and the private
  `src/private-extension.ts` file.
- Editing the materialized private file and running `forge save` re-encrypted
  the path and did not leak the private sentinel into public native objects.

Real dogfood checkout app checks:

- `npm run typecheck`: passed.
- `npm test`: passed, 1 file / 3 tests.
- `npm run build`: passed.
- `npm run lint`: initially failed because ESLint scanned `.forge/**` attempt
  worktrees and lacked an explicit `tsconfigRootDir`. The dogfood repo was fixed
  in commit `6b24be5` by ignoring `.forge/**` and pinning `tsconfigRootDir`;
  lint then passed.

Conclusion: rc7 proves the first local encrypted private-content path: exact
private file labels, local encryption before public tree storage, unauthorized
projection omission, authorized projected overlay transport, materialization,
and private-label preservation on re-save. It does not yet prove hosted key
distribution, multi-admin key rotation UX, same-user zero-trust after local
materialization, or sanitized public reveal/evidence workflows.

## Feature-Specific Embargoed Security-Fix Dogfood

Follow-up dogfood explicitly exercised the rc8 embargoed security-fix workflow
in a detached `forge-dogfood` worktree at `6b24be5`, using an installed
candidate binary from `main` at `d7c3e01`.

Baseline app checks:

- `npm run typecheck`: passed.
- `npm test`: passed.
- `npm run build`: passed.
- `npm run lint`: passed.

Sanitized-source embargo checks:

- `forge init --content-backend native`: initialized the dogfood repository.
- `forge visibility set --visibility embargoed`: preserved the compatibility
  alias by marking a selected proposal as embargoed.
- `forge embargo mark`: marked accepted work as embargoed.
- A generic `forge visibility grant` against the embargoed proposal failed with
  `EMBARGO_WORKFLOW_REQUIRED`.
- `forge embargo release` before accept failed with `EMBARGO_STATE_INVALID`.
- A release with only `sync_materialize` failed with `VISIBILITY_POLICY_UNMET`.
- A release after both `sync_materialize` and `publish_reveal` emitted a
  metadata-only `embargo-release.v1` manifest.
- Releasing to an occupied path failed without advancing release state.
- `forge sync inspect` accepted the release manifest, while generic `forge sync
  import` and `forge sync clone` refused it fail-closed.
- Tampering with the release digest failed inspection.
- `forge embargo reveal --mode sanitized-source` and `forge embargo publish`
  succeeded, but Git branch export stayed blocked because sanitized publication
  does not satisfy the full-source export policy.

Full-source and closure checks:

- A full-source embargo flow proved release, full-source reveal, full-source
  publish, and successful `forge export branch` after publication.
- A closed embargo flow proved reveal remains blocked after close with
  `EMBARGO_STATE_INVALID`.

Conclusion: rc8 proves a local embargoed security-fix workflow with explicit
release capability grants, no-clobber metadata release output, digest-checked
inspection, fail-closed generic sync import/clone, source-mode publication
guards, and Git export only after full-source publication. It does not yet
prove hosted advisory coordination, CVE workflows, cross-organization receiver
identity, or resumable embargo transport.

## Feature-Specific Local Review Surface Dogfood

Follow-up dogfood explicitly exercised the rc9 local proposal review surface in
a temporary clone of the `forge-dogfood` checkout at `6b24be5`, using the
candidate binary from `main` at `be0f458` after PR #106 merged.

Baseline app checks:

- `npm run typecheck`: passed.
- `npm test`: passed.
- `npm run build`: passed.
- `npm run lint`: passed.

Review-surface workflow checks:

- `forge init`: initialized the dogfood repository.
- `forge start` used the dogfood app checks as required gates.
- A tracked README change was saved, proposed, and checked as
  `proposal_019f291ade5572928a3388f0bffb5ef4`.
- `forge run -- npm run typecheck`: passed.
- `forge run -- npm test`: passed.
- `forge run -- npm run build`: passed.
- `forge run -- npm run lint`: passed.
- `forge review show --proposal <proposal>` reported readiness `ready` and
  emitted a read-only JSON aggregate.
- `forge review export --proposal <proposal> --output review.html` wrote a
  static HTML artifact with readiness `ready`.
- `forge review open --proposal <proposal> --output review-open.html
  --no-browser` wrote the same artifact path without browser launch.
- `forge accept --proposal <proposal>` kept the trust-bearing decision in the
  terminal.

Projection and browser checks:

- CLI integration tests prove private proposal paths are represented as
  restricted metadata in both JSON and HTML and do not leak private path
  sentinels or payloads.
- The generated HTML was opened in the in-app browser and checked at desktop and
  mobile widths for readable hierarchy, command/id wrapping, six content
  sections, no forms, no scripts, no buttons, and absence of horizontal
  overflow.

Conclusion: rc9 proves the first local, read-only proposal review surface over
existing Forge ledger facts: readiness, lifecycle, evidence, trust, visibility,
embargo status, diff summary, and terminal handoff. It does not yet prove hosted
accounts, hosted comments, cloud execution, UI-triggered trust-bearing
mutations, or a full GitHub PR replacement.

## Feature-Specific Native/Git Drift Warning Dogfood

Follow-up dogfood explicitly exercised the rc10 native/Git drift warning in a
temporary copy of the `forge-dogfood` checkout, using an installed candidate
binary from `main` at `d480e94` after PR #122 merged. The live dogfood checkout
was left untouched.

Baseline app checks:

- `npm run typecheck`: passed.
- `npm test -- --run`: passed, 1 file / 3 tests.
- `npm run build`: passed.
- `npm run lint`: passed.

Workflow checks:

- `forge init --content-backend native`: initialized a fresh native Forge repo
  in the copied dogfood checkout.
- `forge doctor`: passed before the workflow.
- `forge start` declared `npm run typecheck`, `npm test -- --run`,
  `npm run build`, and `npm run lint` as required gates.
- A Git-only commit added `docs/rc10-git-only.txt`, then an unsaved README edit
  represented the actual attempt work.
- `forge save` reported `changed_paths` as `README.md` and
  `docs/rc10-git-only.txt`.
- The same `forge save` response emitted a top-level warning naming only
  `docs/rc10-git-only.txt` as clean in Git but changed relative to Forge native
  base; the warning did not name the actually dirty `README.md` edit.
- `forge run -- npm run typecheck`: passed.
- `forge run -- npm test -- --run`: passed.
- `forge run -- npm run build`: passed.
- `forge run -- npm run lint`: passed.
- `forge propose --summary ...`, `forge check`, `forge accept --actor
  rc10-dogfood`, and final `forge doctor`: passed.

Accepted proposal:
`proposal_019f3296a1c07e70ad847de0e7bcd7f3`.

Accepted native commit:
`f1:commit:sha256:f0e2798b37439931ee350384a176601653be75952a07cff04bcc525c25eb38cd`.

Conclusion: rc10 preserves native `changed_paths` as a Forge-native-base diff
while giving agents a clear warning when a path is clean in Git and likely comes
from native/Git history drift. It does not automatically reconcile Git HEAD into
Forge native history or block acceptance after the warning.

## Exit Criteria

| Roadmap criterion | Evidence | Status |
| --- | --- | --- |
| Two machines with no git installed clone/fetch/push/pull and reach byte-identical object stores and ledgers with consistent refs. | `scripts/dogfood-native-sync-release-litmus.sh` runs all Forge commands through a `PATH` containing `sh` but no `git`, simulates two isolated directories, then compares exported manifests for native objects, payloads, refs, and syncable domain-ledger rows. `scripts/dogfood-native-sync-peer-nogit.sh` separately covers file-URL clone/fetch/pull/push without git. | Proven |
| Incoming proposal conflicting with a moved target is merged or surfaced as conflict-as-data. | `scripts/dogfood-native-sync-release-litmus.sh` asserts a conflicting fetch succeeds with `merged=false`, records one conflict, and leaves `doctor` healthy. `crates/forge-cli/tests/forge_sync.rs` covers fetch/pull/push divergence, conflict set transport, dedup, and remote push conflicts. | Proven |
| Clean divergent sync records native merge commits at the receiver boundary. | `scripts/dogfood-native-sync-release-litmus.sh`, `scripts/dogfood-native-sync-peer.sh`, and `scripts/dogfood-native-sync-peer-nogit.sh` assert clean fetch/pull/push merge commit ids, HEAD convergence, and domain-ledger convergence. `crates/forge-cli/tests/forge_sync.rs` covers local, file, fake-SSH, and HTTP transports. | Proven |
| Native commits, decisions, and evidence are signed and verified by doctor/verify. | `crates/forge-cli/tests/forge_signatures.rs` verifies evidence, decision, native commit, key rotation, missing signature, invalid signature, and disappeared-subject cases. `scripts/e2e-eval.sh` runs `doctor` through signed lifecycle paths. | Proven |
| Trust ladder is enforced, including failure when a policy requires a stronger tier than available. | `crates/forge-cli/tests/forge_trust_policy.rs` covers `locally_signed`, hosted-runner, third-party, spoofed-upgrade rejection, missing evidence, and export policy. `scripts/dogfood-hosted-runner-attestation.sh` proves hosted accept and third-party export fail closed before attestation and pass after explicit issuer-key attestations. | Proven |
| Git-export consumer verifies a signed trailer back to the ledger. | `scripts/e2e-eval.sh` exports a ranked winner and verifies `Forge-Provenance-Digest`. `crates/forge-cli/tests/forge_accept_export.rs` verifies local signature fingerprint trailers and mismatch failure modes. | Proven |
| Full litmus test covers init, commit history, branch/divergence, merge with conflict resolution, clone/push to another machine, all without git. | The aggregate gate covers this end-to-end surface across scripts: no-git init/history/restore/undo in `scripts/e2e-eval.sh`; no-git clone/fetch/pull/push in `scripts/dogfood-native-sync-release-litmus.sh` and `scripts/dogfood-native-sync-peer-nogit.sh`; real TypeScript conflict suggestion, explicit conflict resolution, re-check, accept, and multi-workspace isolation in `scripts/dogfood-typescript-native.sh`. | Proven as aggregate gate, not one monolithic script |

## Current Release Boundary

The local/native release claim is supportable:

- Forge can run the core native lifecycle with git removed from `PATH`.
- Forge can sync native history and provenance between local/file/SSH/HTTPS peers.
- Forge can represent remote-boundary conflicts as typed conflict-as-data.
- Forge can sign local evidence, decisions, native commits, and sync merge commits.
- Forge can enforce local, hosted-runner, and third-party trust policies.
- Forge can emit and import recipient-scoped sync projection bundles for local
  permissioned collaboration boundaries.
- Forge can bootstrap a local organization identity profile, owner principal,
  signing-key binding, owner role, and audit trail, but rc6 does not yet enforce
  organization policy across the full command surface.
- Forge can store exact private source/config paths as encrypted overlays,
  omit them from unauthorized projections and Git export, and materialize them
  for an authorized local org-bound recipient.
- Forge can run a local embargoed security-fix workflow, produce metadata-only
  release manifests, keep generic sync import/clone fail-closed, and gate Git
  branch export on full-source reveal/publish audit records.
- Forge can still export accepted work to Git branches for existing PR workflows.

The supported public wording should avoid claiming a hosted multi-tenant service,
global identity, revocation infrastructure, resumable network transfer, or
cross-organization certificate authority. Hosted collaboration, organization-wide
policy management, and identity governance remain product follow-ons.

## Addendum 2026-07-18 — CCX native contracts + R21 retirement dogfood

The contract-driven agent-work surface (`forge contract` family: lint, freeze,
brief, run, integrate, verify, stops, show, runs, verdicts, resolve — plan
`docs/plans/completed/2026-07-10-001-feat-ccx-native-contracts-plan.md`) merged
to main at `035e8f3` after two multi-model review gates (triage:
`docs/code-reviews/2026-07-18-ccx-native-contracts.md`).

**R21 retirement criterion: satisfied in anger** (dogfood #3, 2026-07-18,
record: `docs/code-reviews/2026-07-18-dogfood3-gc-repack.md`): a real
Fable-agent contract run implemented the gc repack-reachability fix through
the native surface end-to-end — author → freeze → run (first run refused by
the blast secret-content gate, triaged as a tooling defect and fixed as
diff-aware scanning `29f70d6`) → rerun completed → verify green (fix=2
guard=3) → integrate → accept → doctor green — with the agent invocation as
the only script glue. Per the plan's staged-stability decision, the contract
ledger kinds and their envelope surfaces are now **adopter-grade stable**, and
the overlapping `tools/ccx` script stages (lint/brief/blast/run/verify) are
**retired**; the scripts remain in-tree as the frozen reference
implementation and for their self-tests.

Additional supportable claim: forge gc never repacks unreachable objects;
refused (e.g. secret-bearing) content is reclaimed from the loose store and
packs in one gc cycle (`b5d7f3c`).
