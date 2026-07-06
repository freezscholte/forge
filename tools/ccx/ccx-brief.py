#!/usr/bin/env python3
"""ccx thin harness: deterministic brief emitter — no LLM, byte-stable.

Usage:
    ccx-brief.py --contracts-dir <dir> [--global-policy <file>] <contract.yaml>

Emits the global policy + the task contract + its neighbor contracts (one
level deep, in declared order) with `--- SECTION ---` framing. Output is a
pure function of the input file bytes: no timestamps, no environment data,
stable ordering — identical inputs produce identical bytes
(prompt-cache-friendly, reproducible). Source files are emitted VERBATIM,
never re-serialized.

Neighbors come from the contract's parsed YAML `neighbors:` list; id
`ccx-<name>` resolves to `<contracts-dir>/<name>.yaml`. A missing neighbor
file emits the pilot's MISSING marker and still exits 0; a missing contract
or global-policy file is fail-closed: nonzero exit, nothing on stdout.
Exit 0 = brief emitted, 1 = usage/read/parse error.
"""
import argparse
import sys
from pathlib import Path

try:
    import yaml
except ImportError:
    print(
        "ccx-brief: PyYAML is required (install with: python3 -m pip install pyyaml)",
        file=sys.stderr,
    )
    sys.exit(1)


def section(header: str, file_bytes: bytes) -> bytes:
    # Mirrors the pilot brief.sh emit(): header line + file bytes verbatim
    # + one trailing newline.
    return f"--- {header} ---\n".encode() + file_bytes + b"\n"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--contracts-dir", required=True, metavar="DIR")
    ap.add_argument(
        "--global-policy",
        metavar="FILE",
        help="global policy file (default: <contracts-dir>/_global-policy.yaml)",
    )
    ap.add_argument("contract", metavar="CONTRACT_YAML")
    args = ap.parse_args()

    contracts_dir = Path(args.contracts_dir)
    contract = Path(args.contract)
    if not contract.is_file():
        contract = contracts_dir / contract.name
    if not contract.is_file():
        print(f"ccx-brief: no such contract: {args.contract}", file=sys.stderr)
        return 1

    policy = (
        Path(args.global_policy)
        if args.global_policy
        else contracts_dir / "_global-policy.yaml"
    )
    # Fail-closed: buffer everything; stdout is written only on full success.
    try:
        policy_bytes = policy.read_bytes()
    except OSError as err:
        print(f"ccx-brief: cannot read global policy: {err}", file=sys.stderr)
        return 1
    try:
        contract_bytes = contract.read_bytes()
    except OSError as err:
        print(f"ccx-brief: cannot read contract: {err}", file=sys.stderr)
        return 1

    try:
        parsed = yaml.safe_load(contract_bytes)
    except yaml.YAMLError as err:
        print(f"ccx-brief: contract is not valid YAML: {err}", file=sys.stderr)
        return 1
    if not isinstance(parsed, dict):
        print("ccx-brief: contract is not a YAML mapping", file=sys.stderr)
        return 1
    neighbors = parsed.get("neighbors") or []
    if not isinstance(neighbors, list) or not all(
        isinstance(n, str) for n in neighbors
    ):
        print("ccx-brief: neighbors: must be a list of ids", file=sys.stderr)
        return 1

    out = section("GLOBAL POLICY (normative)", policy_bytes)
    out += section("TASK CONTRACT (normative)", contract_bytes)
    for nid in neighbors:
        name = nid[len("ccx-") :] if nid.startswith("ccx-") else nid
        nfile = contracts_dir / f"{name}.yaml"
        if nfile.is_file():
            try:
                nbytes = nfile.read_bytes()
            except OSError as err:
                print(f"ccx-brief: cannot read neighbor {nid}: {err}", file=sys.stderr)
                return 1
            out += section(f"NEIGHBOR CONTRACT (normative): {nid}", nbytes)
        else:
            out += (
                f"--- NEIGHBOR CONTRACT MISSING: {nid} "
                "(surface as unknown, do not guess) ---\n\n"
            ).encode()

    sys.stdout.buffer.write(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
