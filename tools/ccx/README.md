# CCX Thin Harness

> **Native surface note.** The `forge contract` command family now covers
> lint / brief / blast / run / verify natively as first-class signed ledger
> records (see the "Contract-Driven Agent Work" section of the top-level
> `README.md` and `docs/plans/2026-07-10-001-feat-ccx-native-contracts-plan.md`).
> These scripts remain **authoritative** until the R21 retirement dogfood — a
> full native chain (author → freeze → run → stop → triage → verify → integrate)
> with the agent invocation as the only script glue — is recorded. That
> retirement scenario now runs green as
> `native_chain_end_to_end_retirement_criterion` in
> `crates/forge-cli/tests/forge_contract_retirement.rs`; the scripts stay in place until the
> criterion is formally recorded in the release audit. Nothing below is
> deprecated yet.

A small, Git-compatible toolkit for running **context-closed tasks**:
implementation work executed by fresh agent sessions from byte-stable
contract briefs instead of transferred conversation history. Files and
scripts only — **deliberately no new Forge objects** (the decision-rule
boundary from `docs/brainstorms/2026-07-06-context-closed-tasks-v3.md` §4:
a Forge-native substrate comes only after this harness, the NER-362
dogfood, and pilots 2–3).

Every component transcribes a named defect from the 2026-07-06 contract
pilot (`experiments/ccx/`, RESULTS.md — STRONG pre-registered reading).
Background reading, in order:

- `docs/brainstorms/2026-07-06-context-closed-tasks-v3.md` — the seven
  earned design commitments this harness implements.
- `docs/solutions/design-patterns/stop-on-unknown-gate-for-agent-briefs.md`
- `docs/solutions/architecture-patterns/filesystem-enumeration-shared-exclusion-contract.md`
- `docs/solutions/conventions/contract-acceptance-is-not-merge-ready.md`

## Components

| Tool | Purpose |
|------|---------|
| `ccx-brief.py` | Byte-stable brief emission: global policy + task contract + neighbor contracts, a pure function of the input files |
| `ccx-lint.py` | Contract lint, six rule families; a contract that fails lint never reaches an agent |
| `ccx-blast.py` | Blast-radius check over a unified diff (statement-aware facade allowance) or a forge JSON envelope |
| `run-task.sh` | Dependency-ordered runner: lint preflight → stacked detached-HEAD base → fresh agent session → halt-on-unknown → blast postflight |
| `verify-task.sh` | Independent acceptance re-verification on a rebuilt base, fix set vs guard set |
| `CONTRACT-SCHEMA.md` | Normative `ccx.contract.v1` description |
| `UNKNOWN-TRIAGE.md` | The stop-on-unknown convention and the triage flow |
| `prompts/task-instruction.txt` | The single-sourced stop rule appended to every brief |

**Prerequisite:** Python 3 with PyYAML (`python3 -m pip install pyyaml`).
Every tool that reads contract YAML fails closed with that instruction if
the import is missing.

## Two things this harness is not

- **A merge gate.** Contract acceptance green licenses *integration* of a
  task's output into the stack — never merge. The `/ce-code-review` gate
  stays non-optional (gate layering: acceptance, independent
  re-verification, adversarial review, and CI catch disjoint failure
  classes).
- **A failure tribunal for stops.** A run that halts with a well-formed
  UNKNOWN.md is a SUCCESS outcome. Score it that way. See
  `UNKNOWN-TRIAGE.md`.

## Lifecycle

```
author contract (v1)             ccx-lint.py contracts/task.yaml
        │                              │  errors? fix the contract first
        ▼                              ▼
run-task.sh --clone <scratch-clone> --base <ref> \
    --contracts-dir contracts/ --out runs/task \
    [--stack runs/dep/patch.diff]... contracts/task.yaml
        │   lint preflight → brief + task-instruction → claude -p
        │   → collect patch.diff → UNKNOWN.md? halt (exit 2)
        │   → blast postflight (exit 3 on violation)
        ▼
verify-task.sh --clone <scratch-clone> --base <ref> \
    --contract contracts/task.yaml --out runs/task \
    [--stack runs/dep/patch.diff]...
        │   rebuild exact base → apply patch → fix set → guard set
        ▼
integration licensed; /ce-code-review before any merge
```

Chain form: `run-task.sh ... --chain contracts/a.yaml contracts/b.yaml ...`
topologically orders the tasks by `depends_on`, threads each produced
patch into its dependents' stacks, and refuses to start when a listed
task's dependency is neither in the chain nor supplied via `--stack`.

Run scratch clones live outside this repo (e.g. under `/tmp`) — never
dogfood the `forge` binary from the project root (CLAUDE.md gotcha).

## Exit codes

| Tool | 0 | 1 | 2 | 3 | 4 |
|------|---|---|---|---|---|
| `ccx-lint.py` | clean | usage/parse-internal error | findings at error severity | — | — |
| `ccx-blast.py` | within radius | usage/input error | violation | — | — |
| `run-task.sh` | all tasks ran | usage/fatal (lint error, stack apply failure) | UNKNOWN filed — chain halted for triage | blast violation — artifacts preserved, chain halted | — |
| `verify-task.sh` | fix+guard green | usage/rebuild failure | fix set failed | — | fix green but guard regressed |

`run-task.sh` exit 2 (a stop) is a success outcome pending triage; exit 3
is a real violation.

## Worked example

Fixture contracts under `tests/fixtures/` are runnable end-to-end with a
stub agent:

```bash
# lint a contract
python3 tools/ccx/ccx-lint.py --contracts-dir tools/ccx/tests/fixtures \
    tools/ccx/tests/fixtures/pass-minimal-v1.yaml

# emit its brief (byte-stable: run twice, cmp the outputs)
python3 tools/ccx/ccx-brief.py --contracts-dir tools/ccx/tests/fixtures \
    tools/ccx/tests/fixtures/pass-minimal-v1.yaml

# dry-run the runner against a scratch repo (no agent session)
tools/ccx/run-task.sh --clone /tmp/scratch-repo --base main \
    --contracts-dir tools/ccx/tests/fixtures --out /tmp/ccx-out \
    --dry-run tools/ccx/tests/fixtures/pass-minimal-v1.yaml

# check a produced patch against the contract's blast radius
python3 tools/ccx/ccx-blast.py --contract tools/ccx/tests/fixtures/pass-minimal-v1.yaml \
    --diff < /tmp/ccx-out/pass-minimal-v1/patch.diff
```

## Self-tests

```bash
tools/ccx/tests/run-tests.sh
```

Runs the python suites (stdlib `unittest` only — no pytest dependency) and
the shell tests, and fails if zero tests were collected. Wired into
`scripts/ci.sh` and `.github/workflows/ci.yml`.
