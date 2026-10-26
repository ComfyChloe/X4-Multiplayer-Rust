"""Compares the Python and Rust transcripts for a scenario.

Usage:
    python conformance/compare.py <scenario.json> [--reference <path>]

Runs both implementations, diffs the transcripts, and exits non-zero on any
difference. The diff is printed so a mismatch is immediately actionable.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKSPACE = HERE.parent


def run_reference(scenario: Path, transcript: Path, reference: Path) -> None:
    subprocess.run(
        [
            sys.executable,
            str(HERE / "reference.py"),
            str(scenario),
            str(transcript),
            "--reference",
            str(reference),
        ],
        check=True,
    )


def run_rust(scenario: Path, transcript: Path) -> None:
    binary = WORKSPACE / "target" / "debug" / "x4lan-conformance.exe"
    if not binary.is_file():
        binary = WORKSPACE / "target" / "release" / "x4lan-conformance.exe"
    if not binary.is_file():
        raise SystemExit("build the Rust port first: cargo build -p x4lan-conformance")
    subprocess.run([str(binary), str(scenario), str(transcript)], check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario")
    parser.add_argument(
        "--reference",
        default=str(WORKSPACE.parent / "X4-Multiplayer-Source"),
        help="path to the Python reference implementation",
    )
    arguments = parser.parse_args()

    scenario = Path(arguments.scenario).resolve()
    work = scenario.with_suffix("")
    work.mkdir(parents=True, exist_ok=True)
    python_transcript = work / "python.json"
    rust_transcript = work / "rust.json"

    run_reference(scenario, python_transcript, Path(arguments.reference).resolve())
    run_rust(scenario, rust_transcript)

    expected = json.loads(python_transcript.read_text(encoding="utf-8"))
    actual = json.loads(rust_transcript.read_text(encoding="utf-8"))

    if expected == actual:
        print(f"PASS  {scenario.name}: {len(actual)} events match exactly")
        return 0

    print(f"FAIL  {scenario.name}: transcripts differ", file=sys.stderr)
    for index in range(max(len(expected), len(actual))):
        left = expected[index] if index < len(expected) else None
        right = actual[index] if index < len(actual) else None
        if left != right:
            print(f"  event {index}:", file=sys.stderr)
            print(f"    python: {json.dumps(left)}", file=sys.stderr)
            print(f"    rust:   {json.dumps(right)}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
