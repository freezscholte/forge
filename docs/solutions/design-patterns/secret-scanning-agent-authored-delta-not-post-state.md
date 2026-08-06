---
title: "Secret-scan the agent-authored delta, not post-state files"
date: 2026-07-18
category: design-patterns
module: forge-cli/contract_blast
problem_type: design_pattern
component: blast-secret-content-scan
severity: high
applies_when:
  - A content-level secret/policy scanner gates changes produced by an autonomous agent
  - The codebase legitimately contains secret-shaped strings (test fixtures, scanner test corpora, docs examples)
  - The guarded artifact is also an execution payload that must not be rewritten (redaction would corrupt it)
tags: [secret-scanning, blast-radius, false-positive, diff-aware, ccx, contracts, detect-and-refuse]
---

# Secret-scan the agent-authored delta, not post-state files

## Context

Dogfood #3 (2026-07-18): the first real `forge contract run` was refused with
`secret-content detected in crates/forge-cli/tests/forge_contract.rs`. The
contract *required* editing that file — its flip-guard test deliberately
contains secret-shaped fixture strings, already signed into the base tree. The
U6 scan evaluated the whole post-state of every modified file, so pre-existing
fixture content the agent never wrote tripped the gate. Any repo that tests a
secret scanner carries such fixtures, so whole-file scanning makes the guard
refuse exactly the changes that maintain it.

## Guidance

- Scope the scan to what the actor authored: for modified files, read the
  baseline version and scan only added lines (a simple line-set difference —
  post lines absent from the baseline set — erring toward scanning more); for
  added files, scan everything; skip deletions.
- Keep refusal semantics detect-and-refuse when the artifact is also a payload
  (redacting in place would corrupt what later gets applied/integrated); keep
  bounds and path-only reporting unchanged.
- Precision consequences to accept explicitly: a moved pre-existing line is
  not re-flagged (correct); a duplicated one is not re-flagged (acceptable —
  the content already exists in the signed base); only genuinely new
  secret-bearing lines trigger.
- Non-UTF-8 or unreadable baselines collapse to an empty baseline set —
  fail toward scanning more, never less.

## Why This Matters

The false positive is not cosmetic: it blocks precisely the maintenance work
the guard depends on (updating the tests that pin the guard), and it trains
operators to route around the gate. Diff-aware scoping removed the false
positive with zero loss of new-secret detection — proven by keeping the
original new-file refusal test green while the dogfood regression (modify a
fixture-bearing file with a harmless line) flipped from exit 3 to exit 0, and
a new-secret-line-added case still refuses.

## Examples

- Fix: `feb02a9` (`contract_blast.rs` diff-aware scan + `blob_read` baseline
  access); regression tests in `crates/forge-cli/tests/forge_contract_hardening.rs`.
- Trigger: dogfood #3 run 1 (`docs/code-reviews/2026-07-18-dogfood3-gc-repack.md`).
- Same family as gitleaks' protect-mode staged-diff scanning: mature secret
  scanners scan diffs, not trees, for exactly this reason.
