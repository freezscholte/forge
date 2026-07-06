---
title: "Filesystem-enumeration surfaces must share one exclusion contract — never write a second walker"
date: 2026-07-06
category: architecture-patterns
module: forge-content-native
problem_type: architecture_pattern
component: workspace-drift-equality-vs-scanner-walk
severity: high
applies_when:
  - Any new code enumerates worktree/workspace files to compare against a recorded native tree (equality checks, drift guards, verification passes)
  - A check's "actual side" is built with a different ignore/exclusion stack than the walk that produced its "expected side"
  - forge-store (or any consumer crate) needs tree-vs-filesystem facts that forge-content-native computes internally via pub(crate) primitives
tags: [exclusion-contract, gitignore, forgeignore, is-ignored-by-policy, walk-worktree, workspace-equality, drift-guard, one-walker-rule, ner-382]
---

# Filesystem-enumeration surfaces must share one exclusion contract

## Context

The NER-382 drift guard (refuse `attempt attach` when the workspace dir
drifted from its recorded `materialized_content_ref`) shipped with a
hand-built workspace walk in `forge-store` that filtered only
`forge_content::is_ignored_by_policy`. The recorded tree it compared
against was built by the native scanner (`walk_worktree`), which ALSO
honors `.gitignore`/`.forgeignore` (`.git_ignore(true)`), and the
re-materialization deletion pass honors them too. Result — reproduced live
against the binary: a gitignored build artifact (`target/artifact.o`)
created by running builds inside a workspace (a documented supported flow)
triggered `WORKSPACE_DRIFT`, survived `--discard-workspace-changes`
(re-materialization skips gitignored files), and the very next attach
refused again. Permanent, unclearable false drift that trains users to
reflexively pass the override, defeating the guard. The same hand-built
walk also re-parsed the native tree-object JSON (`entries/name/kind/mode/
object`) in forge-store, skipping `validate_tree_entry` checks the owning
crate applies. Root cause of the hand-build: the natural primitive
(`tree_fingerprints`) was `pub(crate)` in forge-content-native.

## Guidance

One walker, one exclusion contract, owned by forge-content-native:

- Never build a second filesystem walk or tree parse outside the owning
  crate. If the primitive you need is `pub(crate)`, EXPOSE a purpose-built
  read-only function from the owning crate rather than reimplementing.
- Both sides of any tree-vs-filesystem comparison must go through the same
  exclusion stack: policy filter (`is_ignored_by_policy`) AND the ignore
  walker semantics (`.forgeignore`, `.gitignore`, rooted at the scanned
  dir so materialized ignore files apply).
- The fix shape that worked: `forge_content_native::workspace_equality::
  tree_equality_drift(repo_root, scan_root, tree, excluded_paths)` —
  actual side via `walk_worktree` rooted at `scan_root`, expected side via
  `tree_fingerprints` (which enforces tree schema + entry validation),
  bytes-only blob-id comparison, symlink targets compared without
  following, strictly read-only. The forge-store caller shrank ~180 lines.
- Contracts/specs for enumeration features must state the exclusion
  contract explicitly or name the owning primitive ("enumerate via
  walk_worktree semantics"), never leave it implied.

## Why This Matters

A filter divergence between a write path and a read path is a silent
false-negative or false-positive factory: drift goes undetected, or
spurious refusals condition users to bypass the guard. The divergence is
invisible to the feature's own tests (they don't know to create gitignored
files) and to acceptance gates — it was found only by adversarial review.
See also the sibling learning: native-worktree-walker-ignore-engine doc
(2026-05-30) already classified ignore-semantics divergence as a class,
not a one-off; this is the second confirmed instance.

## When to Apply

Before writing ANY loop over `fs::read_dir` in a crate other than
forge-content-native, or any `serde_json` parse of a native object
payload outside it: stop, find the owning primitive, expose it if needed.

## Examples

Anti-pattern (as shipped, commit 158cc65): `collect_workspace_paths` with
policy-only filtering + `collect_expected_tree_files` hand-parsing tree
JSON in forge-store. Fix (commit bc2ea57): both deleted; one shared
read-only primitive in forge-content-native/src/workspace_equality.rs,
pinned by `attach_drift_check_honors_workspace_gitignore` (gitignored
artifact is not drift; non-ignored stray still is).
