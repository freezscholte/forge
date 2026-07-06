---
title: "Gate layering: contract acceptance green is not merge-ready — acceptance, review, and CI catch disjoint failure classes"
date: 2026-07-06
category: conventions
module: compound-engineering
problem_type: convention
component: verification-gate-layering
severity: high
applies_when:
  - A change passed its declared acceptance commands (forge check gates, task-contract acceptance, self-run test suites) and someone proposes skipping the code-review gate
  - Designing verification for agent-produced changes (task contracts, forge trust/check policy, CI pipelines)
  - Interpreting experiment or pilot results where "all gates passed" is used as a quality claim
tags: [gate-layering, goodhart, acceptance-tests, code-review-gate, verifier-defect, ce-code-review, contract-pilot, ner-382]
---

# Contract acceptance green is not merge-ready

## Context

In the 2026-07-06 contract pilot + NER-382 promotion arc, the drift-guard
implementation passed every gate available to it: 11/11 contract
acceptance commands re-run independently on rebuilt bases, the full
`scripts/ci.sh` (fmt, workspace tests, clippy, e2e eval), AND two
independent blinded scorers who rated the patch near-perfect. The layered
`/ce-code-review` gate (8 personas + per-finding validators) then found 15
findings — every one sent to validation CONFIRMED — including a P1
reproduced live against the binary (gitignored artifacts causing
permanent false drift) and a validated composition where the documented
override flag would delete private-labeled files.

## Guidance

Treat the gates as CATCHING DISJOINT FAILURE CLASSES, never as redundant
layers where one green light excuses another:

- **Acceptance commands** verify what the spec's author thought to check.
  They are blind to everything the spec was silent about (here: ignore
  semantics, private-label composition, crash windows).
- **Independent re-verification** (re-running gates on rebuilt bases)
  catches self-report drift and Goodhart-by-accident — but only within
  the same command set.
- **Layered adversarial review + per-finding validation** catches
  spec-silence failures: composition across features, abuse loops,
  crash-ordering, platform divergence. This is where all 15 findings came
  from.
- **CI** is the post-merge backstop, never a substitute (already repo
  law in CLAUDE.md — this learning adds the evidence).

Corollary for contract-driven work: "contract green" licenses
INTEGRATION of a task's output into the stack; only the review gate
licenses MERGE. Do not weaken CLAUDE.md's two non-optional gates on the
argument that contracts/acceptance already passed.

## Why This Matters

Five Goodhart cases were logged in one day (acceptance passing while
intent was violated, including one vacuous test filter that matched zero
tests). The failure mode is seductive precisely because everything is
green — the review gate's cost (~1h wall, ~10 subagents) bought a
reproducible P1 and a private-data-deletion hazard before they reached
main of a public repo.

## When to Apply

Every non-trivial change, and ESPECIALLY changes whose tests were written
by the same process that wrote the code (agent-produced patches with
self-authored acceptance). The more gates a change already passed, the
more suspicious "skip the review" becomes.

## Examples

Evidence trail: `experiments/ccx/RESULTS.md` (verifier-defect class),
review run `/tmp/compound-engineering/ce-code-review/20260706-145749-963e80e5/review.json`
(15/15 validated findings after 11/11 green gates), PR #123 commits
158cc65 (gates-green with P1s) → bc2ea57 (post-review fixes).
