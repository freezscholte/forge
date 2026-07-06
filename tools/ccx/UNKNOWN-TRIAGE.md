# UNKNOWN.md — Stop Convention and Triage Flow

The stop-on-unknown gate converts agent hallucination pressure into typed
signal. It has three legs; **it fails if any leg is missing** (see
`docs/solutions/design-patterns/stop-on-unknown-gate-for-agent-briefs.md`).

## Leg 1 — the mechanical stop convention

The rule is carried verbatim in `prompts/task-instruction.txt`, appended to
every brief by the runner:

> If the brief does not license a decision you need to make, STOP: write
> UNKNOWN.md at the repo root (what you need to know, why the brief does
> not answer it, your best guess of kind: blocking/assumption/observation,
> file:line evidence) and end without making further edits.

The rule names a concrete mechanical act — write a specific file, end the
session — not "ask if unsure", which agents ignore under completion
pressure. The wording is validated by 13/13 observed stops in the pilot
(8/8 on the accidental no-brief negative control, 4/4 on missing code
dependencies, 1 real contract contradiction). Do not editorialize it.

An UNKNOWN.md must state:

1. **What** is needed to proceed.
2. **Why** the provided brief/contract does not answer it.
3. **Kind** (best guess): `blocking` / `assumption` / `observation`.
4. **Evidence**: `file:line` references.

The runner (`run-task.sh`) enforces the halt: when a run leaves UNKNOWN.md
in the clone, the artifact is copied to the run's out dir, the whole chain
stops (exit 2), and nothing dependent executes.

## Leg 2 — stops are scored as SUCCESSES

A correct stop is a success outcome wherever runs are tallied — in run
logs, experiment scoring, and status reports. If stops are recorded as
failures anywhere in the loop, agents learn to guess instead. (Pilot
protocol §B2.3: "first-run unknown surfacing is a success signal.")

## Leg 3 — the triage flow (an unanswered unknown teaches guessing)

Every filed UNKNOWN.md is answered **before any rerun of that task or its
dependents**. Never rerun into an unanswered unknown.

For each filed unknown, the contract author does one of:

- **Contract revision** (the normal path): bump `revision:` in the contract
  and record the resolution as a comment block beside the bump — what the
  unknown was, which run filed it, and how this revision resolves it.
  Model: `experiments/ccx/contracts/task-382-2-drift-guard.yaml` rev 2.
- **Explicit rejection**: when the unknown rests on a misreading and the
  contract already licenses the decision, record the rejection the same
  way (comment beside a revision bump quoting the licensing text). A
  rejection without an identifiable contract clause is a smell — if the
  agent couldn't find the license, the next one won't either; prefer
  revision.

Then rerun the task fresh (new session, revised brief). Post-resolution
reruns in the pilot shipped clean.

This minimal convention (append-only comment header in the revised
contract) is deliberate v0; revisit after the first real triage
(plan: `docs/plans/2026-07-06-001-feat-ccx-thin-harness-plan.md`, Open
Questions).
