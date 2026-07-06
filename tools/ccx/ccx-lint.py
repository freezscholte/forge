#!/usr/bin/env python3
"""ccx thin harness: contract lint — a contract that fails lint never reaches an agent.

Usage:
    ccx-lint.py --contracts-dir <dir> [--repo-root <dir>] [--json] <contract.yaml>
    ccx-lint.py --contracts-dir <dir> --dump-acceptance <contract.yaml>
    ccx-lint.py --contracts-dir <dir> --dump-caps

Six rule families, each transcribing a named pilot defect (see
tools/ccx/CONTRACT-SCHEMA.md, the normative schema):

  R1 parse + shape     valid YAML mapping; required keys; known schema;
                       neighbors/depends_on resolve; depends_on graph acyclic
  R2 satisfiability    allowed_changes.paths non-empty; the line-count caps
                       (scripts/check-rust-line-count.sh, parsed at runtime)
                       leave room to actually land the change; allow set not
                       trivially emptied by forbidden globs
  R3 primitives        declared primitives exist in the named crate and their
                       actual visibility is reachable from allowed_changes
                       (transcribes the pub(crate) tree_fingerprints defect)
  R4 acceptance        cargo-test fix/guard entries are non-vacuous: --test
                       files exist, filters match at least one candidate test
                       path (deliverable targets downgrade to warnings)
  R5 exclusion clause  contracts whose text touches filesystem enumeration
                       must carry exclusion_contract: or an owning walk
                       primitive (transcribes the drift-guard P1)
  R6 command grammar   every acceptance entry matches
                       ^cargo (test|clippy|fmt|build|run)\\b (eval surface)

Severity: on schema ccx.contract.v0 every R2-R6 finding downgrades to a
warning (the frozen pilot record lints with zero errors); R1 errors stay
errors. Exit 0 = clean or warnings only, 1 = usage/internal error, 2 = any
error finding. Human-readable findings go to stderr; --json emits
{"contract", "findings", "verdict"} to stdout.
"""
import argparse
import fnmatch
import json
import re
import shlex
import subprocess
import sys
from pathlib import Path

try:
    import yaml
except ImportError:
    print(
        "ccx-lint: PyYAML is required (install with: python3 -m pip install pyyaml)",
        file=sys.stderr,
    )
    sys.exit(1)

REQUIRED_KEYS = [
    "schema",
    "id",
    "revision",
    "ticket",
    "task",
    "interface",
    "acceptance",
    "allowed_changes",
    "authority",
]
SCHEMA_V0 = "ccx.contract.v0"
SCHEMA_V1 = "ccx.contract.v1"
COMMAND_GRAMMAR = re.compile(r"^cargo (test|clippy|fmt|build|run)\b")
# Shell metacharacters that turn a lint-passing `cargo ...` prefix into an
# injection when the string reaches `eval` in verify-task.sh. The grammar
# regex is prefix-anchored by design (cargo takes arbitrary trailing args),
# so command safety is a separate, explicit check: an acceptance entry must
# BOTH start with an allowed cargo subcommand AND contain no shell control.
SHELL_METACHARACTERS = ";&|`$(){}<>\n\r\\!*?[]#\"'"


def command_is_safe(cmd) -> bool:
    """A grammar-valid, metacharacter-free acceptance command.

    Enforced identically by lint rule 6 and by --dump-acceptance so every
    consumer of acceptance commands (notably verify-task.sh, which eval's
    them) is gated on the same rule regardless of whether it re-runs lint.
    """
    if not isinstance(cmd, str) or not COMMAND_GRAMMAR.match(cmd):
        return False
    return not any(ch in cmd for ch in SHELL_METACHARACTERS)
# Single keyword-list constant for filesystem-enumeration signals (rule 5).
ENUMERATION_SIGNALS = [
    "read_dir",
    "walk",
    "enumerate",
    "scan",
    "drift",
    "workspace files",
    "directory tree",
]
EXCLUSION_DOC = (
    "docs/solutions/architecture-patterns/"
    "filesystem-enumeration-shared-exclusion-contract.md"
)
LINE_COUNT_SCRIPT = "scripts/check-rust-line-count.sh"
SKIP_DIRS = {".git", "target", ".forge", "node_modules", ".venv", "__pycache__"}
# cargo-test flags that consume a value (so the value is not read as a filter).
CARGO_VALUE_FLAGS = {
    "-p",
    "--package",
    "--test",
    "--features",
    "--exclude",
    "--bin",
    "--example",
    "--bench",
    "--profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--jobs",
    "-j",
    "--color",
}


def matches(path: str, pattern: str) -> bool:
    # fnmatch's `*` already crosses `/`; normalize `**` so authors can write
    # gitignore-style patterns (same semantics as experiments/ccx/blast-check.py).
    return fnmatch.fnmatch(path, pattern.replace("**", "*"))


def default_repo_root() -> Path:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True,
            text=True,
            check=True,
        )
        return Path(out.stdout.strip())
    except (OSError, subprocess.CalledProcessError):
        return Path.cwd()


def parse_line_caps(repo_root: Path):
    """Extract (global max_lines, {path: cap}) from scripts/check-rust-line-count.sh.

    The allowlist is regex-parsed at runtime from the `allowed_cap()` case
    arms (shape: `crates/....rs) echo NNN ;;`) so lint can never drift from
    the enforced caps.
    """
    script = repo_root / LINE_COUNT_SCRIPT
    max_lines = 3000
    caps = {}
    if not script.is_file():
        return max_lines, caps
    text = script.read_text(encoding="utf-8", errors="replace")
    m = re.search(r"^\s*max_lines=(\d+)\s*$", text, re.MULTILINE)
    if m:
        max_lines = int(m.group(1))
    for arm in re.finditer(
        r"^\s*(crates/\S+\.rs)\)\s+echo\s+(\d+)\s*;;", text, re.MULTILINE
    ):
        caps[arm.group(1)] = int(arm.group(2))
    return max_lines, caps


def repo_file_list(repo_root: Path):
    files = []
    stack = [repo_root]
    while stack:
        d = stack.pop()
        try:
            entries = sorted(d.iterdir())
        except OSError:
            continue
        for entry in entries:
            if entry.name in SKIP_DIRS:
                continue
            if entry.is_symlink():
                continue
            if entry.is_dir():
                stack.append(entry)
            elif entry.is_file():
                files.append(entry.relative_to(repo_root).as_posix())
    return files


class Linter:
    def __init__(self, contract_path: Path, contracts_dir: Path, repo_root: Path):
        self.contract_path = contract_path
        self.contracts_dir = contracts_dir
        self.repo_root = repo_root
        self.findings = []
        self.is_v0 = False
        self._repo_files = None

    def add(self, rule: str, severity: str, message: str) -> None:
        # v0 downgrade: rule 2-6 errors become warnings (zero errors reserved
        # for broken YAML/shape); v1 keeps full strictness.
        if self.is_v0 and rule != "R1" and severity == "error":
            severity = "warning"
        self.findings.append(
            {"rule": rule, "severity": severity, "message": message}
        )

    @property
    def repo_files(self):
        if self._repo_files is None:
            self._repo_files = repo_file_list(self.repo_root)
        return self._repo_files

    # ---- R1: parse + shape -------------------------------------------------

    def load(self):
        try:
            raw = self.contract_path.read_bytes()
        except OSError as err:
            self.add("R1", "error", f"cannot read contract: {err}")
            return None
        try:
            parsed = yaml.safe_load(raw)
        except yaml.YAMLError as err:
            self.add("R1", "error", f"contract is not valid YAML: {err}")
            return None
        if not isinstance(parsed, dict):
            self.add("R1", "error", "contract is not a YAML mapping")
            return None
        return parsed

    def rule1_shape(self, contract: dict) -> bool:
        missing = [k for k in REQUIRED_KEYS if k not in contract]
        if missing:
            self.add("R1", "error", f"missing required keys: {', '.join(missing)}")
        schema = contract.get("schema")
        if schema == SCHEMA_V0:
            self.is_v0 = True
            self.add(
                "R1",
                "warning",
                f"legacy schema {SCHEMA_V0}; rule 2-6 findings downgrade to warnings",
            )
        elif schema != SCHEMA_V1 and "schema" in contract:
            self.add(
                "R1",
                "error",
                f"unknown schema {schema!r} (expected {SCHEMA_V1} or legacy {SCHEMA_V0})",
            )
        for key in ("neighbors", "depends_on"):
            ids = contract.get(key) or []
            if not isinstance(ids, list):
                self.add("R1", "error", f"{key}: must be a list of contract ids")
                continue
            for cid in ids:
                if self._resolve_id(cid) is None:
                    self.add(
                        "R1",
                        "error",
                        f"{key} id {cid!r} does not resolve to a contract file "
                        f"in {self.contracts_dir}",
                    )
        self._rule1_cycles(contract)
        return not missing

    def _resolve_id(self, cid):
        if not isinstance(cid, str) or not cid.startswith("ccx-"):
            return None
        path = self.contracts_dir / f"{cid[len('ccx-'):]}.yaml"
        return path if path.is_file() else None

    def _rule1_cycles(self, contract: dict) -> None:
        # DFS over the depends_on graph reachable from this contract.
        start = contract.get("id")
        if not isinstance(start, str):
            return

        def deps_of(cid):
            if cid == start:
                raw = contract.get("depends_on") or []
            else:
                path = self._resolve_id(cid)
                if path is None:
                    return []
                try:
                    parsed = yaml.safe_load(path.read_bytes())
                except (OSError, yaml.YAMLError):
                    return []
                if not isinstance(parsed, dict):
                    return []
                raw = parsed.get("depends_on") or []
            return [d for d in raw if isinstance(d, str)]

        visiting, done = [], set()

        def visit(cid) -> bool:
            if cid in done:
                return True
            if cid in visiting:
                cycle = visiting[visiting.index(cid):] + [cid]
                self.add(
                    "R1",
                    "error",
                    "depends_on cycle: " + " -> ".join(cycle),
                )
                return False
            visiting.append(cid)
            ok = all(visit(d) for d in deps_of(cid))
            visiting.pop()
            done.add(cid)
            return ok

        visit(start)

    # ---- R2: satisfiability ------------------------------------------------

    def rule2_satisfiability(self, contract: dict) -> None:
        allowed = contract.get("allowed_changes")
        if not isinstance(allowed, dict):
            self.add("R2", "error", "allowed_changes must be a mapping with paths")
            return
        paths = allowed.get("paths")
        if not isinstance(paths, list) or not paths:
            self.add("R2", "error", "allowed_changes.paths must be a non-empty list")
            return
        paths = [p for p in paths if isinstance(p, str)]

        max_lines, caps = parse_line_caps(self.repo_root)
        matched = set()
        new_file_room = False
        for g in paths:
            hits = [f for f in self.repo_files if matches(f, g)]
            if not hits:
                new_file_room = True  # room to create a new file at/under g
            matched.update(hits)

        rs_files = sorted(f for f in matched if f.endswith(".rs"))
        at_cap = []
        has_room = False
        for f in rs_files:
            cap = caps.get(f, max_lines)
            try:
                lines = sum(
                    1 for _ in open(self.repo_root / f, "rb")
                )
            except OSError:
                continue
            if lines >= cap:
                at_cap.append((f, cap, lines))
            else:
                has_room = True
        if rs_files and not has_room and not new_file_room:
            detail = "; ".join(f"{f} at {n} lines (cap {c})" for f, c, n in at_cap)
            self.add(
                "R2",
                "error",
                "unsatisfiable: every existing allowed .rs file is at/over its "
                f"line-count cap and no allowed glob leaves new-file room: {detail}",
            )
        else:
            for f, cap, lines in at_cap:
                self.add(
                    "R2",
                    "warning",
                    f"allowed file {f} is at/over its line-count cap "
                    f"({lines} lines, cap {cap}); changes there cannot add lines",
                )

        # Trivial total-overlap: every allow glob also matches a forbidden
        # glob pattern (conservative — only the degenerate case is flagged).
        forbidden = [
            f for f in (allowed.get("forbidden_paths") or []) if isinstance(f, str)
        ]
        if paths and forbidden:
            if all(
                any(g == f or matches(g, f) for f in forbidden) for g in paths
            ):
                self.add(
                    "R2",
                    "error",
                    "allowed_changes.paths is emptied by forbidden_paths: every "
                    "allow glob also matches a forbidden glob",
                )

    # ---- R3: primitive existence + visibility -------------------------------

    def _find_definition(self, name: str, roots):
        pat = re.compile(
            r"^\s*(pub(?:\s*\([^)]*\))?)?\s*(?:unsafe\s+)?(?:async\s+)?"
            r"(?:fn|struct|enum|trait|mod|const)\s+" + re.escape(name) + r"\b"
        )
        for root in roots:
            if not root.is_dir():
                continue
            for rs in sorted(root.rglob("*.rs")):
                try:
                    text = rs.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    continue
                for line in text.splitlines():
                    m = pat.match(line)
                    if m:
                        qual = (m.group(1) or "").replace(" ", "")
                        if qual == "pub":
                            vis = "pub"
                        elif qual.startswith("pub("):
                            vis = qual
                        else:
                            vis = "private"
                        return rs, vis
        return None, None

    def rule3_primitives(self, contract: dict) -> None:
        primitives = contract.get("primitives")
        allowed = contract.get("allowed_changes") or {}
        allow_globs = [
            p for p in (allowed.get("paths") or []) if isinstance(p, str)
        ]
        if isinstance(primitives, list) and primitives:
            for entry in primitives:
                if not isinstance(entry, dict) or "name" not in entry:
                    self.add(
                        "R3", "error", f"malformed primitives entry: {entry!r}"
                    )
                    continue
                name = entry.get("name")
                crate = entry.get("crate")
                crate_src = self.repo_root / "crates" / str(crate) / "src"
                if not crate_src.is_dir():
                    self.add(
                        "R3",
                        "error",
                        f"primitive {name}: crate {crate!r} has no src/ under crates/",
                    )
                    continue
                path, vis = self._find_definition(str(name), [crate_src])
                if path is None:
                    self.add(
                        "R3",
                        "error",
                        f"primitive {name}: no definition found in crates/{crate}/src/",
                    )
                    continue
                if vis != "pub":
                    prefix = f"crates/{crate}/"
                    inside = any(
                        g.startswith(prefix) or matches(prefix + "src/x.rs", g)
                        for g in allow_globs
                    )
                    if not inside:
                        self.add(
                            "R3",
                            "error",
                            f"primitive {name} in crates/{crate} is {vis}, but every "
                            "allowed_changes path lies outside the owning crate — "
                            "consumers cannot see it (see "
                            f"{EXCLUSION_DOC})",
                        )
        else:
            # Best-effort: backticked snake_case identifiers in interface text.
            interface = contract.get("interface")
            if not isinstance(interface, str):
                return
            ids = sorted(
                {
                    i
                    for i in re.findall(r"`([a-z][a-z0-9_]*)`", interface)
                    if "_" in i
                }
            )
            if not ids:
                return
            roots = sorted((self.repo_root / "crates").glob("*/src"))
            for name in ids:
                path, _vis = self._find_definition(name, roots)
                if path is None:
                    self.add(
                        "R3",
                        "warning",
                        f"interface names `{name}` but no definition was found "
                        "in any workspace crate (best-effort scan)",
                    )

    # ---- R4: acceptance non-vacuity ------------------------------------------

    @staticmethod
    def _module_base(rel: Path):
        # src/lib.rs, src/main.rs, tests/<name>.rs -> []; src/foo.rs -> [foo];
        # src/foo/mod.rs -> [foo]; src/foo/bar.rs -> [foo, bar]
        parts = list(rel.parts)
        if parts[0] == "tests":
            return []
        parts = parts[1:]  # drop src/
        if parts[-1] in ("lib.rs", "main.rs", "mod.rs"):
            parts = parts[:-1]
        else:
            parts[-1] = parts[-1][:-3]  # strip .rs
        return parts

    @staticmethod
    def _scan_candidates(text: str, base):
        mod_re = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)\s*\{")
        fn_re = re.compile(
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?"
            r'(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)'
        )
        depth = 0
        stack = []  # (mod name, depth at which it opened)
        out = []
        for line in text.splitlines():
            fn = fn_re.match(line)
            if fn:
                out.append(
                    "::".join(list(base) + [m for m, _ in stack] + [fn.group(1)])
                )
            mod = mod_re.match(line)
            if mod:
                stack.append((mod.group(1), depth))
            depth += line.count("{") - line.count("}")
            while stack and depth <= stack[-1][1]:
                stack.pop()
        return out

    def _crate_candidates(self, crate: str, test_file=None):
        crate_dir = self.repo_root / "crates" / crate
        files = []
        if test_file is not None:
            files = [crate_dir / "tests" / f"{test_file}.rs"]
        else:
            for sub in ("src", "tests"):
                d = crate_dir / sub
                if d.is_dir():
                    files.extend(sorted(d.rglob("*.rs")))
        candidates = []
        for f in files:
            if not f.is_file():
                continue
            try:
                text = f.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            candidates.extend(
                self._scan_candidates(text, self._module_base(f.relative_to(crate_dir)))
            )
        return candidates

    def _inside_allowed(self, rel_path: str, allow_globs) -> bool:
        return any(
            rel_path == g or matches(rel_path, g) for g in allow_globs
        )

    def rule4_acceptance(self, fix, guard, contract: dict) -> None:
        allowed = contract.get("allowed_changes") or {}
        allow_globs = [
            p for p in (allowed.get("paths") or []) if isinstance(p, str)
        ]
        for cmd in fix + guard:
            if not isinstance(cmd, str):
                continue
            try:
                toks = shlex.split(cmd)
            except ValueError:
                continue  # R6 handles ungrammatical commands
            if len(toks) < 2 or toks[0] != "cargo":
                continue
            if toks[1] != "test":
                if COMMAND_GRAMMAR.match(cmd):
                    self.add(
                        "R4",
                        "note",
                        f"non-test command exempt from non-vacuity: {cmd}",
                    )
                continue
            crate, test_target, filters = None, None, []
            i = 2
            while i < len(toks):
                t = toks[i]
                if t == "--":
                    break
                if t in ("-p", "--package"):
                    crate = toks[i + 1] if i + 1 < len(toks) else None
                    i += 2
                elif t == "--test":
                    test_target = toks[i + 1] if i + 1 < len(toks) else None
                    i += 2
                elif t in CARGO_VALUE_FLAGS:
                    i += 2
                elif t.startswith("-"):
                    i += 1
                else:
                    filters.append(t)
                    i += 1

            if test_target is not None:
                crates = (
                    [crate]
                    if crate
                    else [
                        p.name
                        for p in sorted((self.repo_root / "crates").iterdir())
                        if p.is_dir()
                    ]
                )
                rels = [f"crates/{c}/tests/{test_target}.rs" for c in crates]
                existing = [r for r in rels if (self.repo_root / r).is_file()]
                if not existing:
                    deliverable = any(
                        self._inside_allowed(r, allow_globs) for r in rels
                    )
                    if deliverable:
                        self.add(
                            "R4",
                            "warning",
                            f"acceptance target is a deliverable — verify post-run: "
                            f"{cmd} (test file does not exist yet)",
                        )
                    else:
                        self.add(
                            "R4",
                            "error",
                            f"vacuous acceptance: {cmd} names --test "
                            f"{test_target} but no such test file exists and it "
                            "is outside allowed_changes.paths",
                        )
                    continue
                if not filters:
                    continue
                candidates = []
                for r in existing:
                    c = Path(r).parts[1]
                    candidates.extend(self._crate_candidates(c, test_target))
            elif filters:
                if crate is None:
                    continue  # workspace-wide filter: skip (conservative)
                candidates = self._crate_candidates(crate)
            else:
                continue  # filterless cargo test: exempt

            for filt in filters:
                if any(filt in cand for cand in candidates):
                    continue
                # No deliverable exemption here (unlike the --test branch): a
                # bare `cargo test <filter>` whose filter matches nothing exits
                # 0 — vacuously green forever, with nothing forcing a matching
                # test to appear. That is exactly the pilot's `-p forge-store
                # provenance` Goodhart defect, so it is always an error. (A
                # missing `--test <target>`, by contrast, makes cargo fail
                # loudly, so its deliverable exemption is safe.)
                self.add(
                    "R4",
                    "error",
                    f"vacuous acceptance: filter {filt!r} in {cmd!r} matches "
                    "no candidate test path in the crate's code",
                )

    # ---- R5: exclusion clause -------------------------------------------------

    def rule5_exclusion(self, contract: dict) -> None:
        chunks = []
        for key in ("task", "interface"):
            v = contract.get(key)
            if isinstance(v, str):
                chunks.append(v)
        nc = contract.get("negative_constraints")
        if nc is not None:
            chunks.append(yaml.safe_dump(nc))
        text = "\n".join(chunks).lower()
        if not any(sig in text for sig in ENUMERATION_SIGNALS):
            return
        if "exclusion_contract" in contract:
            return
        primitives = contract.get("primitives") or []
        if any(
            isinstance(p, dict) and "walk" in str(p.get("name", ""))
            for p in primitives
        ):
            return
        self.add(
            "R5",
            "error",
            "contract text touches filesystem enumeration but declares no "
            "exclusion_contract: and no owning walk primitive — a second walker "
            f"with weaker exclusion semantics is licensed; see {EXCLUSION_DOC}",
        )

    # ---- R6: command grammar ----------------------------------------------------

    def rule6_grammar(self, fix, guard) -> None:
        for cmd in fix + guard:
            if command_is_safe(cmd):
                continue
            if isinstance(cmd, str) and COMMAND_GRAMMAR.match(cmd):
                self.add(
                    "R6",
                    "error",
                    f"acceptance entry contains shell metacharacters "
                    f"(reaches eval in verify-task.sh): {cmd!r}",
                )
            else:
                self.add(
                    "R6",
                    "error",
                    f"acceptance entry violates command grammar "
                    f"^cargo (test|clippy|fmt|build|run): {cmd!r}",
                )

    # ---- driver ------------------------------------------------------------------

    def normalize_acceptance(self, contract: dict):
        acc = contract.get("acceptance")
        if isinstance(acc, dict):
            fix = acc.get("fix") or []
            guard = acc.get("guard") or []
            if not isinstance(fix, list) or not isinstance(guard, list):
                self.add("R1", "error", "acceptance.fix/guard must be lists")
                return [], []
            return fix, guard
        if isinstance(acc, list):
            self.add(
                "R1",
                "warning",
                "flat v0 acceptance list treated as fix set with empty guard set",
            )
            return acc, []
        if acc is not None:
            self.add("R1", "error", "acceptance must be a mapping or a list")
        return [], []

    def run(self):
        contract = self.load()
        if contract is None:
            return
        shape_ok = self.rule1_shape(contract)
        fix, guard = self.normalize_acceptance(contract)
        if not shape_ok:
            return  # shape is broken; deeper rules would just cascade
        self.rule2_satisfiability(contract)
        self.rule3_primitives(contract)
        self.rule4_acceptance(fix, guard, contract)
        self.rule5_exclusion(contract)
        self.rule6_grammar(fix, guard)


def dump_acceptance(contract_path: Path) -> int:
    try:
        parsed = yaml.safe_load(contract_path.read_bytes())
    except (OSError, yaml.YAMLError) as err:
        print(f"ccx-lint: cannot read contract: {err}", file=sys.stderr)
        return 1
    if not isinstance(parsed, dict):
        print("ccx-lint: contract is not a YAML mapping", file=sys.stderr)
        return 1
    acc = parsed.get("acceptance")
    if isinstance(acc, dict):
        out = {"fix": acc.get("fix") or [], "guard": acc.get("guard") or []}
    elif isinstance(acc, list):
        out = {"fix": acc, "guard": []}
    else:
        print("ccx-lint: contract has no acceptance", file=sys.stderr)
        return 1
    # Fail closed: verify-task.sh eval's these strings and does not re-lint,
    # so --dump-acceptance is the gate. Refuse to emit any command that would
    # not pass rule 6 (grammar + no shell metacharacters), regardless of the
    # contract's schema version — the eval surface is not v0-exempt.
    for cmd in list(out["fix"]) + list(out["guard"]):
        if not command_is_safe(cmd):
            print(
                f"ccx-lint: unsafe acceptance command refused: {cmd!r}",
                file=sys.stderr,
            )
            return 2
    json.dump(out, sys.stdout, indent=1)
    print()
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--contracts-dir", required=True, metavar="DIR")
    ap.add_argument("--repo-root", metavar="DIR")
    ap.add_argument("--json", action="store_true", dest="as_json")
    ap.add_argument(
        "--dump-acceptance",
        action="store_true",
        help="print {fix, guard} JSON for the contract and exit",
    )
    ap.add_argument(
        "--dump-caps",
        action="store_true",
        help="print the parsed line-count cap allowlist as JSON and exit (self-test)",
    )
    ap.add_argument("contract", nargs="?", metavar="CONTRACT_YAML")
    args = ap.parse_args()

    repo_root = Path(args.repo_root) if args.repo_root else default_repo_root()

    if args.dump_caps:
        max_lines, caps = parse_line_caps(repo_root)
        json.dump({"max_lines": max_lines, "caps": caps}, sys.stdout, indent=1)
        print()
        return 0

    if args.contract is None:
        print("ccx-lint: a contract file is required", file=sys.stderr)
        return 1

    contracts_dir = Path(args.contracts_dir)
    contract = Path(args.contract)
    if not contract.is_file():
        contract = contracts_dir / contract.name
    if not contract.is_file():
        print(f"ccx-lint: no such contract: {args.contract}", file=sys.stderr)
        return 1

    if args.dump_acceptance:
        return dump_acceptance(contract)

    linter = Linter(contract, contracts_dir, repo_root)
    linter.run()

    has_error = any(f["severity"] == "error" for f in linter.findings)
    has_warning = any(f["severity"] == "warning" for f in linter.findings)
    verdict = "errors" if has_error else ("warnings" if has_warning else "clean")

    for f in linter.findings:
        print(
            f"ccx-lint: {f['severity'].upper()} [{f['rule']}] {f['message']}",
            file=sys.stderr,
        )
    print(f"ccx-lint: {contract}: {verdict}", file=sys.stderr)

    if args.as_json:
        json.dump(
            {
                "contract": str(contract),
                "findings": linter.findings,
                "verdict": verdict,
            },
            sys.stdout,
            indent=1,
        )
        print()
    return 2 if has_error else 0


if __name__ == "__main__":
    sys.exit(main())
