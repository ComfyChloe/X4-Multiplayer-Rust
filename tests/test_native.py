"""Loads the native module into a real Lua 5.1 runtime and exercises it.

This is the test that matters for the native module. The module is loaded
into X4's own Lua runtime through `package.loadlib`, and it binds the Lua
5.1 C API by resolving symbols from that runtime. If the resolution or the
ABI binding is wrong, the module crashes the *game* — not a test harness.

So the module is loaded into a genuine Lua runtime, the same way the game
loads it, and a real UDP round trip is performed.

X4's Windows build ships its Lua runtime as `lua51_64.dll`, and the
reference module resolves its symbols from exactly that name. The same DLL is
built here from the Lua 5.1.5 sources (see `build_lua_reference.bat`) so the
test exercises the same code path.

Usage:
    python tests/test_native.py [--dll <path>] [--lua <path>]

The Lua DLL is required. Lupa is deliberately not used: it links its own
LuaJIT statically and does not export the C API, so the module could not
resolve symbols from it.
"""

from __future__ import annotations

import argparse
import ctypes
import socket
import sys
import threading
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]


class LuaLibrary:
    """A minimal binding to the Lua 5.1 C API, via ctypes."""

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

    def new_state(self) -> ctypes.c_void_p:
        state = self.dll.luaL_newstate()
        if not state:
            raise RuntimeError("luaL_newstate returned nothing")
        self.dll.luaL_openlibs(state)
        return state

    def run(self, state: ctypes.c_void_p, script: str) -> str:
        """Runs a script and returns the string it left on the stack."""
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
        "--dll",
        default=str(WORKSPACE / "target" / "release" / "x4lan_net.dll"),
        help="the native module under test",
    )
    parser.add_argument(
        "--lua",
        default=str(WORKSPACE / "target" / "lua" / "lua51_64.dll"),
        help="the Lua runtime the module is loaded into",
    )
    arguments = parser.parse_args()

    library = Path(arguments.dll)
    if not library.is_file():
        print(f"error: {library} not found; build it first", file=sys.stderr)
        return 2
    lua_path = Path(arguments.lua)
    if not lua_path.is_file():
        print(
            f"error: {lua_path} not found; build it with "
            "tests/build_lua_reference.bat",
            file=sys.stderr,
        )
        return 2

    lua = LuaLibrary(lua_path)
    print(f"loaded the Lua runtime: {lua_path.name}")

    server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    server.bind(("127.0.0.1", 0))
    server.settimeout(3.0)
    host, port = server.getsockname()

    def module_path() -> str:
        return library.as_posix()

    # --- A full round trip, with the server answering in the background ---
    received: list[bytes] = []

    def server_thread() -> None:
        try:
            data, address = server.recvfrom(768)
            received.append(data)
            server.sendto(b"PONG|1|reply", address)
        except OSError:
            pass

    worker = threading.Thread(target=server_thread, daemon=True)
    worker.start()
    state = lua.new_state()
    script = f"""
    local net = package.loadlib([[{module_path()}]], "luaopen_x4lan_net")()
    local socket = net.open("{host}", {port})
    if socket == nil then error("net.open returned nothing") end
    if not socket:send("HELLO|1|probe") then error("send failed") end
    local reply = nil
    for _ = 1, 200 do
        reply = socket:receive()
        if reply ~= nil then break end
    end
    socket:close()
    if reply == nil then return "TIMEOUT" end
    return reply
    """
    result = lua.run(state, script)
    worker.join(timeout=3.0)
    if result != "PONG|1|reply":
        print(f"error: the module received {result!r}", file=sys.stderr)
        return 1
    print("the module sent a packet and received the reply")
    if not received or received[0] != b"HELLO|1|probe":
        print(f"error: the server received {received!r}", file=sys.stderr)
        return 1
    print("the server received the packet the module sent, byte for byte")

    # --- The module may be loaded more than once in one process -----------
    # `package.loadlib` calls the entry point afresh for every new Lua
    # state, so every call has to hand back a working module.
    for attempt in range(3):
        state = lua.new_state()
        script = f"""
        local net = package.loadlib([[{module_path()}]], "luaopen_x4lan_net")()
        if net == nil then return "NIL" end
        if net.open == nil then return "NO_OPEN" end
        return "LOADED"
        """
        result = lua.run(state, script)
        if result != "LOADED":
            print(f"error: load {attempt + 1} returned {result!r}", file=sys.stderr)
            return 1
    print("the module loads repeatedly in one process")

    # --- A closed socket must return nil --------------------------------
    state = lua.new_state()
    script = f"""
    local net = package.loadlib([[{module_path()}]], "luaopen_x4lan_net")()
    local socket = net.open("{host}", {port})
    socket:close()
    if socket:receive() ~= nil then error("a closed socket returned a packet") end
    return "CLOSED_OK"
    """
    if lua.run(state, script) != "CLOSED_OK":
        print("error: a closed socket misbehaved", file=sys.stderr)
        return 1
    print("a closed socket returns nil")

    # --- Bad arguments must raise a Lua error, not crash the process -----
    for bad, description in (
        (f'net.open("not-an-address", {port})', "a non-numeric host"),
        (f'net.open("{host}", 0)', "a port of zero"),
        (f'net.open("{host}", 70000)', "a port above the maximum"),
    ):
        state = lua.new_state()
        script = f"""
        local net = package.loadlib([[{module_path()}]], "luaopen_x4lan_net")()
        local ok = pcall(function() {bad} end)
        if ok then return "ACCEPTED" end
        return "REJECTED"
        """
        if lua.run(state, script) != "REJECTED":
            print(f"error: {description} was accepted", file=sys.stderr)
            return 1
    print("invalid arguments raise a Lua error")

    # --- A message with a percent sign must not be read as a format ------
    state = lua.new_state()
    script = f"""
    local net = package.loadlib([[{module_path()}]], "luaopen_x4lan_net")()
    local ok = pcall(function() net.open("999.999.999.999", {port}) end)
    if ok then return "ACCEPTED" end
    return "REJECTED"
    """
    if lua.run(state, script) != "REJECTED":
        print("error: an invalid host was accepted", file=sys.stderr)
        return 1
    print("an invalid host is rejected")

    server.close()
    print("\nPASS  the native module works inside a real Lua runtime")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
