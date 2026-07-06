---
date: 2026-07-06
version: 3
topic: context-closed-tasks
status: post-pilot design state
origin: Jan Skolte × Claude (Fable 5); consolidates v2, the landscape doc, and the Part B pilot outcome
supersedes: >
  2026-07-05-context-closed-tasks-v2.md §A (design) is updated by this doc;
  v2 Part B (pilot protocol) remains the canonical protocol reference and is
  NOT superseded. The landscape doc (2026-07-06-context-closed-tasks-
  landscape-and-substrate.md) remains the research record.
evidence: experiments/ccx/RESULTS.md (branch experiment/ccx-spikes)
artifact_readiness: design-current; build input for the thin harness
---

# Context-Closed Tasks v3 — What the Pilot Proved, Changed, and Earned

## §1 Thesis, post-pilot

The v2 thesis stands and now has empirical rails: **agent context managed as
contract revisions beats history transfer on decomposable implementation
work.** Pilot (N=8 tasks, 2 arms, blinded scoring, pre-registered): Arm A
(fresh sessions, ~2k-token byte-stable briefs) — 0 implementation defects,
3 unlicensed decisions, 8/8 delivered-as-specified, 100% surfaced-vs-guessed;
Arm B (continuous full-context) — 21+ implementation defects, 40 unlicensed
decisions, one silent task substitution. Both scorers agreed directionally
8/8. Every Arm A defect was in the **contracts**, not the implementations —
risk concentrated into the decomposer exactly as A-CORE predicted. STRONG
reading met; decision rule → thin harness + pilot 2. Caveats recorded in
RESULTS.md §Validity (same-model scorers, N=8, one repo, author=operator).

## §2 New design commitments (earned today, not speculated)

Per §A9.1 discipline, each is backed by a named defect/incident:

1. **Exclusion/environment clause on contracts.** The shipped drift guard
   carried a live-reproduced P1 because the contract was silent on ignore
   semantics — the implementer hand-built a walk with weaker exclusion
   semantics than every sibling surface. Contracts that touch filesystem
   enumeration must state the exclusion contract (policy/.forgeignore/
   .gitignore) or name the primitive that owns it.
   (Earned by: review finding #1, run 20260706-145749-963e80e5.)
2. **Gate layering is a model, not an option.** Contract acceptance
   (self-run commands) / independent re-verification / layered adversarial
   review / CI catch DISJOINT failure classes: 11/11 gates and two blinded
   scorers missed a P1 the persona review reproduced in an hour. Contract
   green licenses integration, never merge. (Earned by: the entire NER-382
   promotion arc.)
3. **Fix set / guard set for `acceptance`** (steal-list item, now earned):
   five Goodhart cases logged where acceptance passed while intent was
   violated, incl. one vacuous test filter. Acceptance needs a must-fix set
   and a must-not-regress set. (Earned by: 4 Arm B verifier defects + B's
   vacuous `-p forge-store provenance` filter.)
4. **Facade/wiring allowance in `allowed_changes`.** Every real slice that
   adds a module needs the facade decl/re-export line; contracts that omit
   it force either a violation or an unlicensed edit. Standing allowance:
   facade files, decl+re-export only. (Earned by: A-382-2 blast violation +
   both arms' cap-collision workarounds on 362-1.)
5. **Contract lint.** My contracts weren't valid YAML; one was
   unsatisfiable (file at exactly its line cap + "add lines, don't grow").
   Lint battery v0: parseable; allowed_changes non-empty and satisfiable;
   referenced primitives exist AND are visible (pub vs pub(crate) — the
   382-2 rev-1 contradiction AND the hand-parse both trace to a fenced
   pub(crate) primitive); acceptance commands match ≥1 test.
   (Earned by: scorer contract-defect findings, all five.)
6. **Dependency stacking is a harness primitive, not an afterthought.**
   Clean-base runs of dependent tasks are physically impossible; the P1
   amendment (stack predecessors' patches, committed, detached HEAD) is now
   protocol. The clean-base failure mode doubles as a cheap
   unknown-surfacing probe. (Earned by: pilot amendment P1.)
7. **Byte-stable briefs** confirmed as design requirement (v2 §7.3):
   verified `cmp`-identical emissions; keep canonical serialization
   mandatory from day one.

## §3 What did NOT survive contact

- **"Contract-author familiarity makes authoring cheap"** — the ~45-min
  figure is a floor; and familiarity did not prevent five contract defects.
  Authoring quality, not authoring speed, is the cost center.
- **"Blast radius as pure allowlist"** — needs the standing facade
  allowance (§2.4) and, per NER-383, semantics decisions where refusal
  fires; a bare path list under-specifies.
- **v2's implicit "acceptance green ≈ done"** — replaced by §2.2 layering.

## §4 Updated roadmap (decision-rule compliant)

1. **Thin harness** (files+scripts, Git-compatible, no Forge objects):
   productize `brief.sh` (+neighbor graphs), `blast-check.py`, the
   UNKNOWN.md stop convention + triage flow, dependency-ordered runner
   (from run-arm-a-stacked.sh), contract lint v0 (§2.5), and the
   acceptance fix/guard split (§2.3).
2. **NER-362 completion** through that harness: tip-resolution contract
   revision (the 362-3 contract defect), affected reruns, promotion round
   with the full gate stack. Dogfoods post-pilot revision flow.
3. **Pilot 2 — neighbor ablation** (pre-registered, v2 §A9.3) on the next
   real feature.
4. **Pilot 3 — CooperBench** (~10 typst pairs × 3 arms, ~$30–75; T3
   feasibility note): contracts vs. NL chat, Shepherd's supervisor as the
   reference point.
5. **Substrate build** (Forge-native contract records, forge brief/unknown,
   merge gate) only after 1–4; the substrate map (landscape §4) stands.
6. **Publish decision** (Jan): RESULTS.md + this arc as a public writeup.

## §5 Open items carried forward

- U4: verified-handoff doc still uncommitted (lineage debt before v4 cites
  it).
- U5: harness-side injection surface — prototype exists (pilot runner);
  design the real one in roadmap item 1.
- U6: brief cost at neighbor-graph scale — measure in pilot 2.
- NER-383: four refusal-semantics decisions (defaults proposed).
- A6.10 review-fatigue mitigations: unexercised (no forge compact yet);
  the replay-evidence idea (landscape §5.3) remains the strongest candidate.
- Inter-rater with a HUMAN scorer: still valuable; both scorers were
  same-model-family agents (recorded caveat, not resolved).
