"""Loads the Rust native module into a real LuaJIT runtime and exercises it.

This is the test that matters for the native module. The module is loaded
into X4's embedded LuaJIT through `package.loadlib`, and it binds the Lua
5.1 C API by resolving symbols from the running interpreter. If that
resolution is wrong, the module crashes the *game* — not a test harness.

So the module is loaded into a genuine LuaJIT runtime (through lupa), the
same way the game loads it, and a real UDP round trip is performed.

Usage:
    python tests/test_native.py [--dll <path>]

Lupa is required. The reference project pins lupa==2.8, which provides
LuaJIT 2.1.
"""

from __future__ import annotations

import argparse
import socket
import sys
import time
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dll",
        default=str(WORKSPACE / "target" / "release" / "x4lan_net.dll"),
    )
    arguments = parser.parse_args()

    library = Path(arguments.dll)
    if not library.is_file():
        print(f"error: {library} not found; build it first", file=sys.stderr)
        return 2

    try:
        from lupa.luajit21 import LuaRuntime
    except ImportError:
        print("error: lupa with LuaJIT 2.1 is required", file=sys.stderr)
        return 2

    lua = LuaRuntime(unpack_returned_tuples=True)
    print(f"Lua runtime: {lua.eval('_VERSION')}")

    # Load the module the same way X4 does: through package.loadlib, with
    # Protected UI mode disabled.
    loader = lua.eval(
        """
        function(path, entry)
            local open = package.loadlib(path, entry)
            if not open then error('package.loadlib failed: ' .. tostring(open)) end
            return open()
        end
        """
    )
    try:
        net = loader(str(library), "luaopen_x4lan_net")
    except Exception as error:  # noqa: BLE001 - report whatever Lua raised
        print(f"error: could not load the native module: {error}", file=sys.stderr)
        return 1
    print(f"loaded {library.name}: open={net.open is not None}")

    # A real UDP round trip, with the module as the client and a plain
    # Python socket as the server.
    server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    server.bind(("127.0.0.1", 0))
    server.settimeout(3.0)
    host, port = server.getsockname()

    client = net.open(host, port)
    if client is None:
        print("error: net.open returned nothing", file=sys.stderr)
        return 1

    payload = b"HELLO|1|" + b"a" * 32
    if not client.send(payload):
        print("error: send returned false", file=sys.stderr)
        return 1
    data, _ = server.recvfrom(768)
    if data != payload:
        print(f"error: server received {data!r}, expected {payload!r}", file=sys.stderr)
        return 1
    print("sent a packet and the server received it intact")

    # The server answers, and the module must receive it.
    reply = b"PONG|1|" + b"b" * 32
    server.sendto(reply, (host, port))
    received = None
    for _ in range(50):
        received = client.receive()
        if received is not None:
            break
        time.sleep(0.02)
    if received is None:
        print("error: the module never received the reply", file=sys.stderr)
        return 1
    if bytes(received) != reply:
        print(f"error: received {received!r}, expected {reply!r}", file=sys.stderr)
        return 1
    print("received the reply and it matched byte for byte")

    # A closed socket must return nil rather than misbehaving.
    client.close()
    if client.receive() is not None:
        print("error: a closed socket returned a packet", file=sys.stderr)
        return 1
    print("a closed socket returns nil")

    # Bad arguments must raise a Lua error, not crash the process.
    for bad in (lambda: net.open("not-an-address", 45841), lambda: net.open(host, 0)):
        try:
            bad()
        except Exception:  # noqa: BLE001 - a Lua error is the expected outcome
            continue
        print("error: an invalid argument was accepted", file=sys.stderr)
        return 1
    print("invalid arguments raise a Lua error")

    server.close()
    print("\nPASS  the native module works inside a real LuaJIT runtime")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
