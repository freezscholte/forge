# Handoff: CCX thin harness (next session)

Date: 2026-07-06 · From: contract-pilot session (context ~60%+, wrapping)
Owner: Jan Skolte · Repo: forge (PUBLIC — only Jan-approved merges to main)

## Where things stand (all verified, nothing in-flight)

- **Merged to main:** PR #123 — NER-382 drift guard + review fixes
  (b82f244). NER-382 is Done in Linear.
- **Awaiting Jan's review:** PR #124 — the complete experiment record
  (branch `experiment/ccx-spikes`, 20 commits: brainstorms v2/v3,
  landscape doc, pilot protocol/rubric/contracts, all run data,
  RESULTS.md, 3 solution docs). Docs-only.
- **Pilot outcome:** STRONG pre-registered reading met
  (`experiments/ccx/RESULTS.md` — read the Validity caveats before
  quoting). Two blinded scorers, 8/8 directional agreement.
- **Open tickets:** NER-383 (4 drift-guard semantics decisions, defaults
  proposed — Jan's call, blocks nothing). NER-362 (intent-aware blame) —
  implementation EXISTS as pilot Arm A patches
  (`experiments/ccx/runs/A-362-*/patch.diff`, stacked order 362-1 →
  362-2-r2 → 362-3-r2 → 362-4-r2 → 362-5-r2, gates verified) but is
  HELD: the 362-3 contract pinned blame's tip resolution to the native
  HEAD ref while repo convention resolves the authoritative tip from the
  ledger (contract defect, scorer-confirmed). Needs a contract revision +
  targeted fix + its own promotion round (full gate stack incl.
  /ce-code-review — see gate-layering solution doc for why that is
  non-negotiable).

## Next objective: build the thin harness (v3 roadmap item 1)

Requirements source: `docs/brainstorms/2026-07-06-context-closed-tasks-v3.md`
§2 (seven earned commitments) + §4 item 1. Productize the pilot's duct
tape into a small, Git-compatible toolkit (files + scripts — deliberately
NO new Forge objects yet; that is the decision rule's explicit boundary):

1. **Brief emitter** — from `experiments/ccx/brief.sh`: byte-stable
   (canonical ordering, no timestamps — verified requirement), neighbor
   resolution (mind the BSD-sed `[[:space:]]` lesson), fail-closed when a
   contract is missing.
2. **Contract lint v0** (earned, v3 §2.5): YAML-parseable; allowed_changes
   non-empty AND satisfiable (the 4730-line-cap contradiction); referenced
   primitives exist AND are visible (pub vs pub(crate) caused two
   defects); acceptance commands match ≥1 test (vacuous-filter Goodhart);
   exclusion clause present for any filesystem-enumeration task (the P1).
3. **Blast-radius check** — from `experiments/ccx/blast-check.py`, plus
   the standing facade allowance (decl/re-export lines in facade files
   always permitted — v3 §2.4).
4. **UNKNOWN.md convention + triage flow** — the stop rule verbatim from
   `run-arm-a.sh`, plus a triage step (an unanswered unknown teaches
   guessing — see the stop-on-unknown solution doc).
5. **Dependency-ordered runner** — from `run-arm-a-stacked.sh`: stack
   predecessors' patches, committed on a DETACHED head (the pilot-run
   branch-pointer bug), `--3way` apply, halt-on-unknown.
6. **Acceptance fix/guard split** (earned, v3 §2.3): must-fix commands +
   must-not-regress commands per contract.

Process: run `/ce-plan` with v3 as input → doc-review gate → `/ce-work`.
Dogfood target after the harness: NER-362 completion THROUGH it (contract
revision flow is itself the thing to exercise).

## Gotchas the fresh session must know

- Public repo; commit docs/experiments to branches; Jan approves merges.
  (Memory file: forge-main-approval-public-repo.)
- Dogfood `forge` binary ONLY in /tmp throwaway repos or a temp clone of
  `~/Github-Private/forge-dogfood` — NEVER from the project root.
- Verify trio + `rtk bash scripts/ci.sh` before any push; the
  code-review gate is non-optional (fresh evidence: the gate found a
  live-reproducible P1 after 11/11 acceptance gates passed —
  `docs/solutions/conventions/contract-acceptance-is-not-merge-ready.md`).
- `forge-content-native/src/lib.rs` is allowlisted at exactly 4730 lines
  and MUST NOT GROW — new code goes in new module files.
- grep the three new `docs/solutions/` docs before designing anything
  walker-, gate-, or unknown-shaped.
- Lineage debt U4: the 2026-07-04 verified-handoff brainstorm is still
  uncommitted; commit or formally drop the reference before v4 cites it.

## Queue after the harness (v3 §4)

NER-362 completion → pilot 2 (neighbor ablation, next real feature) →
pilot 3 (CooperBench, ~$30-75, feasibility note in
`experiments/ccx/T3-cooperbench-feasibility.md`) → substrate build →
publish decision (Jan).
