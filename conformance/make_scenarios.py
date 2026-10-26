"""Generates the conformance scenarios.

The scenarios are written by hand rather than randomised, because each one
targets a specific protocol behaviour: authentication, ordering, lease
expiry, resume, bounds checking and each protocol layer's own messages. A
random fuzzer would cover the same ground less deliberately and would produce
failures that are hard to read.

Usage:
    python conformance/make_scenarios.py [output-directory]
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

TOKEN = "a" * 32
NONCE_1 = "1" * 32
NONCE_2 = "2" * 32
NONCE_3 = "3" * 32
LEASE_1 = "f" * 32
LEASE_2 = "e" * 32
BUILD = "test"
# World synchronisation uses a distinct build label, as the reference requires.
WORLD_BUILD = "world1-test"
BUBBLE = "bubble"
SECTOR = "sector_macro"

P1 = "127.0.0.1:10001"
P2 = "127.0.0.1:10002"
FOREIGN = "127.0.0.1:10003"


def hello(slot="p1", nonce=NONCE_1, token=TOKEN, build=BUILD, sector=SECTOR, bubble=BUBBLE):
    return f"HELLO|1|{token}|{build}|{sector}|{bubble}|{slot}|{nonce}"


def state(lease, seq=1, pose=(1, 2, 3, 0, 0, 0)):
    return f"STATE|1|{lease}|{seq}|" + "|".join(map(str, pose))


def idle(lease, seq=1):
    return f"IDLE|1|{lease}|{seq}"


def scenario(mode, steps, build=BUILD):
    return {
        "mode": mode,
        "token": TOKEN,
        "build": build,
        "bubble": BUBBLE,
        "steps": steps,
    }


def step(at, packet=None, sender=P1, snapshot=False):
    entry = {"at": at}
    if packet is not None:
        entry["packet"] = packet
        entry["from"] = sender
    if snapshot:
        entry["snapshot"] = True
    return entry


def scenarios():
    """Yields every scenario, as (name, scenario) pairs."""
    # --- The base two-slot pose protocol -------------------------------
    yield (
        "simple-join",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.0, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-two-peers",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(0.1, state(LEASE_1, 1, (10, 20, 30, 0.1, 0.2, 0.3))),
                step(0.1, state(LEASE_2, 1, (-10, -20, -30, -0.1, -0.2, -0.3)), sender=P2),
                step(0.2, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-auth-rejects",
        scenario(
            "simple",
            [
                step(0.0, hello(token="b" * 32)),
                step(0.0, hello(build="other")),
                step(0.0, hello(bubble="else")),
                step(0.0, hello(slot="p3")),
                step(0.0, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-slot-conflicts",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                # The same endpoint may not take the other slot.
                step(0.1, hello("p2", NONCE_2)),
                # A different endpoint may not steal an occupied slot.
                step(0.2, hello("p1", NONCE_2), sender=P2),
                step(0.3, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-mismatched-sector",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, hello("p2", NONCE_2, sector="other"), sender=P2),
                step(0.2, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-ordering",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                # A state frame from an address that does not own the lease
                # must be ignored.
                step(0.1, state(LEASE_1, 3), sender=FOREIGN),
                step(0.2, state(LEASE_1, 3)),
                # A lower sequence number must not overwrite a higher one.
                step(0.3, state(LEASE_1, 2, (99, 0, 0, 0, 0, 0))),
                step(0.4, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-bounds",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, state(LEASE_1, 1, (float("nan"), 0, 0, 0, 0, 0))),
                step(0.2, state(LEASE_1, 2, (float("inf"), 0, 0, 0, 0, 0))),
                step(0.3, state(LEASE_1, 3, (50001, 0, 0, 0, 0, 0))),
                step(0.4, state(LEASE_1, 4, (0, 0, 0, 10, 0, 0))),
                step(0.5, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-malformed",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, ""),
                step(0.2, "STATE|1"),
                step(0.3, "HELLO|2|bad"),
                step(0.4, "HELLO|1|" + "a" * 700),
                step(0.5, "a" * 769),
                step(0.6, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-idle",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, state(LEASE_1, 2)),
                step(0.2, idle(LEASE_1, 3)),
                # A reordered state frame must not restore the pose.
                step(0.3, state(LEASE_1, 2)),
                step(0.4, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-expiry",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, state(LEASE_1, 1)),
                # The lease runs out after 30 seconds of silence.
                step(29.999, hello()),
                step(30.0, snapshot=True),
            ],
        ),
    )

    yield (
        "simple-ping",
        scenario(
            "simple",
            [
                step(0.0, f"PING|1|{TOKEN}|{NONCE_1}"),
                step(0.1, f"PING|1|{'b' * 32}|{NONCE_1}"),
            ],
        ),
    )

    yield (
        "simple-resume-rejected",
        scenario(
            "simple",
            [
                step(0.0, hello()),
                step(0.1, f"RESUME|1|{TOKEN}|{BUILD}|{SECTOR}|{BUBBLE}|p1|{NONCE_1}|{LEASE_1}|{NONCE_2}"),
                step(0.2, snapshot=True),
            ],
        ),
    )

    # --- The test bubble -------------------------------------------------
    yield (
        "bubble-action",
        scenario(
            "bubble",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(
                    0.1,
                    f"ACTION|1|{TOKEN}|p1|{'1' * 32}|PICKUP|energycells",
                ),
                step(0.2, snapshot=True),
            ],
        ),
    )

    yield (
        "bubble-world",
        scenario(
            "bubble",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                # A WORLD frame: 17 values, as the host publishes them.
                step(
                    0.1,
                    "WORLD|1|"
                    + LEASE_1
                    + "|"
                    + "|".join(["50", "50", "1", "50", "50", "1"] * 2)
                    + "|0|0|0|0|0|0|0",
                ),
                step(0.2, snapshot=True),
            ],
        ),
    )

    # --- The economy layer -----------------------------------------------
    yield (
        "economy-order",
        scenario(
            "economy",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(
                    0.1,
                    f"ORDER|1|{LEASE_2}|{'1' * 32}|{LEASE_1}|m1|energycells,10,5",
                ),
                step(0.2, snapshot=True),
            ],
        ),
    )

    # --- The co-op layer --------------------------------------------------
    yield (
        "coop-crew",
        scenario(
            "coop",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(0.1, f"CREW|1|{LEASE_1}|1|1|1", sender=P1),
                step(0.2, f"CREW|1|{LEASE_2}|1|2|2", sender=P2),
                step(0.3, snapshot=True),
            ],
        ),
    )

    yield (
        "coop-nav",
        scenario(
            "coop",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                # A NAV frame: entity, epoch, seq, macro, pose, pilot, order.
                step(
                    0.1,
                    f"NAV|1|{LEASE_1}|1|1|1|ship_arg_s_scout_01_a_macro"
                    "|100|200|300|0.1|0.2|0.3|1|1",
                    sender=P1,
                ),
                step(0.2, snapshot=True),
            ],
        ),
    )

    yield (
        "coop-legacy-frame-rejected",
        scenario(
            "coop",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                # A legacy movement frame must never override co-op authority.
                step(0.1, state(LEASE_1, 1)),
                step(0.2, snapshot=True),
            ],
        ),
    )

    yield (
        "coop-health",
        scenario(
            "coop",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(0.1, f"CREW|1|{LEASE_1}|1|1|1", sender=P1),
                step(0.2, f"HACK|1|{LEASE_1}|1|1", sender=P1),
                step(0.3, f"HEALTH|1|{LEASE_1}|1|1|90.0|80.0", sender=P1),
                step(0.4, snapshot=True),
            ],
        ),
    )

    # --- The docking layer -------------------------------------------------
    yield (
        "dock-claim",
        scenario(
            "dock",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                step(0.1, f"CREW|1|{LEASE_1}|1|1|1", sender=P1),
                step(0.2, f"CDOCK|1|{LEASE_1}|1|1|2|EXTERNAL|42", sender=P1),
                step(0.3, snapshot=True),
            ],
        ),
    )


    # --- The universe layer -------------------------------------------------
    # The universe layer had no coverage until a divergence turned up in its
    # identity batching, so it gets a scenario of its own.
    yield (
        "universe-interest",
        scenario(
            "universe",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                # The guest declares the sector it is watching.
                step(0.1, f"UINTEREST|1|{LEASE_2}|1|sector_macro", sender=P2),
                # The host publishes an identity for one of the co-op ships.
                step(
                    0.2,
                    f"IDENT|1|{LEASE_1}|{LEASE_1}|1|P|1|ABCD|{'61'.encode().hex()}|1|0",
                    sender=P1,
                ),
                step(0.3, snapshot=True),
            ],
            build=WORLD_BUILD,
        ),
    )

    yield (
        "universe-rejects-foreign-identity",
        scenario(
            "universe",
            [
                step(0.0, hello()),
                step(0.0, hello("p2", NONCE_2), sender=P2),
                # An identity whose scope is not one of the three the relay knows.
                step(
                    0.1,
                    f"IDENT|1|{LEASE_1}|{LEASE_1}|1|Z|1|ABCD|{'61'.encode().hex()}|1|0",
                    sender=P1,
                ),
                # A name that is not valid hex.
                step(
                    0.2,
                    f"IDENT|1|{LEASE_1}|{LEASE_1}|2|P|1|ABCD|nothex|1|0",
                    sender=P1,
                ),
                # An entity outside the range the scope allows.
                step(
                    0.3,
                    f"IDENT|1|{LEASE_1}|{LEASE_1}|3|P|9|ABCD|{'61'.encode().hex()}|1|0",
                    sender=P1,
                ),
                step(0.4, snapshot=True),
            ],
            build=WORLD_BUILD,
        ),
    )


def main() -> int:
    directory = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parent / "scenarios"
    directory.mkdir(parents=True, exist_ok=True)
    count = 0
    for name, content in scenarios():
        path = directory / f"{name}.json"
        path.write_text(json.dumps(content, indent=2) + "\n", encoding="utf-8")
        count += 1
    print(f"wrote {count} scenarios to {directory}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
