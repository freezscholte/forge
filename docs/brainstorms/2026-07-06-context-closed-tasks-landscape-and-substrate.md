---
date: 2026-07-06
version: 1
topic: context-closed-tasks — landscape check + substrate grounding
status: research-notes (feeds v3 of the context-closed-tasks brainstorm)
origin: Jan Skolte × Claude (Fable 5) research session, 2026-07-06
relates_to: docs/brainstorms/2026-07-05-context-closed-tasks-v2.md
inputs: >
  Shepherd paper (arXiv 2605.10913v3, shepherd-agents.ai); CooperBench paper
  (arXiv 2601.13295, cooperbench.com); code-level exploration of the Forge
  workspace (forge-store, forge-policy, forge-evidence, forge-content-native)
artifact_readiness: research-input; spike protocol in §7 is executable
---

# Context-Closed Tasks — Landscape Check and Substrate Grounding

Two questions drove this session: (1) does **Shepherd** (Stanford/Northeastern
meta-agent substrate) overlap with or invalidate the context-closed-tasks
thesis, and (2) how much of the v2 design's machinery already exists in the
Forge codebase, verified at the code level rather than assumed. A third
question emerged mid-session: is **CooperBench** a proper benchmark for
testing our solution?

**Verdicts, upfront:**
- Shepherd is **orthogonal, not competing** — it versions the *process*
  (execution traces); we version the *specification* (contract revisions).
  Its related-work section strengthens the §A2 wedge claim: the entire
  "VCS-for-agents" cluster is crowding the trace/checkpoint layer; nobody is
  on the contract/merge-validity layer.
- CooperBench is a **good benchmark for one slice** of the thesis (typed
  coordination artifacts vs. natural-language coordination) and a **wrong
  benchmark for the core bet** (A-CORE decomposition quality) — its tasks
  arrive pre-decomposed, which removes the load-bearing variable of the
  Part B pilot. Candidate for pilot 3, not a replacement for Part B. See §3.
- The Forge substrate is **further along than v2 assumed**: revision
  anchoring, digest+sign+op-chain patterns, and per-revision changed-path
  persistence already exist. Blast-radius checking is nearly free. The
  genuine gaps are session/brief lifecycle, evidence hermeticity fields, the
  reverse index, and any predicate language beyond command gates. See §4.

---

## §1. Shepherd: what it is

*Shepherd: Enabling Programmable Meta-Agents via Reversible Agentic Execution
Traces* (Yu, Chong, Nandi, Soylu, Sun, Manning, Shi — Northeastern + Stanford
NLP; arXiv 2605.10913, v3 2026-06-24; open-source Python, `pip install
shepherd-ai`).

Every model call, tool call, and environment mutation becomes a structured
event in a **reversible, Git-like execution trace**. Meta-agents (agents that
manage other agents) get six verbs: create, observe, intercept, revert, fork,
replay. Filesystem/process/scope state is captured atomically via
copy-on-write; forking restores **byte-identical** state. Three demonstrated
uses:

| Use case | Mechanism | Result |
|---|---|---|
| Supervisor over parallel coders | LLM meta-agent watching live effect streams; tools: `inject` / `handoff` / `discard` | CooperBench 28.8% → 54.7% |
| Counterfactual workflow repair (CRO) | Proposer emits candidate edits + **fix set** + **guard set**; fork at first affected commit, replay suffix | +12.8% vs. MetaHarness on Terminal-Bench 2.0, 58% less wall-clock |
| RL training meta-agent | Fork-point selection for credit assignment | 2× GRPO uplift |

Stated limitations (their Appendix A.1): proof-of-existence framing, unresolved
supervision-token-cost tradeoff, and counterfactual replay assumes **weak
coupling** between edits and side effects.

## §2. Axis analysis: process vs. specification

Shepherd and context-closed tasks rhyme superficially ("Git-like",
"first-class objects", changes "held as a proposal") but sit on opposite axes.

**2.1 Opposite context-management bet.** A forked/resumed Shepherd agent
receives its **byte-identical prior message prefix** — deliberately, for >95%
KV-cache reuse. There is no compilation, summarization, or constraint
extraction anywhere in the paper. Shepherd's answer to context is *make
history perfectly cheap to keep and rewind*; v2's thesis is *compile history
into typed artifacts and discard the narrative*. Shepherd does not engage
with our question at all: context still grows monotonically in their world;
they make replaying it efficient.

**2.2 Coordination via LLM judgment, not declared scope.** Their supervisor
is an Opus/Sonnet meta-agent subscribed to both workers' effect streams,
reactively choosing inject/handoff/discard. **No path-overlap detection, no
declared scopes, no blast radius.** The headline coordination result is
bought with continuous frontier-model supervision: expensive,
nondeterministic, and producing no durable artifact. `allowed_changes` is the
deterministic, near-free version of the same protection (and per §4, Forge
already persists per-revision changed paths).

**2.3 No contracts, invariants, negative constraints, or typed unknowns.**
Closest analogs in the paper: typed task signatures ("a task is fully
specified by its signature and docstring"), a **reversibility tier** on
effects (reversible / compensable / irreversible), and CRO's fix/guard sets.
Acceptance in CRO is purely score-gated — no human review, no revision-bound
evidence objects, no authority/confidence model.

**2.4 Their related work confirms the wedge.** Shepherd positions against
AgentGit (VCS operations as agent-invocable tools), BranchFS (kernel-level
filesystem branching), AgentSPEX (checkpointing DSL), OpenHands (event
streams). The entire cluster is on the **trace/checkpoint layer**. Nobody in
that citation graph makes *change validity a merge-time property of
contract-bound evidence* — exactly the §A2 wedge. **v3 action:** add
Shepherd + AgentGit + BranchFS + AgentSPEX to §A2's related work with the
process-axis vs. specification-axis framing.

**2.5 Counterargument to record (so v3 doesn't dodge it):** Shepherd's
supervisor result *is* evidence that reactive LLM supervision works — one
could argue contracts are unnecessary if supervision gets cheap enough.
Rebuttal, three-fold: (a) **cost** — supervision is a per-run recurring
frontier-model spend; a contract is authored once and enforced for free;
(b) **determinism** — a gate either fires or doesn't; a supervisor
sometimes notices; (c) **durability** — supervision leaves nothing behind;
a contract violation caught becomes a permanent negative constraint. The
honest version of the rebuttal concedes these are *complementary*: reactive
supervision covers the unclosable remainder (C0/C1 work) that contracts
never will.

## §3. CooperBench: is it a proper test for us?

*CooperBench: Why Coding Agents Cannot be Your Teammates Yet* (Stanford + SAP
Labs; arXiv 2601.13295; cooperbench.com; a Harbor adapter exists). 652 tasks
across 12 OSS libraries in Python, TypeScript, Go, **and Rust**. Each task
gives two agents different features that are logically compatible but
conflict at the code level; workspaces are isolated; coordination is
restricted to natural language (Redis channel). Headline finding: the
**curse of coordination** — ~30% average success drop when two agents
cooperate vs. one agent doing both tasks; GPT-5 and Sonnet 4.5 reach only
~25% success cooperating.

**3.1 The failure taxonomy maps almost one-to-one onto our primitives.**
This is the strongest reason to take the benchmark seriously:

| CooperBench failure class | Share | Contract-substrate answer |
|---|---|---|
| Expectation failures — wrong beliefs about partner state/plans | 42% | Neighbor contracts in the brief: partner's interface + blast radius are *declared*, not inferred |
| Commitment failures — agents break promises, unverifiable claims | 32% | A contract **is** an enforced commitment; blast-radius + evidence gates make claims verifiable at merge time |
| Communication failures — questions unanswered, jammed channel | 26% | `forge unknown` replaces free-form chat with typed, gated asks; agents spend up to 20% of budget on NL communication that CooperBench shows doesn't improve success |

Read that way, CooperBench's empirical result is an argument *for* typed
coordination artifacts over natural-language coordination — the paper itself
concludes the channel jams with repetition, unresponsiveness, and
hallucination.

**3.2 What CooperBench tests — and what it cannot.**
- **Tests well:** the coordination/enforcement slice — do declared blast
  radii + neighbor contracts + typed unknowns beat NL chat (their baseline)
  and beat/approach an LLM supervisor (Shepherd's 54.7%) at a fraction of the
  cost? Deterministic contracts vs. Opus supervisor on the same benchmark
  would be a striking, publishable comparison.
- **Cannot test:** **A-CORE.** CooperBench tasks arrive *pre-decomposed* by
  the benchmark authors — decomposition quality, the load-bearing unproven
  assumption of v2, is held constant by construction. It also cannot test H1
  (brief-only fresh sessions vs. continuity — its sessions are single-shot)
  or H2 (context growth across a task *sequence* — its tasks are
  independent).
- **Practical frictions:** two-agent concurrent harness + Redis channel to
  integrate; results comparable to published numbers only with matched
  models/scaffolds; 652 tasks is real compute spend.

**3.3 Verdict.** CooperBench is a **proper benchmark for pilot 3** (the
coordination claim), sequenced *after* Part B (decomposition + brief value)
and pilot 2 (neighbor ablation). It must not displace Part B: passing
CooperBench with contracts would prove enforcement value on pre-decomposed
tasks while leaving the central thesis — that we can decompose real work
into contract-closed tasks at acceptable cost — untested. A cheap
**recon spike** (§7, T3) is justified now; a full run is not.

## §4. Substrate grounding: what the Forge codebase already has

Code-level exploration (2026-07-06) of forge-store (~19k LOC + 21
migrations), forge-content-native (~5.4k), forge-evidence (~1.4k),
forge-policy (811), forge-cli (~5.9k). Summary of v2-relevant findings:

| v2 feature | Existing substrate | Genuinely new work |
|---|---|---|
| Contract records (typed, revision-bound) | Revision anchor exists (`proposal_revisions`; decisions/checks/publications already FK to it). Established new-record-kind pattern: numbered migration + domain-separated digest tag (`integrity.rs`, e.g. `forge.evidence.v0\0`) + op-kind string + Ed25519 signing — embargo/visibility/org-governance all followed it | New table + digest tag + op kind; there is no generic `RecordKind` enum to extend — each kind is a bespoke (but mechanical) slice |
| Merge rule (§A2 target end state) | `enforce_trust_policy` (`trust.rs:725`) is the exact structural template: open repo → read policy → verify subjects for revision → typed error. Trust ladder (6 rungs) shows how ordered gating extends | A parallel `enforce_contract_policy` following the same shape |
| Blast radius / `allowed_changes` | `changed_paths_json` persisted per snapshot **and** per proposal revision since migration 001; full native diff engine with rename detection; secret-risk filtered | Just the membership predicate + where the allowlist is declared. No diff plumbing needed |
| Hermetic evidence (A4.6) | ~40% there: evidence digest already binds tree/snapshot, command, args, exit, actor, parsed outcome — tamper-evident, signed, chain-folded. `DigestWriter` designed for additive extension | Toolchain lock, env digest, runner hash are net-new digest fields + a migration. `forge-policy/src/lib.rs:27` states verbatim that verdicts do NOT bind environment/cwd/executable — the gap is documented, not accidental |
| `forge brief` | `forge review` (691 LOC) is a deterministic, no-LLM, aggregated read-only surface — the right skeleton — but proposal-scoped, not task/session-scoped. `intents.check_spec_json` is today's only constraint-carrying metadata | Session/task-scoped emission is net-new; concatenation semantics can reuse the review builder pattern |
| `forge compact` / `forge unknown` | Op-log + `views.state_json` is the idiomatic place to record lifecycle events; id-minting (`new_id(prefix)`) trivial | Entirely new commands, tables, type enums. No session lifecycle exists at all |
| Reverse index (§A8) | All source data persisted (changed paths, evidence ids, revision ids) — backfillable | New file-keyed join table; today paths live in opaque JSON blobs, O(scan) to query |
| Gate/predicate language | `forge-policy` verdict engine: per-gate `Passed/Failed/Missing/Stale`, snapshot-bound, latest-evidence-wins, re-evaluated in-transaction at accept | `Gate` is a flat command struct, not an enum — blast-radius/contract-satisfied gates need a new variant or a parallel eval pass |

**Implication for sequencing:** v2's "pilot needs zero Forge changes" holds
(YAML + `brief.sh`; blast-radius violations countable with plain `git diff`).
And the post-pilot build cost is *lower* than v2 assumed, because the
digest/sign/revision-anchor machinery and changed-path plumbing already
exist. The two load-bearing reuse points: the `integrity.rs`/`signing.rs`
record-kind pattern, and the forge-policy revision-bound verdict engine.

## §5. Steal list (earned-schema candidates, per §A9 discipline)

1. **Fix set / guard set** (from CRO): split contract `acceptance` into
   *must-fix* and *must-not-regress* command sets. Maps directly onto
   forge-policy's existing per-gate verdict rollup. Earn condition: first
   pilot defect where a single acceptance command passed while regressing a
   neighbor.
2. **Reversibility tiers** (reversible / compensable / irreversible) as
   typed vocabulary for `negative_constraints.scope.operations` — "forbidden
   because irreversible" beats prose reasons.
3. **Replay evidence for compact deltas** (A6.10 mitigation #4): a proposed
   contract delta carries evidence that, with the delta present, the session
   that produced defect X halts at the right point. Converts a plausibility
   judgment into a checkable claim. Far future; note it now so review-fatigue
   mitigation design doesn't stop at rate-limiting.
4. **Structured effect streams as compact's input**: Shepherd is evidence
   that a structured event stream is a far better compilation source than a
   prose transcript. Forge's op-log + evidence records are halfway there;
   keep in mind when compact's input format is designed.
5. **CooperBench as pilot-3 vehicle** — per §3.3.

## §6. Unknowns surfaced this session (typed per A4.4)

> **Status update (2026-07-06 end of day, post-pilot):** U1 RESOLVED-yes
> (T3: shadow dataset dir, zero code). U2 partially resolved (compare our
> own arms; Shepherd = reference point only). U3 RESOLVED-positive (T2:
> attempts isolate; per-attempt blast radii compose; `attempt compare`
> carries the data). U4 still open. U5 partially answered: the pilot's
> `run-arm-a.sh` is the first working prototype of harness-side brief
> injection. U6 holds at pilot scale (byte-stable, trivial regen);
> unverified at neighbor-graph scale. Full pilot outcome:
> `experiments/ccx/RESULTS.md` (STRONG reading, 2-scorer 8/8 directional
> agreement); post-pilot design deltas in the v3 doc
> (2026-07-06-context-closed-tasks-v3.md).

- **U1 (assumption)** — "CooperBench's Harbor adapter can inject a
  per-agent brief and restrict the NL channel" — needed for any
  contracts-vs-chat arm; unverified. → T3 recon.
- **U2 (blocking, for pilot 3 only)** — which models/scaffolds make our
  CooperBench numbers comparable to the published 28.8%/54.7%? Requires
  reading their harness config, possibly contacting authors.
- **U3 (assumption)** — "Forge's competing-attempts feature composes with
  per-attempt blast radii" — two attempts under one intent with disjoint
  `allowed_changes` is the natural in-Forge analog of the CooperBench
  scenario; untested. → T2 spike.
- **U4 (observation)** — v2's `depends_on` target
  (`docs/brainstorms/2026-07-04-verified-handoff-requirements.md`) does not
  exist in the repo; the verified-handoff doc was never committed. Lineage
  gap to close before v3 cites it.
- **U5 (observation)** — the trace-layer projects (Shepherd, AgentGit,
  BranchFS) are all Python/agent-framework-side. If contract briefs win,
  the *emission* point (who calls `forge brief` and injects it) is
  harness-side and framework-specific — an integration surface v2 doesn't
  design. Park for v3.
- **U6 (assumption)** — "brief regeneration cost is negligible" — v2
  asserts briefs are regenerated fresh, never cached; fine for YAML concat,
  unverified once neighbor graphs + declassification queries exist.

## §7. Quick tests — spike protocol (branch: `experiment/ccx-spikes`)

All spikes are read-only against production code or live under
`experiments/ccx/`; none touch `crates/`. Dogfood sessions driving the
`forge` binary run **only in throwaway `/tmp` repos** (CLAUDE.md gotcha —
never `forge init` in the project root). Nothing here presumes the Part B
pilot's outcome; T1 doubly serves as Part B setup.

- **T1 — `brief.sh` + two real contracts (½ day).** Write the A4.1 minimal
  schema as YAML for two real Forge modules (candidate: `forge-policy` gate
  evaluation + `forge-evidence` capture); `brief.sh` concatenates contract +
  neighbor contracts + global policy. Deliverable: token count of a real
  brief vs. current CLAUDE.md+context baseline, and the felt authoring cost
  (A6.11 datum). This is Part B setup work — zero waste.
- **T2 — blast-radius predicate spike (½ day).** In a `/tmp` dogfood repo:
  drive `init → start → save → propose`, then a script reads the proposal
  revision's changed paths (via `forge diff`/`--json` surface) and tests
  membership against a YAML `allowed_changes`. Proves the "just a predicate"
  claim in §4 and exercises U3 with two competing attempts.
- **T3 — CooperBench recon (½ day, no compute).** Clone benchmark + Harbor
  adapter; answer U1/U2: task format, harness entry points, whether
  per-agent context injection is supported, cost estimate for a 20-task
  Rust-subset run. Deliverable: one-page feasibility note; no runs.
- **T4 — Shepherd trace inspection — DROPPED (2026-07-06).** Lowest
  information value; only relevant once `forge compact` design starts, which
  is post-pilot at the earliest. Revisit then.

### §7.1 Agreed execution sequencing (decided with Jan, 2026-07-06)

Research is at diminishing returns — the remaining unknowns (U1–U6, A-CORE)
are empirical and only move by doing. Agreed plan:

- **Step 0** — commit the v2 doc + this doc on branch `experiment/ccx-spikes`
  (not main; only Jan-approved work merges to main — this is a public repo).
- **Step 1 — T1** in the forge repo on the spike branch: contracts for
  `forge-policy` gate evaluation + `forge-evidence` capture under
  `experiments/ccx/contracts/`, plus `brief.sh`. Deliverables: real brief
  token count vs. CLAUDE.md-plus-context baseline; authoring-cost datum.
  *Known noise:* the author has just deep-explored these modules, so the
  cost datum is a **lower bound**, not a measurement — record it as such.
- **Step 2 — T2** in a **temporary clone of `forge-dogfood` under /tmp**
  (never the original, never the forge project root; `.forge/` is gitignored
  so the clone starts clean → fresh `forge init --content-backend native`).
  Drive `start → save → propose`; script tests the revision's changed paths
  against a YAML `allowed_changes`. Bonus probe for U3: two competing
  attempts under one intent with disjoint blast radii — the in-Forge
  miniature of the CooperBench scenario.
- **Step 3 — T3** CooperBench recon runs in parallel (background agent, no
  compute runs).
- **Step 4 — regroup:** T1 cost + T2 predicate result + T3 feasibility note
  are the direct inputs for v3 and the Part B go/no-go, including the B3–B4
  questions needing Jan's call (feature choice, CLAUDE.md stripping on a
  branch, pinned Arm B definition).

## §7b. Origin lineage — tracking against the founding prompt

The journey started (2026-07-04) from a raw brainstorm whose threads map to
the current state as follows. Recorded so v3 doesn't re-litigate the
deliberate divergences.

**Threads carried forward faithfully:** context handover as the central
problem (→ the thesis: context as a version-control problem); hard rules
that evolve with the codebase, where changing them is itself a process step
(→ contract records + lifecycle); "LLMs predict the next token" as the root
cause intuition (→ §A3: hallucination is unconstrained generation);
"basics first, VCS functionality is the core" (→ pilot-before-building,
wedge = the object model, not a convention).

**Threads deliberately diverged — with reasons, so they stay closed:**
1. *Nudges → gates.* The founding prompt asked for Forge to "nudge agents
   in the right direction" at pipeline points. Sharpened into enforcement:
   nudges are instructions and instructions fail under pressure — the same
   reason Parnas/Meyer/TDD failed as human discipline. CooperBench now
   backs this empirically (32% commitment failures even when communication
   works).
2. *Opaque encodings rejected* (steganography/images/binary of session
   history; v2 A6.6). Density was never the bottleneck — decision-relevance
   is; and the v2 move is to not transfer full-fidelity history at all, but
   compile it. Shepherd is the natural experiment: it preserves history
   byte-identically and still does nothing about context growth.
3. *Human readability retained, integrity kept.* The founding prompt
   floated "fine if humans can't read it, as long as integrity holds."
   Integrity survived fully (hash-chained, signed, revision-bound).
   Readability was inverted on purpose: the human review gate is the anchor
   against contract corruption and compact poisoning (A6.10) — remove
   readability and the only non-model defense goes with it.

**Thread parked, not lost:** the SDLC-pipeline/software-factory inside the
Forge CLI is the separate workflow-engine track (v2 §A5: workflow answers
*what now*, contracts answer *what exactly, within what bounds*).

**Standing drift check:** the founding problem is context handover across a
single lineage of work — one feature, many sessions. CooperBench measures
concurrent-agent coordination: adjacent, benchmarkable, and therefore the
shiny tangent. It stays pilot 3, behind Part B (decomposition) and pilot 2
(neighbor ablation). If CooperBench jumps the queue, we are deviating.

### §7.2 Measurement addendum (Jan's question, 2026-07-06: "how do we measure this?")

v2 §B2 already pins the metric set: headline = **unlicensed decisions per
accepted patch** ("which brief line authorized this decision?", scored
identically in every arm), plus the five-class defect taxonomy at first
review, unknown flow (surfaced vs. guessed vs. silent-guess defects,
first-run vs. post-resolution success scored separately), tokens injected
at start + total, revisions/task, blast-radius violations, wall-clock and
cost. The protocol safeguards are also pinned (order-randomized same tasks,
clean branch per run, anonymized diffs, no session logs before defect
scoring, pre-registered readings, frozen contracts, pinned Arm B).

Three additions this session, closing real gaps:

1. **Inter-rater check.** "Unlicensed decision" and defect classification
   are judgment calls. For ≥2 of the ~6 tasks, two reviewers (or one human +
   one independent agent session) score independently; report agreement.
   Low agreement → tighten the rubric before trusting the headline number.
   Write the scoring rubric BEFORE the first arm runs (with the frozen
   contracts).
2. **Cache-aware cost accounting.** Raw token counts mislead: Arm B (one
   continuous session) rides KV/prompt-cache discounts within the session,
   while Arm A pays fresh-input prices per session unless briefs share a
   stable prefix. B2's cost metric must separate cached-read vs. fresh
   input tokens, and report both tokens and $ cost.
3. **What the spikes measure (pre-pilot):** T1 → brief tokens vs.
   CLAUDE.md-baseline + authoring cost (lower bound; author had just
   explored the modules). T2 → binary predicate feasibility + U3. T3 →
   feasibility verdict. None of these validate H1/H-CORE — they price the
   pilot, they don't replace it.

Honest limits, recorded: N≈6 is signal, not evidence (v2 §A9.3 already
concedes this); single-project subject (Forge) limits external validity;
the authoring-cost datum is contaminated by author familiarity.

### §7.3 KV-cache note (Jan's question: "does Shepherd's cache optimization matter for us?")

Shepherd needs byte-identical history replay because *history is its
context carrier* — >95% cache hits are what make that affordable. We
deliberately don't carry history, so their mechanism doesn't transfer. But
the underlying economics do, in two places:

1. **Byte-stable briefs (new design requirement for `forge brief`).** A
   brief is a deterministic function of (contract revision, neighbor set,
   policy) — v2 already requires no-LLM emission. Strengthen that to
   **canonical serialization**: stable key order, stable contract ordering,
   no timestamps. Identical inputs → identical bytes → the brief becomes a
   stable prompt prefix that prompt-caching discounts across every task
   touching the same neighborhood. Reproducibility (already required for
   security) buys cache economics for free — but only if byte-stability is
   specified from day one. Costs nothing now; retrofitting later would.
2. **Pilot cost honesty** — see §7.2 item 2.

Out of scope until verified-handoff work resumes: mid-task resume (where
Shepherd's replay actually shines) — our unit is task-scoped fresh
sessions; C0/C1 interruption stays with the verified-handoffs track.

## §8. Decision points — resolved 2026-07-06

1. Spike branch + T1/T2/T3 — **approved**; T4 dropped (see §7).
2. v3 timing — **after the spikes report**; this doc holds findings
   meanwhile.
3. Docs committed on **`experiment/ccx-spikes`**, not main — Jan approves
   all merges to main (public repo). U4 lineage gap (uncommitted
   verified-handoff doc) still open; resolve before v3 cites it.
4. Pilot-3 positioning (CooperBench after Part B and pilot 2) — **agreed**.

## Sources

- Shepherd: https://shepherd-agents.ai/ · https://arxiv.org/abs/2605.10913
- CooperBench: https://cooperbench.com/ · https://arxiv.org/abs/2601.13295 ·
  Harbor adapter: https://github.com/harbor-framework/harbor/tree/main/adapters/cooperbench
- Forge substrate findings: code exploration 2026-07-06 (file/line refs in §4
  are verified against the working tree at commit 6238c53)
