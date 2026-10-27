"""Randomised differential fuzzer.

Generates random but valid-ish packet sequences, replays them through both
the Python reference and the Rust port, and diffs the transcripts.

The hand-written scenarios in `make_scenarios.py` cover known behaviours.
This fuzzer covers the *combinations* nobody thought to write down: a state
frame after a lease expiry, a resume with a stale nonce, a co-op frame with
the wrong epoch, and so on.

Usage:
    python conformance/fuzz.py [iterations] [--seed 12345]
"""

from __future__ import annotations

import argparse
import json
import random
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORKSPACE = HERE.parent
DEFAULT_REFERENCE = WORKSPACE.parent / "X4-Multiplayer-Source"

TOKEN = "a" * 32
BUILD = "test"
BUBBLE = "bubble"
SECTOR = "sector_macro"
ADDRESSES = ["127.0.0.1:10001", "127.0.0.1:10002", "127.0.0.1:10003"]
NONCE_1 = "1" * 32
NONCE_2 = "2" * 32

MODES = ["simple", "bubble", "economy", "coop", "dock"]

# The reference hands out leases from a counter, so the port must see the
# same sequence. These are the leases the first joins receive.
LEASE_1 = "f" * 32
LEASE_2 = "e" * 32


def random_nonce(rng: random.Random) -> str:
    return f"{rng.randrange(1, 1 << 64):016x}" * 2


def random_packets(rng: random.Random, mode: str, count: int) -> list[dict]:
    """Builds a random packet sequence."""
    steps: list[dict] = []
    for index in range(count):
        at = round(rng.uniform(0.0, 40.0), 3)
        sender = rng.choice(ADDRESSES)
        packet = rng.choice(
            [
                lambda: f"HELLO|1|{TOKEN}|{BUILD}|{SECTOR}|{BUBBLE}|{rng.choice(['p1', 'p2', 'p3'])}|{random_nonce(rng)}",
                lambda: f"HELLO|1|{TOKEN}|{BUILD}|{SECTOR}|{BUBBLE}|p1|{NONCE_1}",
                lambda: f"HELLO|1|{TOKEN}|{BUILD}|{SECTOR}|{BUBBLE}|p2|{NONCE_2}",
                lambda: f"PING|1|{TOKEN}|{random_nonce(rng)}",
                lambda: f"PING|1|{'b' * 32}|{random_nonce(rng)}",
                lambda: f"STATE|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.randrange(0, 100)}|"
                + "|".join(
                    str(rng.choice([0.0, 1.5, -3.25, 1e10, 0.1 + 0.2, 1e-9, float("nan"), 12345.6789]))
                    for _ in range(6)
                ),
                lambda: f"IDLE|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.randrange(0, 100)}",
                lambda: f"RESUME|1|{TOKEN}|{BUILD}|{SECTOR}|{BUBBLE}|{rng.choice(['p1', 'p2'])}|"
                f"{random_nonce(rng)}|{rng.choice([LEASE_1, LEASE_2])}|{random_nonce(rng)}",
                lambda: f"CREW|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.randrange(0, 50)}|"
                f"{rng.choice([0, 1, 2])}|{rng.choice([0, 1, 2])}",
                lambda: f"NAV|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.choice([1, 2])}|{rng.randrange(0, 50)}|"
                f"{rng.choice([1, 2])}|{rng.choice([1, 2])}|ship_arg_s_scout_01_a_macro|"
                + "|".join(str(rng.uniform(-1000, 1000)) for _ in range(6))
                + f"|{rng.choice([0, 1])}|{rng.randrange(0, 50)}",
                lambda: f"HEALTH|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.choice([1, 2])}|"
                f"{rng.randrange(0, 50)}|{rng.uniform(0, 100)}|{rng.uniform(0, 100)}",
                lambda: f"CDOCK|1|{rng.choice([LEASE_1, LEASE_2])}|{rng.choice([1, 2])}|{rng.randrange(0, 5)}|"
                f"{rng.randrange(0, 50)}|{rng.choice(['APPROACH', 'EXTERNAL', 'STORED', 'FREE', 'UNKNOWN'])}|"
                f"{rng.choice([0, 1, 42, 1000])}",
                lambda: f"ACTION|1|{TOKEN}|{rng.choice(['p1', 'p2'])}|{random_nonce(rng)}|"
                f"{rng.choice(['PICKUP', 'TRADE', 'BOGUS'])}|{rng.choice(['energycells', 'other'])}",
                lambda: f"ORDER|1|{LEASE_2}|{random_nonce(rng)}|{LEASE_1}|"
                f"{rng.choice(['m1', 'm2'])}|energycells,{rng.randrange(1, 100)},{rng.randrange(1, 100)}",
                lambda: f"ECON|1|{LEASE_1}|{rng.randrange(1, 10)}|1|1|M,100,200,300,400;W,energycells,"
                f"{rng.randrange(1, 1000)},1,1,10,10,5,10,5,10,5",
                lambda: "GARBAGE|1|nonsense",
                lambda: "",
            ]
        )()
        step = {"at": at, "packet": packet, "from": sender}
        if rng.random() < 0.4:
            step["snapshot"] = True
        steps.append(step)
    return steps


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("iterations", nargs="?", type=int, default=50)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument(
        "--reference",
        default=str(DEFAULT_REFERENCE),
        help="path to the Python reference implementation",
    )
    arguments = parser.parse_args()

    reference = Path(arguments.reference).resolve()

    rng = random.Random(arguments.seed)
    failures = 0
    with tempfile.TemporaryDirectory(prefix="x4lan-fuzz-") as temporary:
        directory = Path(temporary)
        for iteration in range(arguments.iterations):
            mode = rng.choice(MODES)
            steps = random_packets(rng, mode, rng.randrange(2, 14))
            content = {
                "mode": mode,
                "token": TOKEN,
                "build": BUILD,
                "bubble": BUBBLE,
                "steps": steps,
            }
            scenario = directory / f"fuzz-{iteration}.json"
            scenario.write_text(json.dumps(content, indent=2) + "\n", encoding="utf-8")

            result = subprocess.run(
                [
                    sys.executable,
                    str(HERE / "compare.py"),
                    str(scenario),
                    "--reference",
                    str(reference),
                ],
                capture_output=True,
                text=True,
            )
            if result.returncode != 0:
                failures += 1
                print(f"FAIL  iteration {iteration} (mode={mode}, seed={arguments.seed})")
                print(result.stdout[-3000:])
                print(result.stderr[-3000:])
                # Keep the failing scenario for inspection.
                keep = HERE / "scenarios" / f"fuzz-failure-{arguments.seed}-{iteration}.json"
                keep.write_text(scenario.read_text(encoding="utf-8"), encoding="utf-8")
                print(f"      saved to {keep}")
                if failures >= 3:
                    break

    print(f"fuzz: {arguments.iterations} iterations, {failures} failures (seed {arguments.seed})")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
