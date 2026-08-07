# Forge Public Release Notes

## v0.1.0-rc11

Forge v0.1.0-rc11 is a public release candidate that ships the contract-driven
agent-work surface: agent tasks are now first-class, ledger-recorded Forge
objects that can be authored, frozen, run, verified, and integrated entirely
through the native CLI.

### Changed

- Added the `forge contract` family — `lint`, `freeze`, `brief`, `run`,
  `integrate`, `verify`, `stops`, `show`, `runs`, `verdicts`, `resolve` — with
  fail-closed scratch runs, typed stop codes, and redacted agent-output capture.
- Made the blast secret-content gate diff-aware: modified files are scanned only
  on the agent-authored delta against the baseline blob, so pre-existing content
  no longer triggers refusals; new files still scan whole.
- Fixed `forge gc` to never repack unreachable objects: refused (e.g.
  secret-bearing) content is reclaimed from both the loose store and packs in a
  single gc cycle.
- Extended `forge doctor` and gc coverage to the contract ledger kinds.
- Retired the overlapping `tools/ccx` script stages; the scripts remain in-tree
  as the frozen reference implementation. Contract ledger kinds and their
  envelope surfaces are now adopter-grade stable.

### Verification

The aggregate dogfood gate passed at the tagged commit on 2026-08-06:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 604 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

The contract surface itself was proven in anger by dogfood #3: a real agent
contract run implemented the gc repack-reachability fix end-to-end (author →
freeze → run → verify → integrate → accept → doctor green), recorded in
`docs/code-reviews/2026-07-18-dogfood3-gc-repack.md` and the release audit
addenda in `docs/P9_RELEASE_AUDIT.md`.

## v0.1.0-rc10

Forge v0.1.0-rc10 is a public release candidate focused on making native
history/Git drift visible before agents accept work. Native `changed_paths`
remain ledger-accurate, but `forge save` and `forge propose` now warn when a
reported path is clean in Git and differs only because Forge's native base and
Git HEAD have drifted.

### Changed

- Added a best-effort native-backend warning for paths that are clean in Git but
  changed relative to Forge's native base.
- Surfaced the warning from both `forge save` and `forge propose`, so agents see
  stale native-base drift before `check` or `accept`.
- Kept the native snapshot contract unchanged: `changed_paths` still reports the
  diff against the Forge native base, and the warning explains how to interpret
  Git-clean paths rather than hiding them.

### Current Boundary

This RC improves local agent UX for repositories that use both Git and Forge
native history. It does not automatically reconcile Git HEAD into Forge native
history, rewrite `changed_paths`, or prevent a user from accepting a proposal
after reviewing the warning.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc10 forge-cli
```

### Release Validation

The rc10 preparation ran the aggregate release dogfood gate on `main` at
`d480e94` after PR #122 merged:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 604 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

External dogfood ran with an installed candidate binary from current `main`
against a temporary copy of the real `forge-dogfood` checkout, leaving the live
checkout untouched:

- `npm run typecheck`: passed.
- `npm test -- --run`: passed, 1 file / 3 tests.
- `npm run build`: passed.
- `npm run lint`: passed.
- `forge init --content-backend native` and `forge doctor`: passed.
- `forge start` declared the dogfood app checks as required gates.
- A Git-only commit added `docs/rc10-git-only.txt`, then an unsaved README edit
  represented the actual attempt work.
- `forge save` reported `changed_paths` as `README.md` and
  `docs/rc10-git-only.txt`, and emitted a warning naming only
  `docs/rc10-git-only.txt` as clean in Git but changed relative to Forge native
  base.
- `forge run -- npm run typecheck`: passed.
- `forge run -- npm test -- --run`: passed.
- `forge run -- npm run build`: passed.
- `forge run -- npm run lint`: passed.
- `forge propose --summary ...`, `forge check`, `forge accept --actor
  rc10-dogfood`, and final `forge doctor`: passed.
- Accepted dogfood proposal:
  `proposal_019f3296a1c07e70ad847de0e7bcd7f3`.
- Accepted native commit:
  `f1:commit:sha256:f0e2798b37439931ee350384a176601653be75952a07cff04bcc525c25eb38cd`.

## v0.1.0-rc9

Forge v0.1.0-rc9 is a public release candidate focused on the first local
proposal review surface. It gives maintainers a read-only review command group
and a self-contained static HTML page for deciding whether one proposal is
ready, risky, or blocked without parsing raw JSON or mutating Forge state from a
browser.

### Added

- Added the read-only `forge review` command group for local proposal review:
  `review show` emits the machine-readable review aggregate, `review export`
  writes a self-contained static HTML review page, and `review open` exports the
  same page before a best-effort browser launch.
- The review surface summarizes readiness as `ready`, `risky`, or `blocked`
  from existing Forge facts: proposal lifecycle, latest check/evidence, trust
  policy, visibility/projection state, private-path restrictions, embargo state,
  publication state, and terminal handoff commands.
- Added projection-safe JSON and HTML egress for review. Sanitized review output
  remains the default; private paths, payloads, and embargo-sensitive details
  are represented as restricted metadata unless existing policy grants allow a
  recipient-specific projection check.

### Current Boundary

The review surface is local-first and read-only. It does not add hosted
accounts, comments, teams, notifications, cloud execution, or browser-triggered
accept/reject/reveal/publish/export actions.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc9 forge-cli
```

### Release Validation

The rc9 preparation ran the aggregate release dogfood gate on `main` at
`be0f458` after PR #106 merged:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 603 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

Feature-specific review-surface validation covered:

- `forge review show` returns a read-only JSON aggregate with no operation row.
- Missing checks render a blocked review and unknown proposal ids return
  `UNKNOWN_PROPOSAL`.
- `forge review export` writes escaped static HTML with no mutating controls,
  forms, or scripts.
- `forge review open --no-browser` writes the same HTML artifact without
  launching a browser.
- Private proposal paths are represented as restricted metadata in both JSON
  and HTML and do not leak the private path sentinel or payload.
- A real dogfood proposal in `forge-dogfood` at `6b24be5`, using the candidate
  binary from `main` at `be0f458`, exercised `npm run typecheck`, `npm test`,
  `npm run build`, `npm run lint`, `forge review show`, `forge review export`,
  `forge review open --no-browser`, and terminal `forge accept`.
- The dogfood proposal was
  `proposal_019f291ade5572928a3388f0bffb5ef4`.
- The generated HTML review surface was opened in the in-app browser and
  checked on desktop and mobile widths, including long id/command wrapping,
  six content sections, no forms, no scripts, no buttons, and no horizontal
  overflow.

## v0.1.0-rc8

Forge v0.1.0-rc8 is a public release candidate focused on embargoed
security-fix workflows. It lets agents mark accepted work as embargoed, require
explicit release capabilities before publication, emit metadata-only release
manifests for authorized reviewers, and keep branch export blocked until an
audited reveal/publish step allows the source mode that was chosen.

### What Changed Since rc7

- Added schema migration 21 for embargo workflow state, events, authorized
  release capabilities, release manifests, and publish records.
- Added `forge embargo mark`, `grant`, `release`, `reveal`, `publish`, and
  `close` commands for a local embargo lifecycle around accepted proposals.
- Added a compatibility path so `forge visibility set --visibility embargoed`
  marks the selected proposal as embargoed while the general visibility grant
  path refuses to bypass the embargo workflow.
- Added release guards that require both `sync_materialize` and `publish_reveal`
  before a metadata-only embargo release manifest can be emitted.
- Added fail-closed receiver behavior for generic sync import/clone of embargo
  release manifests, plus digest verification for release-manifest inspection.
- Added source-mode publication controls: sanitized-source publication remains
  blocked from Git branch export, while full-source publication allows export
  after the reveal/publish audit trail is recorded.
- Moved GitHub Actions setup to Node 24 for the supported CI runtime.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc8 forge-cli
```

### Release Validation

The rc8 preparation ran the aggregate release dogfood gate on `main` at
`d7c3e01` after PR #105 merged:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 596 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

Feature-specific dogfood ran with an installed candidate binary from current
`main` against the real `forge-dogfood` checkout at `6b24be5`:

- `npm run typecheck`: passed.
- `npm test`: passed.
- `npm run build`: passed.
- `npm run lint`: passed.
- A sanitized-source embargo flow proved `mark`, compatibility visibility
  marking, release-capability grants, accepted proposal gating, no-clobber
  release output, metadata-only release inspection, generic sync import/clone
  fail-closed behavior, tamper-digest rejection, sanitized reveal/publish, and
  blocked Git branch export after sanitized publication.
- A full-source embargo flow proved release, full-source reveal/publish, and
  successful Git branch export after publication.
- A closed embargo flow proved reveal remains blocked after close.

### Current Boundary

This RC proves the first local embargoed security-fix path: accepted work can be
kept out of Git export until explicit release capabilities, reveal mode, and
publish audit records exist. Release manifests are metadata-only and generic
sync import/clone intentionally refuse them fail-closed pending a fuller
authorized receiver flow. Forge still does not provide hosted advisory
coordination, CVE issuance, multi-tenant identity, revocation infrastructure, or
resumable transport.

## v0.1.0-rc7

Forge v0.1.0-rc7 is a public release candidate focused on encrypted private
content overlays. It lets one Forge graph carry public work plus private
source/config paths, stores private paths outside the public native tree, omits
private material from unauthorized projections, and materializes authorized
private overlays for an org-bound recipient key.

### What Changed Since rc6

- Added the internal `forge-private` crate with age X25519 envelope helpers for
  private payload encryption, recipient fingerprints, tamper detection, and
  debug/serde secrecy boundaries.
- Added schema migration 20 for organization encryption key bindings, private
  path labels, encrypted private payload rows, and private-content audit rows.
- Added `forge visibility path set` for exact repo-relative private path labels
  and `forge org encryption bind-local` / `forge org decrypt-authority` for the
  first local org-bound decrypt-authority flow.
- Changed native save for labeled private paths so public `forge-tree:` content
  excludes those paths and encrypted overlays are recorded separately.
- Hardened public egress: public Git export and unauthorized projected sync omit
  private plaintext, private ciphertext, private object paths, and private
  existence metadata.
- Added authorized projected sync transport/materialization for private overlays
  when the recipient has `sync_materialize` visibility and an active org
  encryption key binding.
- Added private-tainted evidence fail-closed behavior for this slice so raw
  command output from private materialized work is not persisted before a future
  sanitized reveal mode exists.
- Updated install and plugin references to the `forge-vcs/forge` repository
  path after the GitHub organization transfer.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc7 forge-cli
```

### Release Validation

The rc7 preparation ran the aggregate release dogfood gate on `main` at
`2711761` after PR #103 merged:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 589 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 5.9.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

Focused feature validation also passed:

- `cargo test -p forge-cli --test forge_encrypted_private_content`: 10 passed
- `cargo test -p forge-cli --test forge_schema --test forge_org_identity --test forge_sync --test forge_visibility`: 66 passed
- `cargo test -p forge-sync`: 8 passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed

Feature-specific dogfood ran with the candidate binary in temporary copies of
the `forge-dogfood` checkout:

- A private dogfood source file was labeled under an attempt and saved as an
  encrypted overlay while the public app file remained in the public tree.
- An unauthorized projected sync bundle used generic `forge-sync.v2` metadata
  and omitted the private path, private sentinel, private object path, and
  private capability/count metadata.
- An authorized projected sync bundle carried one encrypted private overlay,
  without plaintext sentinel or private object-store paths.
- A fresh target repository with the recipient private key imported the
  authorized bundle with `--materialize`, restored the private file locally, and
  re-saved without leaking the private sentinel into public native objects.

External dogfood app validation also ran in the real `forge-dogfood` checkout:

- `npm run typecheck`: passed
- `npm test`: passed, 1 file / 3 tests
- `npm run build`: passed
- `npm run lint`: initially exposed stale ESLint scoping around `.forge/**`
  worktrees; `forge-dogfood` commit `6b24be5` pins `tsconfigRootDir` and ignores
  `.forge/**`, after which lint passed.

### Current Boundary

This RC proves the first local encrypted private-content path: exact private
file labels, local at-rest encryption, authorized projected transport, and
materialization with local private-label preservation. It does not yet provide a
hosted key-distribution service, multi-admin key rotation UX, cross-org
certificate authority, same-user zero-trust after plaintext materialization, or
public reveal/sanitized private-evidence workflows.

## v0.1.0-rc6

Forge v0.1.0-rc6 is a public release candidate focused on the first
organization identity and key-governance foundation slice. It adds an
explicit organization bootstrap path and durable identity-governance tables
without yet claiming hosted identity, certificate authority, revocation, or
organization policy-management behavior.

### What Changed Since rc5

- Added schema migration 19 for organization authority profiles, principals,
  principal aliases, key bindings, role bindings, issuer bindings, and
  organization policy audit rows.
- Added `forge org status` so agents can inspect whether organization
  governance is enabled and see principal/key/role counts through the stable
  JSON envelope.
- Added `forge org init --actor <id> [--reason <text>]` to bootstrap a local
  owner principal, bind the current local signing key to that principal, record
  the owner role, and write replay-safe audit/operation evidence.
- Added typed organization-governance errors:
  `ORG_NOT_ENABLED`, `ORG_ALREADY_ENABLED`, and `ORG_AUTHORITY_REQUIRED`.
- Added schema metadata for the new org commands and errors so agents can
  discover the surface without scraping human help text.
- Preserved legacy local workflows before and after organization bootstrap:
  visibility policy, key status, and existing lifecycle commands continue to
  work when org governance is disabled or newly enabled.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc6 forge-cli
```

### Release Validation

The rc6 preparation ran the aggregate release dogfood gate on the PR #101
candidate branch after the NER-357 implementation and rc6 release-doc updates:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all -- --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 568 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 5.9.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

Focused feature validation also passed:

- `cargo test -p forge-cli --test forge_org_identity`: 5 passed
- `cargo test -p forge-cli --test forge_schema`: 7 passed
- `cargo test -p forge-store --test migrate`: 9 passed
- `cargo test -p forge-store`: 79 passed
- `cargo test -p forge-cli --tests`: 303 passed

Feature-specific dogfood ran with the candidate binary in the
`forge-dogfood` checkout:

- `forge --json org status` migrated the existing dogfood repository to schema
  version 19 and reported organization governance disabled with zero
  principal/key/role counts.
- `forge --json visibility policy` and `forge --json key status` continued to
  work before org activation.
- `forge --json --request-id dogfood-org-init org init --actor skolte --reason
  "NER-357 dogfood bootstrap"` created one owner principal, one signing-key
  binding, one owner role binding, one `org_init` audit row, and an
  `org init`/`org_initialized` operation.
- Replaying the same request id with changed actor/reason returned
  `idempotent_replay: true` and the original bootstrap result.
- A second bootstrap without the replay id failed closed with
  `ORG_ALREADY_ENABLED`.
- `forge --json org status` after activation reported organization governance
  enabled with principal/key/role counts of 1/1/1.
- A fresh temporary repository rejected blank actors with
  `ORG_AUTHORITY_REQUIRED` and left organization profile rows unmodified, then
  successfully bootstrapped and replayed a valid owner initialization.
- `forge schema` exposed `org status`, `org init`, and the org-governance error
  codes.

### Current Boundary

This RC adds the durable local foundation for organization identity and
key-governance. It does not yet enforce organization policy across every
existing command, manage multi-admin grants, rotate or revoke organization
authority, provide hosted identity, or implement a cross-organization
certificate authority. Those remain follow-on slices.

## v0.1.0-rc5

Forge v0.1.0-rc5 is a public release candidate focused on permissioned Forge
projections. It makes local visibility policy explicit and adds
recipient-scoped sync export/import hardening so restricted work can be kept out
of projected bundles.

### What Changed Since rc4

- Added visibility policy storage for work packages, including visibility
  labels, grants, revocation, audit rows, and typed visibility errors.
- Added `forge visibility` command coverage and schema metadata so agents can
  inspect policy and projection decisions without scraping human output.
- Added protocol-visible sync projection metadata for full and recipient-scoped
  manifests.
- Added recipient-scoped `sync export --recipient` support for
  `sync_materialize` projections.
- Hardened projected sync boundaries by filtering unsafe ledger tables,
  recomputing projected counts, validating reachable native object closures,
  rejecting unsupported projection capabilities, and rejecting mismatched
  incremental projection bases.
- Updated the shell e2e migration-head gate so it derives the expected migration
  version from migration files instead of a hardcoded version literal.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc5 forge-cli
```

### Release Validation

The rc5 preparation ran the aggregate release gate on `main` at `ba24105`:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: 561 passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 5.9.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

External dogfood validation also ran against the published `v0.1.0-rc5` tag at
`ba241050` in the `forge-dogfood` checkout:

- isolated `cargo install --git ... --tag v0.1.0-rc5` succeeded
- `forge --version`, `forge schema --json`, and `forge doctor --json` passed
- baseline `npm run typecheck`, `npm test`, and `npm run build` passed
- baseline `npm run lint` exposed dogfood app tooling friction: ESLint scanned
  Forge-managed attempt worktrees and needed a `.forge/**` ignore plus explicit
  TypeScript parser root
- the fix was completed through the rc5 Forge lifecycle:
  `forge start`, `forge save`, four `forge run` evidence commands,
  `forge propose --summary`, `forge check`, and `forge accept`
- `forge check` passed all four required gates: `npm run typecheck`,
  `npm test`, `npm run build`, and `npm run lint`
- accepted native commit:
  `f1:commit:sha256:377a30002adb0dda9b342dcef1291a8eec6f7ee56d01e7c548d2e79179f9edfe`

Follow-up feature-specific dogfood for permissioned projections found a
receiver-side blocker:

- `forge visibility set` marked the dogfood proposal private
- `forge visibility check` denied `rc5-outsider` with `disclosure: hidden`
- `forge visibility grant` allowed `rc5-release-auditor` to
  `sync_materialize`
- `forge sync export --recipient rc5-outsider` produced a projected bundle with
  `native_head: null`
- `forge sync export --recipient rc5-release-auditor` produced a projected
  bundle with native head
  `f1:commit:sha256:377a30002adb0dda9b342dcef1291a8eec6f7ee56d01e7c548d2e79179f9edfe`
- full, non-projected `forge sync export` plus `forge sync import --materialize`
  succeeded in a fresh receiver
- projected `forge sync import` failed with `COMMAND_FAILED: apply sync bundle`
  for the allowed recipient bundle, with and without `--materialize`
- projected `forge sync clone` failed with `COMMAND_FAILED: clone sync bundle`

That means rc5 validates visibility decisions and projected export metadata, but
does not validate projected receiver import/materialization. Treat this as a
known rc5 limitation and a required fix before the next release candidate. This
is tracked in Linear as NER-363.

### Current Boundary

This RC is still a local/native release candidate. Permissioned projections are
Forge-managed projection enforcement, not encryption or a hosted
identity/governance system. Recipient-scoped projection import/clone is not yet
validated in rc5 because feature-specific dogfood exposed receiver-side
`COMMAND_FAILED` failures for projected bundles.

## v0.1.0-rc4

Forge v0.1.0-rc4 is a public release candidate focused on the second
open-source dogfood feedback pass after rc3. It keeps the rc3 release boundary
intact while fixing command-line friction that showed up in fresh agent
workflows.

### What Changed Since rc3

- Fixed `forge --version` and help-display paths so they print successfully and
  exit with status `0`.
- Made `STALE_BASE` responses actionable by adding a reason, recovery hint, and
  concrete recovery steps to the JSON details and human message.
- Added a non-fatal `forge doctor` warning when JavaScript/TypeScript test
  configuration may scan Forge-managed `.forge/worktrees` directories without a
  `.forge/**` exclude.
- Confirmed the rc4 build locally through focused dogfood smokes for version
  exit status, stale-base recovery JSON, and the doctor JS/TS worktree warning.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc4 forge-cli
```

### Release Validation

The rc4 preparation ran the aggregate release gate:

```bash
bash scripts/dogfood-release-gate.sh
```

Gate results:

- `cargo fmt --all --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 6.0.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

## v0.1.0-rc3

Forge v0.1.0-rc3 is a public release candidate focused on the first
open-source dogfood feedback after rc2. It keeps the rc1/rc2 release boundary
intact while tightening agent-facing UX, evidence hygiene, and public issue
intake.

### What Changed Since rc2

- Added GitHub issue templates for bug reports, feature requests, and security
  hardening requests.
- Added `forge propose --summary <text>` so agents can attach a concise proposal
  summary without depending on an unsupported positional argument.
- Improved unsupported structured-gate errors. For commands such as
  `--require-tests-pass "npm test"`, Forge now returns the original gate and a
  concrete `--require "npm test"` fallback suggestion for plain exit-code
  gating.
- Redacted local repository/worktree paths from persisted `forge run` evidence
  excerpts, including macOS `/private/tmp`/`/tmp` and
  `/private/var`/`/var` aliases.
- Documented `.forge/**` excludes for broad JavaScript/TypeScript test runners
  so tools such as Vitest do not discover duplicate tests in managed attempt
  worktrees.
- Documented stale-base recovery guidance for `accept` and `export`: start a
  fresh attempt from the current base, re-save, rerun evidence, then
  propose/check/accept again.
- Updated the Forge agent plugin install guidance to use the rc3 tag.

### Installation

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc3 forge-cli
```

### Release Validation

The rc3 preparation ran the aggregate release gate:

```bash
bash scripts/dogfood-release-gate.sh
```

The TypeScript dogfood portion used the local dogfood repository's TypeScript
binary on `PATH` because `tsc` is an explicit environment prerequisite for that
script. Gate results:

- `cargo fmt --all --check`: passed
- `cargo clippy --workspace --all-targets -- -D warnings`: passed
- `cargo test --workspace`: passed
- `scripts/e2e-eval.sh`: PASS=95 FAIL=0
- `scripts/dogfood-hosted-runner-attestation.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-release-litmus.sh`: PASS=32 FAIL=0
- `scripts/dogfood-native-sync-peer.sh`: PASS=26 FAIL=0
- `scripts/dogfood-native-sync-peer-nogit.sh`: PASS=26 FAIL=0
- `scripts/dogfood-typescript-native.sh`: PASS=44 FAIL=0 with TypeScript 6.0.3
- `scripts/dogfood-native-storage-scale.sh --smoke`: PASS=30 FAIL=0

## v0.1.0-rc2

Forge v0.1.0-rc2 is a public release candidate focused on the open-source
launch surface around rc1. The core local/native Forge CLI remains the rc1
feature set, with release readiness improvements for installation, security,
contribution, and agent onboarding.

### What Changed Since rc1

- Added the Forge agent plugin for Codex and Claude Code, including plugin
  manifests, marketplace metadata, and the `forge-cli` skill.
- Added README installation instructions that use the tagged GitHub release
  candidate:

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc2 forge-cli
```

- Added public `SECURITY.md` and `CONTRIBUTING.md` guides.
- Documented current package boundaries: Cargo GitHub tag install is supported;
  Homebrew and crates.io packages are planned but not published yet.
- Dogfooded the agent skill in a temporary repository against the local Forge
  binary, including lifecycle, multi-attempt, compare, export, and sync flows.

### Release Validation

Quick validation on `main` before preparing rc2:

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

All three commands passed locally on `d35f3a1`.

## v0.1.0-rc1

Forge v0.1.0-rc1 is the first public release candidate for the local/native
Forge surface: agent-native source control for checked change attempts, with
Git interop where existing PR workflows still need it.

### What Is Included

- Native Forge content storage with content-addressed blob, tree, and commit
  objects.
- Intent, attempt, snapshot, proposal, evidence, check, decision, publication,
  operation, and view records in a local SQLite ledger.
- Stable JSON envelopes for agent use with typed errors, warnings, details, and
  retry metadata.
- Declarative check gates bound to the exact proposal revision under review.
- Evidence redaction, structured evidence summaries, tamper-evident row hashes,
  and operation-chain verification.
- Compare/rank for competing attempts under one intent, including diff,
  per-gate results, structured metrics, and deterministic ranking.
- Native history navigation with `log`, `checkout`, `restore`, and `undo`.
- Native diff, merge, typed conflict-as-data, explicit conflict resolution, and
  evidence-backed conflict suggestions.
- Physical per-attempt workspaces, mark-sweep garbage collection, pack/index
  storage, retention accounting, and storage budget warnings.
- Local Ed25519 signatures for evidence, accepted decisions, native accepted
  commits, and sync merge commits.
- Trust policy enforcement for `locally_signed`, hosted-runner attestations, and
  third-party attestations.
- Versioned Forge sync manifests and peer clone/fetch/pull/push over local
  paths, `file://`, fake/real SSH command transport, and HTTPS `sync serve`
  endpoints.
- Git export interop with structured `Forge-*` provenance trailers and
  `export verify-branch`.

### Release Validation

The release gate is:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

The gate runs formatting, clippy, the workspace test suite, binary e2e checks,
hosted/third-party attestation dogfood, no-git native sync litmus, peer sync,
TypeScript multi-workspace dogfood, and native storage-scale smoke.

The latest audited run is recorded in [docs/P9_RELEASE_AUDIT.md](docs/P9_RELEASE_AUDIT.md).

### Supported Claim

Forge can now honestly claim:

- the core native lifecycle runs without the git binary in `PATH`
- native sync transfers both content and the review/provenance ledger
- clean divergent peers converge through native merge commits
- true remote-boundary conflicts surface as typed conflict-as-data
- local evidence, decisions, and native commits are signed and verified
- accept/export can enforce local, hosted-runner, and third-party trust rungs
- accepted work can still be exported to Git branches for existing PR workflows

### Current Boundaries

This release candidate does not claim:

- hosted multi-tenant collaboration
- global identity governance
- certificate authority or revocation infrastructure
- resumable/partial network transfer
- hosted permissions or organization policy management
- stable hosted API compatibility

Those are product follow-ons, not blockers for the local/native public release.

### Before Tagging

1. Confirm `main` is clean and synced with `origin/main`.
2. Run `rtk bash scripts/dogfood-release-gate.sh`.
3. Confirm GitHub `verify` is green on the release-prep PR.
4. Confirm [docs/P9_RELEASE_AUDIT.md](docs/P9_RELEASE_AUDIT.md) still names the
   commit validated by the latest release gate, or update it.
5. Tag the release candidate, for example:

```bash
git tag -a v0.1.0-rc1 -m "Forge v0.1.0-rc1"
git push origin v0.1.0-rc1
```

6. Make the repository public only after the pushed tag, README, license, and
   release notes are visible on `main`.
