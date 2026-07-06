"""Tests for tools/ccx/ccx-blast.py (subprocess-level, stdlib unittest only).

Run from the repo root:
    python3 -m unittest discover -s tools/ccx/tests -p 'test_blast.py'
"""
import json
import subprocess
import sys
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
SCRIPT = REPO_ROOT / "tools" / "ccx" / "ccx-blast.py"
FIXTURES = Path(__file__).resolve().parent / "fixtures"
FACADE = "crates/forge-store/src/lib.rs"

REPLAY_CONTRACT = REPO_ROOT / "experiments/ccx/contracts/task-382-2-drift-guard.yaml"
REPLAY_PATCH = REPO_ROOT / "experiments/ccx/runs/A-382-2-r2/patch.diff"


def run_blast(args, stdin_text):
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        input=stdin_text,
        capture_output=True,
        text=True,
        cwd=REPO_ROOT,
    )


def report_of(proc):
    return json.loads(proc.stdout)


class BlastDiffModeTests(unittest.TestCase):
    def test_allowed_paths_within_radius(self):
        diff = (
            "diff --git a/src/foo.rs b/src/foo.rs\n"
            "--- a/src/foo.rs\n"
            "+++ b/src/foo.rs\n"
            "@@ -1,2 +1,3 @@\n"
            " line one\n"
            "+added line\n"
            " line two\n"
        )
        proc = run_blast(["--diff", "--allow", "src/**"], diff)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "within_blast_radius")
        self.assertEqual(report["changed_paths"], ["src/foo.rs"])
        self.assertEqual(report["violations"], [])
        self.assertEqual(report["facade_allowed"], [])

    def test_facade_single_line_decls_allowed(self):
        diff = (
            f"diff --git a/{FACADE} b/{FACADE}\n"
            f"--- a/{FACADE}\n"
            f"+++ b/{FACADE}\n"
            "@@ -1,2 +1,4 @@\n"
            " mod bar;\n"
            "+mod foo;\n"
            "+pub use foo::Bar;\n"
            " pub use bar::Baz;\n"
        )
        proc = run_blast(["--diff", "--allow", "docs/**"], diff)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "within_blast_radius")
        self.assertEqual(report["facade_allowed"], [FACADE])
        self.assertEqual(report["violations"], [])

    def test_facade_multiline_use_continuation_allowed(self):
        # Added lines are bare identifiers INSIDE an existing wrapped
        # `pub use foo::{ ... };` block whose opener is only hunk context.
        diff = (
            f"diff --git a/{FACADE} b/{FACADE}\n"
            f"--- a/{FACADE}\n"
            f"+++ b/{FACADE}\n"
            "@@ -1,4 +1,6 @@\n"
            " pub use foo::{\n"
            "     Alpha,\n"
            "+    Bravo,\n"
            "+    Charlie,\n"
            "     Delta,\n"
            " };\n"
        )
        proc = run_blast(["--diff", "--allow", "docs/**"], diff)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "within_blast_radius")
        self.assertEqual(report["facade_allowed"], [FACADE])

    def test_facade_fn_body_is_violation(self):
        diff = (
            f"diff --git a/{FACADE} b/{FACADE}\n"
            f"--- a/{FACADE}\n"
            f"+++ b/{FACADE}\n"
            "@@ -1,1 +1,4 @@\n"
            " mod bar;\n"
            "+fn sneaky() {\n"
            "+    do_things();\n"
            "+}\n"
        )
        proc = run_blast(["--diff", "--allow", "docs/**"], diff)
        self.assertEqual(proc.returncode, 2)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "violation")
        self.assertEqual(report["facade_allowed"], [])
        self.assertEqual(
            report["violations"],
            [{"path": FACADE, "kind": "outside allowlist"}],
        )

    def test_forbidden_path_violation(self):
        diff = (
            "diff --git a/crates/forge-content-native/src/lib.rs b/crates/forge-content-native/src/lib.rs\n"
            "--- a/crates/forge-content-native/src/lib.rs\n"
            "+++ b/crates/forge-content-native/src/lib.rs\n"
            "@@ -1,1 +1,2 @@\n"
            " existing\n"
            "+added\n"
        )
        proc = run_blast(
            ["--diff", "--allow", "**", "--forbid", "crates/forge-content-native/**"],
            diff,
        )
        self.assertEqual(proc.returncode, 2)
        report = report_of(proc)
        self.assertEqual(
            report["violations"],
            [{"path": "crates/forge-content-native/src/lib.rs", "kind": "forbidden"}],
        )

    def test_default_forbid_beats_allow_all_contract(self):
        diff = (
            "diff --git a/.env b/.env\n"
            "--- a/.env\n"
            "+++ b/.env\n"
            "@@ -1,1 +1,2 @@\n"
            " OLD=1\n"
            "+SECRET=hunter2\n"
        )
        contract = FIXTURES / "blast-allow-all.yaml"
        proc = run_blast(["--diff", "--contract", str(contract)], diff)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        report = report_of(proc)
        self.assertEqual(report["allow"], ["**"])
        self.assertEqual(
            report["violations"],
            [{"path": ".env", "kind": "default_forbidden"}],
        )

    def test_rename_and_new_file_headers_and_empty_diff(self):
        diff = (
            "diff --git a/old/name.rs b/new/name.rs\n"
            "similarity index 90%\n"
            "rename from old/name.rs\n"
            "rename to new/name.rs\n"
            "diff --git a/brand.rs b/brand.rs\n"
            "new file mode 100644\n"
            "--- /dev/null\n"
            "+++ b/brand.rs\n"
            "@@ -0,0 +1,1 @@\n"
            "+fn brand() {}\n"
        )
        proc = run_blast(
            ["--diff", "--allow", "old/**", "--allow", "new/**", "--allow", "brand.rs"],
            diff,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        report = report_of(proc)
        self.assertEqual(
            report["changed_paths"], ["brand.rs", "new/name.rs", "old/name.rs"]
        )
        self.assertEqual(report["verdict"], "within_blast_radius")

        # Empty diff: nothing changed, trivially within radius.
        proc = run_blast(["--diff", "--allow", "src/**"], "")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(report_of(proc)["verdict"], "within_blast_radius")

    def test_garbage_stdin_is_usage_error(self):
        for args in (["--diff", "--allow", "src/**"], ["--allow", "src/**"]):
            proc = run_blast(args, "this is neither a diff nor json\n")
            self.assertEqual(proc.returncode, 1)
            self.assertIn("ccx-blast", proc.stderr)


class BlastEnvelopeModeTests(unittest.TestCase):
    def test_pilot_envelope_semantics_unchanged(self):
        envelope = json.dumps({
            "schema_version": "forge.cli.v0",
            "data": {
                "proposal_revision_id": "rev_123",
                "changed_paths": ["src/a.rs", "docs/x.md"],
            },
        })
        proc = run_blast(["--allow", "src/**"], envelope)
        self.assertEqual(proc.returncode, 2)
        report = report_of(proc)
        self.assertEqual(report["revision"], "rev_123")
        self.assertEqual(report["verdict"], "violation")
        self.assertEqual(
            report["violations"],
            [{"path": "docs/x.md", "kind": "outside allowlist"}],
        )

        proc = run_blast(["--allow", "src/**", "--allow", "docs/**"], envelope)
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(report_of(proc)["verdict"], "within_blast_radius")

    def test_envelope_facade_path_is_plain_violation_with_note(self):
        envelope = json.dumps({
            "schema_version": "forge.cli.v0",
            "data": {"snapshot_id": "snap_1", "changed_paths": [FACADE]},
        })
        proc = run_blast(["--allow", "src/**"], envelope)
        self.assertEqual(proc.returncode, 2)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "violation")
        self.assertEqual(report["facade_allowed"], [])
        self.assertEqual(
            report["violations"],
            [{
                "path": FACADE,
                "kind": "outside allowlist",
                "note": "facade path — rerun in --diff mode for line-level facade allowance",
            }],
        )


class BlastReplayPinTests(unittest.TestCase):
    def test_a_382_2_r2_replay(self):
        """Replay pin: the real A-382-2-r2 patch against its real contract.

        The patch touches four contract-allowed paths plus the
        crates/forge-store/src/lib.rs facade, whose only hunk edits lines
        inside an existing multi-line `pub use attempts::{...}` re-export
        block (opener present only as hunk context). The facade hunk must
        be LICENSED via facade_allowed and produce no violation.
        """
        diff = REPLAY_PATCH.read_text(encoding="utf-8")
        proc = run_blast(["--diff", "--contract", str(REPLAY_CONTRACT)], diff)
        self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
        report = report_of(proc)
        self.assertEqual(report["verdict"], "within_blast_radius")
        self.assertEqual(report["violations"], [])
        self.assertEqual(report["facade_allowed"], [FACADE])
        self.assertEqual(
            report["changed_paths"],
            [
                "crates/forge-cli/src/args.rs",
                "crates/forge-cli/src/commands/core.rs",
                "crates/forge-store/src/attempts.rs",
                "crates/forge-store/src/error.rs",
                FACADE,
            ],
        )


if __name__ == "__main__":
    unittest.main()
