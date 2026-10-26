"""Runs the real game-side Lua client against the Rust relay over real UDP.

This is the end-to-end proof that the Rust relay is a drop-in replacement:
the actual `client.lua` and `transport.lua` from the game-side extension
drive a real Lua runtime, talking to the real relay over a real socket.

The reference project's own equivalent only runs on Linux, because it loads
the native module through lupa, which links its own LuaJIT and does not
export the C API. Here the module is loaded into a `lua51_64.dll` built
from the Lua 5.1.5 sources, which is the same name X4's Windows build ships,
so the whole path is exercised as the game runs it.

What it proves:

* the relay accepts a join from the real client and issues a lease,
* the relay relays each client's pose to the other,
* the client's own interpolation produces the other player's position,
* and the float rendering the relay emits is one the client accepts.

Usage:
    python tests/test_end_to_end.py [--extension <path>]
"""

from __future__ import annotations

import argparse
import ctypes
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]

TOKEN = "a" * 32
BUILD = "test"
BUBBLE = "bubble"
SECTOR = "sector_macro"
NONCE_1 = "1" * 32
NONCE_2 = "2" * 32


class Lua:
    """A minimal Lua 5.1 binding, via ctypes."""

    def __init__(self, path: Path):
        self.dll = ctypes.CDLL(str(path))
        self.dll.luaL_newstate.restype = ctypes.c_void_p
        self.dll.luaL_openlibs.argtypes = [ctypes.c_void_p]
        self.dll.luaL_loadstring.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
        self.dll.luaL_loadstring.restype = ctypes.c_int
        self.dll.lua_pcall.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
        ]
        self.dll.lua_pcall.restype = ctypes.c_int
        self.dll.lua_tolstring.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.POINTER(ctypes.c_size_t),
        ]
        self.dll.lua_tolstring.restype = ctypes.c_char_p

    def run(self, script: str) -> str:
        state = self.dll.luaL_newstate()
        if not state:
            raise RuntimeError("luaL_newstate returned nothing")
        self.dll.luaL_openlibs(state)
        if self.dll.luaL_loadstring(state, script.encode("utf-8")) != 0:
            raise RuntimeError(f"the script did not compile: {self.error(state)}")
        if self.dll.lua_pcall(state, 0, 1, 0) != 0:
            raise RuntimeError(f"the script raised: {self.error(state)}")
        length = ctypes.c_size_t()
        pointer = self.dll.lua_tolstring(state, -1, ctypes.byref(length))
        if not pointer:
            return ""
        return ctypes.string_at(pointer, length.value).decode("utf-8", "replace")

    def error(self, state: ctypes.c_void_p) -> str:
        length = ctypes.c_size_t()
        pointer = self.dll.lua_tolstring(state, -1, ctypes.byref(length))
        if not pointer:
            return "(no message)"
        return ctypes.string_at(pointer, length.value).decode("utf-8", "replace")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--extension",
        default=str(
            WORKSPACE.parent / "X4-Multiplayer-Source" / "extension" / "x4_lan_lab"
        ),
        help="the x4_lan_lab extension directory",
    )
    parser.add_argument(
        "--lua",
        default=str(WORKSPACE / "target" / "lua" / "lua51_64.dll"),
    )
    parser.add_argument(
        "--relay",
        default=str(WORKSPACE / "target" / "release" / "x4lan-relay.exe"),
    )
    parser.add_argument(
        "--module",
        default=str(WORKSPACE / "target" / "release" / "x4lan_net.dll"),
    )
    arguments = parser.parse_args()

    extension = Path(arguments.extension)
    ui = extension / "ui"
    for required in (ui / "transport.lua", ui / "client.lua"):
        if not required.is_file():
            print(f"error: {required} not found", file=sys.stderr)
            return 2
    for required in (Path(arguments.lua), Path(arguments.relay), Path(arguments.module)):
        if not required.is_file():
            print(f"error: {required} not found; build it first", file=sys.stderr)
            return 2

    # A free port for the relay.
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()

    relay = subprocess.Popen(
        [
            str(arguments.relay),
            "--bind",
            "127.0.0.1",
            "--port",
            str(port),
            "--token",
            TOKEN,
            "--build",
            BUILD,
            "--bubble",
            BUBBLE,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        time.sleep(0.6)
        if relay.poll() is not None:
            print("error: the relay exited immediately", file=sys.stderr)
            print(relay.stdout.read() if relay.stdout else "", file=sys.stderr)
            return 1

        lua = Lua(Path(arguments.lua))
        transport_source = (ui / "transport.lua").read_text(encoding="utf-8")
        client_source = (ui / "client.lua").read_text(encoding="utf-8")

        script = f"""
        X4LAN_NATIVE_PATH = [[{Path(arguments.module).as_posix()}]]
        {transport_source}
        {client_source}

        local first = X4LAN_TRANSPORT.open("127.0.0.1", {port})
        local second = X4LAN_TRANSPORT.open("127.0.0.1", {port})
        local one = X4LAN_CLIENT.new(
            {{token="{TOKEN}", build="{BUILD}", bubble="{BUBBLE}", sector="{SECTOR}", slot="p1"}},
            first, "{NONCE_1}")
        local two = X4LAN_CLIENT.new(
            {{token="{TOKEN}", build="{BUILD}", bubble="{BUBBLE}", sector="{SECTOR}", slot="p2"}},
            second, "{NONCE_2}")

        -- Lua 5.1 has no os.sleep, so the loop is paced with os.clock. On
        -- Windows os.clock is wall-clock time, and the spin keeps the
        -- client's virtual clock in step with the relay's real one.
        local function wait(seconds)
            local target = os.clock() + seconds
            while os.clock() < target do end
        end

        local now = 0
        local sentOne, sentTwo = 0, 0
        for step = 1, 150 do
            now = now + 0.01
            wait(0.01)
            if one:update(now, {{step * 1.0, 0, 0, 0, 0, 0}}) then sentOne = sentOne + 1 end
            if two:update(now, {{-step * 1.0, 0, 0, 0, 0, 0}}) then sentTwo = sentTwo + 1 end
        end

        local seenOne = one:render(now)
        local seenTwo = two:render(now)
        return string.format("sent=%d,%d lease=%d,%d one=%s two=%s",
            sentOne, sentTwo,
            one.lease and 1 or 0, two.lease and 1 or 0,
            seenOne and tostring(seenOne[1]) or "nil",
            seenTwo and tostring(seenTwo[1]) or "nil")
        """
        result = lua.run(script)
        print(f"the clients reported: {result}")
    finally:
        relay.terminate()
        try:
            output = relay.communicate(timeout=5)[0] or ""
        except subprocess.TimeoutExpired:
            relay.kill()
            output = ""

    fields = dict(
        part.split("=", 1) for part in result.split() if "=" in part
    )
    if fields.get("lease") != "1,1":
        print("error: the clients did not both receive a lease", file=sys.stderr)
        return 1
    one = fields.get("one", "nil")
    two = fields.get("two", "nil")
    if one == "nil" or two == "nil":
        print("error: a client never rendered the other player's pose", file=sys.stderr)
        return 1
    if not float(one) < 0 < float(two):
        print(
            f"error: the clients did not see each other: one={one} two={two}",
            file=sys.stderr,
        )
        return 1
    print("each client rendered the other player's position, with the right sign")

    if "p1@" not in output or "p2@" not in output:
        print(f"error: the relay did not report both peers:\n{output}", file=sys.stderr)
        return 1
    print("the relay reported both peers as connected")

    print("\nPASS  the real Lua client and the Rust relay work together")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
