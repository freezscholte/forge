# Handoff: CCX harness → Forge-native (Rust) brainstorm

Date: 2026-07-10 · Owner: Jan Skolte · Repo: `forge` (PUBLIC; this line is
LOCAL ONLY — upstream unset, never push without Jan's explicit go).

## TL;DR

Two consecutive dogfoods validated the CCX thin harness end-to-end; the
second ran with **zero harness fixes needed**, which was the pre-agreed
criterion for designing the Forge-native version. Next step is a
`/ce-brainstorm` (requirements-only) for turning the file-and-script
harness into product surface: contracts as Forge-native objects, the
runner/verifier as `forge` subcommands, runs/stops/verdicts recorded in
the ledger.

## Where things stand

- **Branch/state:** local `main` == `experiment/ccx-dogfood2` @ `e15b638`.
  Contains: NER-362 blame (ledger tip, enrichment, legend), NER-382 drift
  guard (from origin), NER-386 typed blame codes, NER-387 `--at`, all
  harness fixes, both dogfood triage docs. **Never pushed** — `main`'s
  history contains the (now tip-deleted) experiment records; a public push
  needs the cherry-pick split or an explicit publish-everything decision.
- **Harness:** `tools/ccx/` — lint (6 rule families), byte-stable brief
  emitter, blast-radius check, dependency-ordered runner with
  halt-on-unknown, fix/guard verifier. Self-tests fully self-contained as
  of `e15b638` (frozen pilot record lives in `tools/ccx/tests/fixtures/pilot/`).
- **Research record:** archived in the private
  `github.com/forge-vcs/forge-research` (`f2d9e05`), incl. both dogfood
  runs, all agent transcripts/patches, and `PROVENANCE.md`.
- **Open Linear (Forge project):** NER-388 (shared typed `UNKNOWN_COMMIT`
  for checkout + blame `--at`).

## Evidence base for the brainstorm

- `docs/code-reviews/2026-07-07-ner362-dogfood.md` +
  `2026-07-10-ner386-387-dogfood2.md` — what the gates caught, per run.
- forge-research `experiments/ccx/dogfood-362/RESULTS.md` and
  `dogfood-386-387/RESULTS.md` — run narratives; #2's "Harness-to-Rust
  verdict input" section is the direct motivation.
- `tools/ccx/CONTRACT-SCHEMA.md` — the de-facto v1 contract schema to be
  formalized; `tools/ccx/UNKNOWN-TRIAGE.md` — the stop convention (Leg 2:
  stops are successes — must survive any port).
- `docs/plans/2026-07-06-001-feat-ccx-thin-harness-plan.md` — original
  plan; its Open Questions carry two deferred items (per-id `--stack`
  matching instead of the count guard; `matches()` dedup across
  lint/blast).
- `docs/solutions/design-patterns/stop-on-unknown-gate-for-agent-briefs.md`
  and `docs/solutions/architecture-patterns/…typed-error-contract…` —
  patterns the native design must preserve.

## Seed questions for the brainstorm (not answers)

1. What is a contract natively — a new ledger object kind with its own
   lifecycle (draft → reviewed → frozen → revised), or a content object
   referenced by intents? How do revisions + UNKNOWN resolutions map to
   the existing decision/evidence model?
2. Which harness stages become `forge` subcommands vs stay policy checks
   (`forge-policy` already evaluates gates — is lint a check family)?
3. Are runs/stops first-class: agent session as an attempt? UNKNOWN as a
   typed stop record scored as success? blast violation as a check verdict?
4. What stays out of scope v1 (e.g. the agent-command execution itself may
   stay a thin shell; the eval-sink hardening from verify-task.sh must not
   regress).
5. Operational learnings to design in: every operand path canonicalized at
   argument boundaries (NER-384/385 family); no silent caps (per-id stack
   matching); unknown-key strictness for contract YAML (lint currently
   tolerates unknown top-level keys — too permissive).

## Gotchas (unchanged)

- Public repo; only Jan-approved pushes/merges to public main; screen
  internal codenames. Work on a feature branch off local `main`.
- Dogfood the `forge` binary only in /tmp scratch repos.
- Verify trio + `rtk bash scripts/ci.sh` before considering anything done;
  `/ce-code-review` gate is non-optional.
- `forge-content-native/src/lib.rs` at exactly its 4730-line cap.

## Start prompt (copy-paste into a fresh session)

> Read docs/handoffs/2026-07-10-ccx-harness-to-rust-brainstorm.md first —
> full state, evidence base, and seed questions. We're on local main
> (e15b638, LOCAL ONLY — never push). Task: run /ce-brainstorm for
> "CCX harness → Forge-native": turn the validated tools/ccx file-and-
> script harness into product surface — contracts as Forge-native objects,
> runner/verifier as forge subcommands, runs/stops/verdicts in the ledger.
> Requirements-only output (no implementation): scope what v1 natively
> owns vs what stays scripts, anchored in the two dogfood records and the
> seed questions in the handoff. Before drafting requirements, read
> tools/ccx/CONTRACT-SCHEMA.md, tools/ccx/UNKNOWN-TRIAGE.md, and the two
> dogfood code-review docs. The stop-on-unknown gate's three legs and the
> fix/guard exit-code split are non-negotiable invariants to preserve.
> Create the brainstorm branch off local main; run the doc-review gate on
> the output before handing it to /ce-plan.
