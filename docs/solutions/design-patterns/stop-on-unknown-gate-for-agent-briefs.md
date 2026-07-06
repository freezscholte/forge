---
title: "The stop-on-unknown gate: converting agent hallucination pressure into typed signal"
date: 2026-07-06
category: design-patterns
module: experiments/ccx
problem_type: design_pattern
component: agent-brief-unknown-convention
severity: high
applies_when:
  - Designing prompts/briefs for autonomous implementation agents where guessing is costlier than halting
  - An agent task depends on artifacts (code, contracts, decisions) that may be missing or contradictory at run time
  - Evaluating agent runs — a stop must be scoreable as a SUCCESS, not a failure, or the incentive collapses
tags: [unknown-gate, stop-on-unknown, incentive-inversion, hallucination, agent-briefs, contract-pilot, unknown-md, ccx]
---

# The stop-on-unknown gate

## Context

The contract pilot (2026-07-06, `experiments/ccx/`) gave fresh
implementation agents a brief plus one rule: "If the brief does not
license a decision you need to make, STOP: write UNKNOWN.md at the repo
root (what you need, why the brief doesn't answer it, kind:
blocking/assumption/observation, file:line evidence) and end without
further edits." The hypothesis (v2 brainstorm A4.4): an unknown is an
unconstrained region the model otherwise fills with plausibility;
surfacing must beat guessing.

## Guidance

Implement it as an incentive-inverted STOP CONVENTION, not an instruction:

- The rule names a concrete mechanical act (write a specific file, end
  the session) — not "ask if unsure," which agents ignore under
  completion pressure.
- The protocol scores a correct stop as a SUCCESS outcome (v2 §B2.3
  "first-run unknown surfacing is a success signal"). If stops are scored
  as failures anywhere in the loop, agents learn to guess.
- Required content: what is needed, why the provided context doesn't
  answer it, best-guess kind (blocking / assumption / observation), and
  file:line evidence — this makes the stop triageable in minutes.
- Pair with a fail-closed harness: a broken/missing brief should produce
  stops, not improvisation (and did — see below).

## Why This Matters

Observed compliance across the pilot, with zero silent guesses found by
two independent blinded scorers:

- 8/8 fresh agents stopped when a harness bug delivered prompts with NO
  contract at all (accidental negative control, ~$1/run).
- 4/4 stopped on missing code dependencies (clean-base runs of dependent
  tasks) with precise statements of what was absent.
- 1 agent, six minutes into implementation, discovered a REAL
  contradiction between its contract and the codebase (the rev-1 382-2
  contract mandated `diff_working_vs_tree`, which writes a status cache,
  while also mandating a read-only check) and filed a blocking unknown
  citing the exact mechanism — converting what would have been a silent
  design coin-flip into a contract revision. Post-resolution, one fresh
  rerun shipped clean.

Contrast: the continuous-session arm (no stop affordance) silently
substituted a different ticket's work for one task and adapted tests to
divergent names rather than flagging them — deviations discovered only at
review.

## When to Apply

Any brief/prompt for an autonomous implementation run, and any experiment
harness measuring agent quality. The gate needs three legs or it fails:
mechanical stop convention + stops-scored-as-success + triage flow that
actually answers the unknowns (an unanswered unknown teaches guessing).

## Examples

Pilot artifacts: `experiments/ccx/run-arm-a.sh` (the rule verbatim in the
task instruction), `experiments/ccx/runs-invalid-01-nobrief/` (8/8
negative control), `experiments/ccx/runs/A-382-2/UNKNOWN.md` (the
contract-contradiction stop), RESULTS.md §Unknown flow.
