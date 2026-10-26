"""Differential conformance runner.

Replays a scenario through the **Python reference implementation** and writes a
transcript. The Rust port replays the same scenario and writes its own
transcript. The two are then compared byte for byte.

This is the only honest way to claim the Rust port is 1:1 with the Python one.
Reading both implementations side by side and asserting they "look the same"
is not evidence; running identical inputs through both and diffing the outputs
is.

Usage:
    python conformance/reference.py <scenario.json> <transcript.json>

The reference implementation is expected at --reference (default
../X4-Multiplayer-Source), and is imported read-only. Nothing in that tree is
modified or executed beyond importing its modules.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load_reference(root: Path):
    """Imports the reference relay's modules."""
    sys.path.insert(0, str(root))
    from x4lan import bubble, coop, docking, economy, host, missions, universe  # noqa: F401
    from x4lan import cargo, identity, npc_combat  # noqa: F401

    return host, bubble, economy, coop, docking, universe


def build_session(mode: str, token: str, build: str, bubble: str, modules):
    """Builds the reference session for a mode."""
    host, bubble_module, economy_module, coop_module, docking_module, universe_module = modules
    if mode == "simple":
        return host.Session(token, build, bubble)
    if mode == "bubble":
        return bubble_module.BubbleSession(token, build, bubble)
    if mode == "economy":
        return economy_module.EconomySession(token, build, bubble)
    if mode == "coop":
        return coop_module.CoopSession(token, build, bubble)
    if mode == "dock":
        return docking_module.DockSession(token, build, bubble)
    if mode == "universe":
        return universe_module.UniverseSession(token, build, bubble)
    raise SystemExit(f"unknown mode: {mode}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario")
    parser.add_argument("transcript")
    parser.add_argument(
        "--reference",
        default=str(Path(__file__).resolve().parents[1] / "X4-Multiplayer-Source"),
        help="path to the Python reference implementation",
    )
    arguments = parser.parse_args()

    root = Path(arguments.reference).resolve()
    if not (root / "x4lan" / "host.py").is_file():
        print(f"error: no reference implementation at {root}", file=sys.stderr)
        return 2

    scenario = json.loads(Path(arguments.scenario).read_text(encoding="utf-8"))
    modules = load_reference(root)
    session = build_session(
        scenario["mode"], scenario["token"], scenario["build"], scenario["bubble"], modules
    )

    # Leases must match the Rust port's sequence, so the reference draws them
    # from a counter instead of the system CSPRNG.
    counter = {"value": 0}

    def deterministic_lease(nbytes: int = 16) -> str:
        counter["value"] += 1
        return f"{counter['value']:032x}"

    import secrets

    secrets.token_hex = deterministic_lease

    events = []
    for step in scenario.get("steps", []):
        at = float(step.get("at", 0.0))
        packet = step.get("packet")
        if packet is not None:
            host_address = step.get("from", "127.0.0.1:10001")
            address = (host_address.split(":")[0], int(host_address.split(":")[1]))
            for response in session.receive(packet.encode("ascii"), address, at):
                events.append(
                    {
                        "at": at,
                        "kind": "reply",
                        "to": host_address,
                        "payload": response.decode("ascii"),
                    }
                )
        if step.get("snapshot"):
            for address, snapshot in session.snapshots(at):
                events.append(
                    {
                        "at": at,
                        "kind": "snapshot",
                        "to": f"{address[0]}:{address[1]}",
                        "payload": snapshot.decode("ascii"),
                    }
                )

    Path(arguments.transcript).write_text(
        json.dumps(events, indent=2) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
