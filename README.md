# Forge

Agent-native source control for checked change attempts.

Forge is a Rust CLI that records the lifecycle around an agent or human change:

```text
init -> start -> save -> run -> propose -> check -> accept -> sync/export
```

It keeps source snapshots, evidence, policy checks, decisions, native history,
and publication provenance in a local `.forge` repository. Every command can
emit a stable JSON envelope with `--json`, so agents can branch on typed data
instead of scraping terminal text.

Forge still interoperates with Git: accepted proposals can be exported to Git
branches with structured `Forge-*` provenance trailers. The native backend now
also supports Forge-owned content storage, history, diff/merge, garbage
collection, pack/index storage, and native peer sync.

## Installation

Forge is currently published as a public release candidate from GitHub. Install
the latest tagged RC with Cargo:

```bash
cargo install --git https://github.com/forge-vcs/forge --tag v0.1.0-rc10 forge-cli
```

This installs the `forge` binary:

```bash
command -v forge
forge schema --json
```

To build from source instead:

```bash
git clone https://github.com/forge-vcs/forge.git
cd forge
cargo install --path crates/forge-cli
```

Homebrew and crates.io packages are planned, but not published yet. Until then,
the GitHub tag install is the supported installation path.

## Agent Plugin

Forge ships a skill-only plugin that teaches coding agents how to use the
`forge` CLI safely. It packages the `forge-cli` skill for Codex and Claude Code
without adding hooks or MCP servers.

For Codex:

```bash
codex plugin marketplace add forge-vcs/forge
codex plugin add forge@forge
```

For Claude Code:

```text
/plugin marketplace add forge-vcs/forge
/plugin install forge@forge
/reload-plugins
```

For local plugin development from a checkout:

```bash
codex plugin marketplace add .
codex plugin add forge@forge
```

Claude Code can also add the local checkout with:

```text
/plugin marketplace add .
/plugin install forge@forge
```

After installing or updating the plugin, start a new agent thread so the bundled
skills are loaded.

## Why Forge Exists

Git is excellent at storing commits. It is not designed around agent workflows
where several attempts compete under one intent, each attempt carries command
evidence, and a reviewer wants to accept or publish only the checked proposal.

Forge makes those concepts first-class:

- intents, attempts, snapshots, proposals, evidence, checks, decisions, and
  publications are durable ledger records
- checks are bound to the exact proposal revision they evaluated
- compare/rank surfaces competing attempts by evidence and diff, not by branch
  naming convention
- accepted native commits carry the proposal, decision, actor, and evidence
  digest that justified them
- sync transfers native content plus the ledger rows needed to review the same
  evidence on another machine

## Safety Defaults

- Snapshots and exported branches exclude `.forge`, `.env`, `.env.*`, private
  keys, credential files, and obvious secret-risk paths.
- Evidence excerpts redact common token, password, secret, key, PEM, credential
  URL, high-entropy values, and local worktree paths before JSON output or
  SQLite persistence.
- `forge run` caps captured stdout/stderr excerpts and defaults to a 30 second
  timeout. Use `forge run --timeout-ms <n> -- <command>` for a shorter local
  bound.
- `forge restore`, `forge checkout`, `forge undo`, `forge attempt attach`, and
  materializing sync commands refuse unsaved dirty work before overwriting the
  worktree.
- `.forge/worktrees/<attempt-id>/` directories are managed stash space: only the
  attached attempt, materialized at the repo root, is editable. Edits made
  directly inside those stash directories are discarded on the next attach.
- Broad test runners should exclude `.forge/**`; otherwise tools such as Vitest
  may discover duplicate tests inside managed attempt worktrees.
- Mutating `--request-id` values are scoped to the command and replay the
  original success or failure. Reusing one for a different mutating command
  returns `REQUEST_ID_CONFLICT`.
- Repository writes use a local advisory lock, SQLite WAL, typed errors, and
  crash-tested store-before-ledger ordering.
- Evidence, decisions, and native commits are tamper-evident and locally signed;
  `forge doctor` verifies the ledger chain, native DAG, object store, packs, and
  signatures.
- Trust policy can require `locally_signed`, `hosted_runner_signed`, or
  `third_party_attested` signatures before accept/export.

## Common Workflow

```bash
forge init --content-backend native
forge start "implement the billing retry fix" --require "cargo test"

# edit files
forge save
forge run -- cargo test
forge propose
forge check
forge accept
```

To publish into an existing Git workflow:

```bash
forge export branch forge/billing-retry
forge export verify-branch forge/billing-retry
```

To compare competing attempts under one intent:

```bash
forge attempt start --intent <intent-id>
forge attempt attach <attempt-id>
# edit, save, run, propose
forge compare --intent <intent-id>
```

To review one proposal through a local read-only surface:

```bash
forge review show --proposal <proposal-id>
forge review export --proposal <proposal-id> --output review.html
forge review open --proposal <proposal-id>
```

The review aggregate and exported HTML start with proposal readiness
(`ready`, `risky`, or `blocked`), lifecycle state, evidence audit, visibility
and embargo status, projection-safe diff metadata, and copyable terminal
handoff commands. The browser surface does not accept, reject, reveal, publish,
export, or mutate Forge state; trust-bearing actions still run through the CLI.

## Native Sync

Native sync moves Forge history and ledger provenance between Forge repositories:

```bash
forge sync clone ./bundle.forge-sync.json
forge sync fetch /path/to/peer
forge sync pull file:///absolute/path/to/peer
forge sync push ssh://host/absolute/path/to/peer
```

Supported transports:

- local paths
- `file://` URLs
- `ssh://host/absolute/path`
- `https://` endpoints exposing `forge sync serve`

Fast-forward sync imports native object payloads and allowlisted ledger rows.
Clean divergent peers create native merge commits. True conflicts are persisted
as typed conflict-as-data records that can be inspected and resolved through the
Forge contract instead of being flattened into a text-only merge failure.

## Trust and Attestation

Forge records local Ed25519 signatures for new evidence, accepted decisions, and
native accepted commits. The trust ladder exposed by `forge trust policy` is:

- `self_reported`
- `locally_observed`
- `locally_signed`
- `hosted_runner_observed`
- `hosted_runner_signed`
- `third_party_attested`

Hosted-runner and third-party trust are explicit issuer-key attestations over a
proposal's current evidence subjects:

```bash
forge trust attest hosted-runner --proposal <proposal-id> --key runner.pk8
forge trust attest third-party --proposal <proposal-id> --key auditor.pk8
forge trust policy --accept locally_signed --export third_party_attested
```

Peer-imported signatures remain cryptographically verifiable, but they do not
silently satisfy local, hosted-runner, or third-party policy.

## Current Command Groups

- lifecycle: `init`, `start`, `save`, `run`, `propose`, `check`, `accept`,
  `reject`, `show`
- attempts and review: `attempt start`, `attempt list`, `attempt show`,
  `attempt attach`, `proposal list`, `review show`, `review export`,
  `review open`, `compare`, `attempt compare`, `diff`
- intents: `intent list`, `intent show`
- worktree/history: `restore`, `checkout`, `log`, `blame`, `undo`
- native merge: `merge`, `conflict list`, `conflict show`,
  `conflict show --suggest`, `conflict resolve`
- maintenance: `doctor`, `gc`
- trust: `key status`, `key rotate`, `trust policy`,
  `trust attest hosted-runner`, `trust attest third-party`
- visibility and embargo: `visibility policy`, `visibility set`,
  `visibility grant`, `visibility revoke`, `visibility check`,
  `visibility path set`, `embargo mark`, `embargo grant`,
  `embargo revoke`, `embargo release`, `embargo reveal`,
  `embargo publish`, `embargo close`
- sync: `sync export`, `sync inspect`, `sync import`, `sync clone`,
  `sync fetch`, `sync pull`, `sync push`, `sync serve`
- Git interop: `export branch`, `export pr-body`, `export verify-branch`
- contract: `schema`

Run `forge schema --json` for the machine-readable command shapes, error
registry, and provenance notes.

## Development

The repository uses `rtk` in local automation. Public contributors can run the
same commands directly if `rtk` is not installed.

```bash
rtk cargo fmt --all --check
rtk cargo test --workspace
rtk cargo clippy --workspace --all-targets -- -D warnings

# Without rtk:
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The release dogfood gate aggregates the core local, native, sync, storage, and
attestation checks:

```bash
rtk bash scripts/dogfood-release-gate.sh
```

## Citation

If you use Forge in published work, please cite it using [CITATION.cff](CITATION.cff).

## License

Forge is licensed under the MIT License. See [LICENSE](LICENSE).
