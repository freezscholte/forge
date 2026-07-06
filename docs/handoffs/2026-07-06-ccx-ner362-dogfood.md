# Handoff: CCX harness → NER-362 dogfood (next session)

Date: 2026-07-06 · Owner: Jan Skolte · Repo: `forge` (PUBLIC) + private
`forge-research` for archiving. Model/effort: your call.

## TL;DR

The CCX thin harness is **built, code-reviewed, and fully green**. Everything is
**local only** — the public branch and PR were deleted, with a fallback tag. The
next phase is the **NER-362 dogfood**: complete that ticket *through* the harness
by revising the `362-3` contract (tip-resolution defect), rerunning the affected
tasks, and running the full promotion round. Start with **D1 (author the revised
`362-3` v1 contract — offline, zero agent cost)**.

## Where things stand (all verified)

- **Branch:** `experiment/ccx-spikes`, HEAD `d6e89a9`. **Upstream is unset** — a
  bare `git push` refuses; nothing can be accidentally pushed.
- **Public copy removed:** the remote branch `origin/experiment/ccx-spikes` was
  deleted and **PR #124 auto-closed**. Caveat: it was public for a window, so
  caches/forks/GHArchive may retain it; true server-side purge needs a GitHub
  Support ticket (not done, likely unnecessary for research content — no secrets
  seen). Rotate any real credentials if `f21022b`'s history ever held them.
- **Fallback tag:** `archive/ccx-spikes-2026-07-06` pins all 195 commits at HEAD;
  recoverable by name even if the branch is reset/deleted locally.
- **Private research repo:** `github.com/forge-vcs/forge-research`, cloned at
  `/Users/skolte/Github-Private/forge-research` (currently empty). Plan: copy
  experiments + research narrative there for archiving **once the experiment is
  finalized** (i.e. after this dogfood).
- **forge CLI:** `forge 0.1.0` at `~/.cargo/bin/forge` — needed for D3's live
  runs / any real `forge` lifecycle, NOT for harness unit tests (cargo+git only).

## The harness (what shipped — 9 commits, `782154a`..`d6e89a9`)

`tools/ccx/` — files + scripts only, **no new Forge objects** (decision-rule
boundary). Plan: `docs/plans/2026-07-06-001-feat-ccx-thin-harness-plan.md`.
Code-review triage: `docs/code-reviews/2026-07-06-ccx-thin-harness.md` (10 real
findings fixed incl. 3 cross-model-confirmed P1s, 2 deferred).

| Tool | Role |
|---|---|
| `ccx-brief.py` | byte-stable brief emitter, YAML neighbor resolution, fail-closed |
| `ccx-lint.py` | 6 rule families; `--json`, `--dump-acceptance` (fail-closed), `--dump-caps` |
| `ccx-blast.py` | statement-aware facade allowance + always-on default-forbid; `--diff`/envelope |
| `run-task.sh` + `lib.sh` | lint-gated, detached-HEAD stacking, halt-on-unknown (exit 2), blast postflight (exit 3), `depends_on` chain |
| `verify-task.sh` | fix-set/guard-set split (exit 2 = fix failed, 4 = guard regressed) |
| `CONTRACT-SCHEMA.md`, `UNKNOWN-TRIAGE.md`, `README.md`, `prompts/task-instruction.txt` | schema v1, triage flow, stop rule |

**Verify:** `bash tools/ccx/tests/run-tests.sh` (42 python + 2 shell suites) —
wired into `scripts/ci.sh` and `.github/workflows/ci.yml`. Full CI mirror
(`rtk bash scripts/ci.sh`) passed end-to-end (95/95 e2e checks + trio).

## NER-362 dogfood — the next work

**Why NER-362:** the implementation already exists as pilot Arm-A stacked patches
(`experiments/ccx/runs/A-362-1`, `A-362-2-r2`, `A-362-3-r2`, `A-362-4-r2`,
`A-362-5-r2`), but is **held**: the `362-3` contract pinned blame's tip
resolution to the **native HEAD ref**, while repo convention resolves the
authoritative tip from the **ledger** (scorer-confirmed contract defect). So the
dogfood is the *post-pilot contract-revision flow*, exercised through the harness.

**Real-contract lint already run (dogfood signal):** linting the five frozen
`experiments/ccx/contracts/task-362-*.yaml` surfaced defects the pilot missed
(its `brief.sh` never parsed YAML):
- `362-1` **and** `362-3`: **invalid YAML** (R1 errors — unescaped colons /
  backticks in `interface:`). `362-3` can't be parsed at all.
- `362-2`: R5 exclusion-clause gap, R4 vacuous `provenance` filter, R2 cap
  collision (`forge-content-native/src/lib.rs` at its 4730 cap).
- `362-4`: R3 names `check_status`/`decision_status`/`intent_title` (not found),
  R4 vacuous `blame` filter.
- `362-5`: R4 `forge_blame` test is a not-yet-existing deliverable (correct warn).

### Phases

**D1 — Author the revised `362-3` v1 contract (offline, no agent, START HERE).**
Produce a valid **`ccx.contract.v1`** that: fixes the YAML; fixes tip resolution
to read the authoritative tip from the **ledger, not the native HEAD ref**; adds
the exclusion clause / `primitives:` / `acceptance.{fix,guard}` split; records the
resolution as a revision comment (model: `task-382-2-drift-guard.yaml` rev 2 +
`UNKNOWN-TRIAGE.md`). Re-lint with `python3 tools/ccx/ccx-lint.py
--contracts-dir <dir> <contract>` until clean. **Do NOT edit the frozen
`experiments/ccx/contracts/` files** — author the revision alongside them
(proposed home: `experiments/ccx/dogfood-362/` — CONFIRM location with Jan).

**D2 — Scratch clone.** `lib.sh` now refuses `--clone` == the harness repo, so
make a disposable clone outside the project root: e.g. `git clone
~/Github-Private/forge /tmp/forge-dogfood` (or `~/Github-Private/forge-dogfood`).
Full history, not shallow (the runner checks).

**D3 — Reruns through `run-task.sh` (LIVE — real `claude -p`, costs time/$).**
Re-run `362-3` (revised) + dependents `362-4`, `362-5` on the restacked base;
`362-1`/`362-2` are upstream and unaffected. Gate this behind Jan's review of the
D1 contract. Runner default agent cmd is `claude -p --output-format json
--dangerously-skip-permissions`; `--dry-run` substitutes a no-op for a mechanical
rehearsal first.

**D4 — Verify + promote.** `verify-task.sh` fix/guard on rebuilt bases → verify
trio → `/ce-code-review` gate (non-optional — gate-layering doc). Only then is it
merge-ready.

### Two decisions for Jan
1. **Where revised contracts live** (frozen record stays byte-identical) —
   proposed `experiments/ccx/dogfood-362/`.
2. **Live-run appetite for D3** — full rerun now, or D1 offline first + review gate
   before spending agent runs. Recommended: D1 first.

## Gotchas
- Public repo; only Jan-approved merges to `main`; screen internal codenames.
  Keep working local (upstream stays unset).
- Dogfood the `forge` binary ONLY in `/tmp` throwaway repos or a temp clone —
  NEVER from the project root (a stray `forge init` breaks repo-scoped commands).
- `forge-content-native/src/lib.rs` is allowlisted at exactly 4730 lines; new
  code goes in new module files.
- Verify trio + `rtk bash scripts/ci.sh` before any push; `/ce-code-review` is
  non-optional (it found live P1s after all gates were green — see the triage doc).
- Deferred harness follow-ups (in the plan's Open Questions): `--stack` per-id
  matching (currently a count guard); `matches()` dedup across lint/blast.
- Archiving to `forge-research` is deferred until the experiment is finalized
  (post-dogfood); `test_lint.py`/`test_blast.py` are coupled to the forge
  workspace and will need `--repo-root` re-pointing if the harness moves.
