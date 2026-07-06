"""Tests for tools/ccx/ccx-brief.py (unit U2, thin-harness plan).

Stdlib unittest only. Runnable from the repo root via:
    python3 -m unittest discover -s tools/ccx/tests -p 'test_brief.py'
"""
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TESTS_DIR = Path(__file__).resolve().parent
FIXTURES = TESTS_DIR / "fixtures"
CCX_DIR = TESTS_DIR.parent
REPO_ROOT = CCX_DIR.parent.parent
BRIEF_PY = CCX_DIR / "ccx-brief.py"
PILOT_DIR = REPO_ROOT / "experiments" / "ccx"
PILOT_CONTRACTS = PILOT_DIR / "contracts"


def run_brief(*args):
    return subprocess.run(
        [sys.executable, str(BRIEF_PY), *args],
        capture_output=True,
    )


class TestBrief(unittest.TestCase):
    def test_happy_path_framing_and_order(self):
        res = run_brief(
            "--contracts-dir", str(FIXTURES), str(FIXTURES / "brief-task.yaml")
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        out = res.stdout

        policy = (FIXTURES / "_global-policy.yaml").read_bytes()
        task = (FIXTURES / "brief-task.yaml").read_bytes()
        na = (FIXTURES / "brief-neighbor-a.yaml").read_bytes()
        nb = (FIXTURES / "brief-neighbor-b.yaml").read_bytes()
        expected = (
            b"--- GLOBAL POLICY (normative) ---\n" + policy + b"\n"
            b"--- TASK CONTRACT (normative) ---\n" + task + b"\n"
            b"--- NEIGHBOR CONTRACT (normative): ccx-brief-neighbor-a ---\n"
            + na
            + b"\n"
            b"--- NEIGHBOR CONTRACT (normative): ccx-brief-neighbor-b ---\n"
            + nb
            + b"\n"
        )
        self.assertEqual(out, expected)

    def test_byte_stability(self):
        args = ("--contracts-dir", str(FIXTURES), str(FIXTURES / "brief-task.yaml"))
        first = run_brief(*args)
        second = run_brief(*args)
        self.assertEqual(first.returncode, 0)
        self.assertEqual(second.returncode, 0)
        self.assertEqual(first.stdout, second.stdout)

    def test_missing_neighbor_emits_marker_exit_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            for f in FIXTURES.iterdir():
                shutil.copy(f, tmpdir / f.name)
            task = tmpdir / "brief-task.yaml"
            task.write_text(
                task.read_text().replace(
                    "ccx-brief-neighbor-b", "ccx-brief-neighbor-ghost"
                )
            )
            res = run_brief("--contracts-dir", str(tmpdir), str(task))
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn(
            b"--- NEIGHBOR CONTRACT MISSING: ccx-brief-neighbor-ghost "
            b"(surface as unknown, do not guess) ---\n\n",
            res.stdout,
        )
        # The present neighbor is still emitted normally.
        self.assertIn(
            b"--- NEIGHBOR CONTRACT (normative): ccx-brief-neighbor-a ---\n",
            res.stdout,
        )

    def test_missing_contract_fails_closed(self):
        res = run_brief("--contracts-dir", str(FIXTURES), "no-such-contract.yaml")
        self.assertNotEqual(res.returncode, 0)
        self.assertEqual(res.stdout, b"")

    def test_missing_global_policy_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            for f in FIXTURES.iterdir():
                if f.name != "_global-policy.yaml":
                    shutil.copy(f, tmpdir / f.name)
            res = run_brief(
                "--contracts-dir", str(tmpdir), str(tmpdir / "brief-task.yaml")
            )
        self.assertNotEqual(res.returncode, 0)
        self.assertEqual(res.stdout, b"")

    def test_pilot_regression_pin_byte_identical(self):
        """Characterization: byte-identical to the pilot brief.sh output."""
        contract = PILOT_CONTRACTS / "task-382-2-drift-guard.yaml"
        self.assertTrue(contract.is_file(), f"missing pilot contract: {contract}")
        pilot = subprocess.run(
            ["bash", str(PILOT_DIR / "brief.sh"), str(contract)],
            capture_output=True,
        )
        self.assertEqual(pilot.returncode, 0, pilot.stderr)
        ours = run_brief("--contracts-dir", str(PILOT_CONTRACTS), str(contract))
        self.assertEqual(ours.returncode, 0, ours.stderr)
        self.assertEqual(ours.stdout, pilot.stdout)


if __name__ == "__main__":
    unittest.main()
