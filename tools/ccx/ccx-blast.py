#!/usr/bin/env python3
"""ccx blast-radius check (v1 of experiments/ccx/blast-check.py).

Usage:
    forge propose --json | ccx-blast.py --allow 'src/**' --forbid 'src/main.ts'
    git diff | ccx-blast.py --diff --contract tools/ccx/contracts/task.yaml

Two input modes on stdin:

  * envelope mode (default; auto-detected when the first non-whitespace char
    is `{`): a forge.cli.v0 envelope whose data carries `changed_paths`,
    tested path-level against the allow/forbid globs — exactly the pilot
    semantics. A facade path outside the allowlist is still a plain
    violation (exit 2), annotated with a note to rerun in --diff mode.
  * --diff mode: a unified diff; changed paths come from the file headers
    (`--- a/…` / `+++ b/…`, `/dev/null`, `rename from/to`), and hunks are
    kept so facade files (default: crates/forge-store/src/lib.rs,
    crates/forge-cli/src/main.rs; extend with --facade) may be permitted
    outside the allowlist iff every changed line is declaration/re-export
    only, judged statement-aware (multi-line brace-balanced `mod …;` /
    `pub use …;` spans, inferred from hunk context lines too).

Globs use fnmatch with `**` normalized to `*` (which already crosses `/`),
so authors can write gitignore-style patterns. --contract sources allow
globs from `allowed_changes.paths` and forbid globs from
`allowed_changes.forbidden_paths`; explicit --allow/--forbid are additive.
A default-forbid list (.forge/**, .env, private keys, credential paths) is
ALWAYS applied regardless of contract or flags.

Exit 0 = inside blast radius, 2 = violation(s), 1 = usage/parse error.
"""
import argparse
import fnmatch
import json
import re
import sys

# Always-forbidden paths, applied regardless of --contract / --allow /
# --forbid. Mirrors Forge's snapshot/export exclusions.
DEFAULT_FORBID = [
    ".forge/**",
    ".env",
    ".env.*",
    "*.pem",
    "*_rsa",
    "*_ed25519",
    "*.key",
    ".aws/**",
    ".ssh/**",
    "*credentials*",
]

# Facade files (see docs/adr/0001-domain-modules.md): decl/re-export-only
# hunks are allowed here even outside the contract allowlist.
DEFAULT_FACADES = [
    "crates/forge-store/src/lib.rs",
    "crates/forge-cli/src/main.rs",
]

FACADE_NOTE = "facade path — rerun in --diff mode for line-level facade allowance"

# Start of a (possibly multi-line) declaration/re-export statement:
# `mod x;`, `pub mod x;`, `use …;`, `pub use …;`, `pub(crate) use …;`.
STMT_RE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?(?:use|mod)\b")


def matches(path: str, pattern: str) -> bool:
    # fnmatch's `*` already crosses `/`; normalize `**` so authors can write
    # gitignore-style patterns.
    return fnmatch.fnmatch(path, pattern.replace("**", "*"))


def load_contract(path: str):
    """Return (allow, forbid) globs from a ccx contract's allowed_changes."""
    try:
        import yaml
    except ImportError as err:
        raise SystemExit(
            "ccx-blast: --contract requires PyYAML "
            f"(python3 -m pip install pyyaml): {err}"
        )
    try:
        with open(path, encoding="utf-8") as handle:
            doc = yaml.safe_load(handle)
    except (OSError, yaml.YAMLError) as err:
        raise SystemExit(f"ccx-blast: cannot read contract {path}: {err}")
    if not isinstance(doc, dict):
        raise SystemExit(f"ccx-blast: contract {path} is not a mapping")
    allowed = doc.get("allowed_changes") or {}
    return list(allowed.get("paths") or []), list(allowed.get("forbidden_paths") or [])


def strip_diff_path(raw: str):
    """`--- a/x` / `+++ b/x` payload -> repo path, or None for /dev/null."""
    path = raw.split("\t", 1)[0].strip()
    if path in ("/dev/null", ""):
        return None
    if path.startswith("a/") or path.startswith("b/"):
        path = path[2:]
    return path


HUNK_RE = re.compile(r"^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@")


def parse_diff(text: str):
    """Parse a unified diff into per-file records with raw hunk lines.

    Each record: {"paths": set[str], "hunks": [[(tag, body), ...], ...]}.
    Handles `diff --git` blocks, headerless `---`/`+++` pairs, /dev/null
    (new/deleted files), and `rename from/to` lines. Raises ValueError on
    input that has no diff structure at all.
    """
    lines = text.splitlines()
    records = []
    cur = None
    i = 0

    def start():
        nonlocal cur
        cur = {"paths": set(), "hunks": []}
        records.append(cur)

    while i < len(lines):
        line = lines[i]
        hunk_match = HUNK_RE.match(line)
        if line.startswith("diff --git "):
            start()
        elif line.startswith("rename from ") or line.startswith("rename to "):
            if cur is None:
                start()
            cur["paths"].add(line.split(" ", 2)[2].strip())
        elif line.startswith("--- "):
            # A headerless diff starts each file at `---`; inside a
            # `diff --git` block it belongs to the current record.
            if cur is None or cur["hunks"]:
                start()
            path = strip_diff_path(line[4:])
            if path:
                cur["paths"].add(path)
        elif line.startswith("+++ "):
            if cur is None:
                start()
            path = strip_diff_path(line[4:])
            if path:
                cur["paths"].add(path)
        elif hunk_match:
            if cur is None:
                raise ValueError(f"hunk header before any file header: {line}")
            old_left = int(hunk_match.group(1) or 1)
            new_left = int(hunk_match.group(2) or 1)
            hunk = []
            i += 1
            while i < len(lines) and (old_left > 0 or new_left > 0):
                raw = lines[i]
                tag = raw[:1] or " "
                body = raw[1:]
                if tag == " ":
                    old_left -= 1
                    new_left -= 1
                elif tag == "-":
                    old_left -= 1
                elif tag == "+":
                    new_left -= 1
                elif tag == "\\":  # "\ No newline at end of file"
                    i += 1
                    continue
                else:
                    break
                hunk.append((tag, body))
                i += 1
            cur["hunks"].append(hunk)
            continue
        i += 1

    if not records and text.strip():
        raise ValueError("no unified diff file headers found")
    return records


def hunks_decl_only(hunks) -> bool:
    """True iff every changed line in every hunk is declaration-only.

    Statement-aware, not per-line: a changed line is allowed when it is
    blank, a `//` comment, an attribute, or lies within a `mod …;` /
    `[pub] use …;` statement span. Spans are reconstructed separately for
    the old side (context + removed) and new side (context + added) of
    each hunk, so an opener like `pub use foo::{` that only appears as
    context still licenses added/removed continuation lines inside it.
    Brace balance is tracked from the statement opener; the span closes at
    a `;` once braces are balanced. Anything else (fn bodies, structs,
    non-use code) fails.
    """
    for hunk in hunks:
        for keep in ("-", "+"):
            in_stmt = False
            depth = 0
            for tag, body in hunk:
                if tag not in (" ", keep):
                    continue
                stripped = body.strip()
                code = stripped.split("//", 1)[0].strip()
                if not in_stmt and code and STMT_RE.match(code):
                    in_stmt = True
                    depth = 0
                line_in_stmt = in_stmt
                if in_stmt:
                    depth += code.count("{") - code.count("}")
                    if depth <= 0 and code.endswith(";"):
                        in_stmt = False
                if tag != keep:
                    continue
                allowed = (
                    not stripped
                    or stripped.startswith("//")
                    or stripped.startswith("#[")
                    or stripped.startswith("#![")
                    or line_in_stmt
                )
                if not allowed:
                    return False
    return True


def classify(path, allow, forbid):
    """Shared allow/forbid classification; returns a violation kind or None."""
    if any(matches(path, glob) for glob in DEFAULT_FORBID):
        return "default_forbidden"
    if any(matches(path, glob) for glob in forbid):
        return "forbidden"
    if not any(matches(path, glob) for glob in allow):
        return "outside allowlist"
    return None


def emit(report, violations):
    json.dump(report, sys.stdout, indent=1)
    print()
    return 2 if violations else 0


def run_envelope(text, allow, forbid, facades):
    try:
        envelope = json.loads(text)
    except json.JSONDecodeError as err:
        print(f"ccx-blast: stdin is not JSON: {err}", file=sys.stderr)
        return 1
    data = envelope.get("data", envelope)
    changed = data.get("changed_paths")
    if changed is None:
        print("ccx-blast: no changed_paths in payload", file=sys.stderr)
        return 1

    violations = []
    for path in changed:
        kind = classify(path, allow, forbid)
        if kind is None:
            continue
        violation = {"path": path, "kind": kind}
        if kind == "outside allowlist" and path in facades:
            violation["note"] = FACADE_NOTE
        violations.append(violation)

    report = {
        "revision": data.get("proposal_revision_id") or data.get("snapshot_id"),
        "changed_paths": changed,
        "allow": allow,
        "forbid": forbid,
        "default_forbid": DEFAULT_FORBID,
        "mode": "envelope",
        "facade_allowed": [],
        "violations": violations,
        "verdict": "violation" if violations else "within_blast_radius",
    }
    return emit(report, violations)


def run_diff(text, allow, forbid, facades):
    try:
        records = parse_diff(text)
    except ValueError as err:
        print(f"ccx-blast: stdin is not a unified diff: {err}", file=sys.stderr)
        return 1

    hunks_for = {}
    for record in records:
        for path in record["paths"]:
            hunks_for.setdefault(path, []).extend(record["hunks"])
    changed = sorted(hunks_for)

    violations = []
    facade_allowed = []
    for path in changed:
        kind = classify(path, allow, forbid)
        if kind is None:
            continue
        if kind == "outside allowlist" and path in facades:
            if hunks_decl_only(hunks_for[path]):
                facade_allowed.append(path)
                continue
        violations.append({"path": path, "kind": kind})

    report = {
        "revision": None,
        "changed_paths": changed,
        "allow": allow,
        "forbid": forbid,
        "default_forbid": DEFAULT_FORBID,
        "mode": "diff",
        "facade_allowed": facade_allowed,
        "violations": violations,
        "verdict": "violation" if violations else "within_blast_radius",
    }
    return emit(report, violations)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--allow", action="append", default=[], metavar="GLOB")
    ap.add_argument("--forbid", action="append", default=[], metavar="GLOB")
    ap.add_argument("--contract", metavar="FILE",
                    help="ccx contract; sources allow/forbid from allowed_changes")
    ap.add_argument("--diff", action="store_true",
                    help="read a unified diff on stdin instead of a forge envelope")
    ap.add_argument("--facade", action="append", default=[], metavar="PATH",
                    help="extend the facade-file set (default: forge-store lib.rs, forge-cli main.rs)")
    args = ap.parse_args()

    allow = list(args.allow)
    forbid = list(args.forbid)
    if args.contract:
        contract_allow, contract_forbid = load_contract(args.contract)
        allow.extend(contract_allow)
        forbid.extend(contract_forbid)
    if not allow:
        print("ccx-blast: at least one allow glob required (--allow or --contract)",
              file=sys.stderr)
        return 1
    facades = set(DEFAULT_FACADES) | set(args.facade)

    text = sys.stdin.read()
    if args.diff:
        return run_diff(text, allow, forbid, facades)
    stripped = text.lstrip()
    if stripped.startswith("{"):
        return run_envelope(text, allow, forbid, facades)
    if any(line.startswith(("diff --git ", "--- ", "+++ ")) for line in text.splitlines()):
        return run_diff(text, allow, forbid, facades)
    print("ccx-blast: stdin is neither a forge JSON envelope nor a unified diff",
          file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
