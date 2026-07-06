# CCX Contract Schema — `ccx.contract.v1`

Normative description of the task-contract format consumed by the CCX thin
harness (`ccx-brief.py`, `ccx-lint.py`, `ccx-blast.py`, `run-task.sh`,
`verify-task.sh`). v1 is additive over the pilot's `ccx.contract.v0`
(`experiments/ccx/contracts/`); v0 compatibility rules are at the end.

A contracts directory contains one YAML file per task contract plus one
global-policy file (`_global-policy.yaml`, `kind: global_policy`) that is
prepended to every brief.

## Top-level keys (task contract)

| Key | Required | Meaning |
|-----|----------|---------|
| `schema` | yes | `ccx.contract.v1` (or legacy `ccx.contract.v0`) |
| `id` | yes | `ccx-<name>`; the file must be `<name>.yaml` in the contracts dir (neighbor/depends_on resolution relies on this) |
| `revision` | yes | Integer, bumped on every contract revision; resolutions of filed unknowns are recorded as comments beside the bump (see `UNKNOWN-TRIAGE.md`) |
| `ticket` | yes | Tracking id (e.g. `NER-362`) |
| `task` | yes | One-line task statement |
| `interface` | yes | The normative implementation surface: what to build, where, and the behavioral contract |
| `invariants` | no | Properties that must hold; treated as normative text in the brief |
| `acceptance` | yes | Fix/guard command sets — see below |
| `negative_constraints` | no | Rules with `scope`/`reason`/`source_evidence`, as in v0 |
| `neighbors` | no | List of contract ids (`ccx-<name>`) emitted after the task contract in the brief, in declared order |
| `depends_on` | no | List of contract ids whose produced patches must be stacked beneath this task; drives the runner's chain ordering. The graph must be acyclic |
| `primitives` | no | List of `{name, crate, visibility}` entries naming the code primitives the interface relies on; lint verifies existence and visibility |
| `exclusion_contract` | conditionally | Free text naming the exclusion semantics (policy / `.forgeignore` / `.gitignore`) or the owning walk primitive. **Required by lint whenever the contract text touches filesystem enumeration** (see lint rule 5) |
| `allowed_changes` | yes | `paths` (globs, non-empty), optional `forbidden_paths`, optional `public_api_change_policy` |
| `authority` | yes | `{source, confidence, reviewer}` as in v0 |

## `acceptance` — fix set / guard set

```yaml
acceptance:
  fix:                    # must pass; the task's own proof
    - cargo test -p forge-cli --test forge_blame
  guard:                  # must not regress; pre-existing behavior
    - cargo test -p forge-store
    - cargo clippy --workspace --all-targets -- -D warnings
```

- `fix` failures mean the task is not done (verify exit 2).
- `guard` failures with fix green mean the task works but regressed
  something (verify exit 4) — mechanically distinguishable on purpose.
- Every entry must match the command grammar `cargo
  (test|clippy|fmt|build|run) ...` (lint rule 6): these strings are
  executed by the verifier with the operator's shell privileges, so the
  eval surface stays constrained to a reviewable command family.

## `primitives`

```yaml
primitives:
  - { name: tree_fingerprints, crate: forge-content-native, visibility: pub }
```

Lint (rule 3) greps the named crate for the symbol definition (error if
absent) and errors when the actual visibility qualifier fences the
primitive off from the contract's consumers (any `allowed_changes.paths`
outside the owning crate cannot see a `pub(crate)` item). This transcribes
the NER-382 defect where a `pub(crate)` `tree_fingerprints` caused a
hand-built second walker — see
`docs/solutions/architecture-patterns/filesystem-enumeration-shared-exclusion-contract.md`.

## `exclusion_contract`

```yaml
exclusion_contract: >
  enumerate via walk_worktree semantics (policy + .forgeignore + .gitignore,
  rooted at the scanned dir)
```

Required whenever `interface`/`task`/`negative_constraints` text matches
filesystem-enumeration signals (`read_dir`, `walk`, `enumerate`, `scan`,
`drift`, `workspace files`, `directory tree`). Naming an owning walk
primitive in `primitives:` (e.g. `walk_worktree`) also satisfies the rule.
This transcribes the drift-guard P1: a contract silent on ignore semantics
licensed a walker with weaker exclusion semantics than every sibling
surface.

## `depends_on` vs `neighbors`

- `neighbors` shapes the **brief** (which other contracts the implementer
  reads); it has no execution-ordering meaning.
- `depends_on` shapes the **run** (whose patches are stacked first); it has
  no brief meaning. A task typically lists its `depends_on` contracts among
  its `neighbors` too, but the harness does not require it.

## v0 compatibility

The frozen pilot contracts (`experiments/ccx/contracts/`) remain readable
by every tool:

- A flat `acceptance:` list is treated as the fix set with an empty guard
  set (warning).
- On `schema: ccx.contract.v0`, all lint rule 2–6 findings downgrade to
  warnings; errors are reserved for v1. This guarantees the frozen pilot
  record lints with zero errors while new v1 contracts get full strictness.
- v0 contracts have no `primitives`/`exclusion_contract`/`depends_on`;
  the corresponding checks degrade to best-effort warnings.
