"""Compares the relay's float rendering against the reference and against Lua.

The wire protocol renders poses as text, and the game-side Lua renders them
with `string.format("%.9g", value)`. If the relay renders a float
differently, the two sides disagree about where a ship is — the pose is
still a valid number, so nothing fails loudly, it is just wrong.

So the relay's rendering is compared value by value against:

* **Python**, the implementation the relay replaces. This must match
  exactly; it is the 1:1 requirement.
* **Lua**, the implementation on the other end of the wire. This must match
  exactly too, with one documented exception: Python renders negative zero
  as `-0` and Lua renders it as `0`. That is a difference between the two
  languages, not a fault in the port — and it cannot arise on the wire,
  because Lua never sends `-0`, so the relay never holds a negative zero.

The values are chosen to cover the awkward cases: values that need rounding,
values that switch to exponential notation, values that are exactly
representable, values that are not, and denormals.

Usage:
    python tests/test_float_format.py [--lua <path>]
"""

from __future__ import annotations

import argparse
import ctypes
import math
import random
import struct
import subprocess
import sys
from pathlib import Path

WORKSPACE = Path(__file__).resolve().parents[1]


def build_values() -> list[float]:
    """The values both implementations are asked to render."""
    values: list[float] = [
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        0.2,
        0.3,
        0.1 + 0.2,
        1.0 / 3.0,
        2.0 / 3.0,
        1e-5,
        1e-4,
        0.0001,
        0.00012345,
        1e15,
        1e16,
        1e17,
        1e-300,
        1e300,
        123456789.0,
        1234567891.0,
        50000.0,
        50000.0000001,
        -50000.0,
        math.pi,
        -math.pi,
        2.0 * math.pi,
        1.7976931348623157e308,
        5e-324,
        0.5,
        0.25,
        1.5,
        2.5,
        3.14159265358979,
        100.0,
        1000.0,
        10000.0,
        12345.6789,
        0.1 + 0.7,
        1.0 / 7.0,
        2.718281828459045,
        1e-10,
        1e-9,
        1e-8,
        9.99999999999e-5,
        9.999999999999999e15,
    ]
    # A spread of random values, including denormal and near-boundary ones.
    generator = random.Random(20261026)
    for _ in range(400):
        exponent = generator.randint(-300, 300)
        mantissa = generator.uniform(1.0, 10.0)
        values.append(mantissa * (10.0**exponent))
    for _ in range(100):
        # Raw bit patterns, to reach values no decimal generator would hit.
        bits = generator.getrandbits(64)
        value = struct.unpack("<d", struct.pack("<Q", bits))[0]
        if math.isfinite(value):
            values.append(value)
    return values


def lua_format(path: Path, values: list[float]) -> list[str]:
    """Renders the values with Lua's own string.format."""
    library = ctypes.CDLL(str(path))
    library.luaL_newstate.restype = ctypes.c_void_p
    library.luaL_openlibs.argtypes = [ctypes.c_void_p]
    library.luaL_loadstring.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
    library.luaL_loadstring.restype = ctypes.c_int
    library.lua_pcall.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
    ]
    library.lua_pcall.restype = ctypes.c_int
    library.lua_tolstring.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    library.lua_tolstring.restype = ctypes.c_char_p

    state = library.luaL_newstate()
    library.luaL_openlibs(state)

    # Lua reads the values from a table, so the values have to be written with
    # enough digits to survive the round trip.
    literals = ", ".join(repr(value) for value in values)
    script = (
        "local values = {" + literals + "}\n"
        "local out = {}\n"
        "for i = 1, #values do out[i] = string.format('%.9g', values[i]) end\n"
        "return table.concat(out, '\\n')\n"
    )
    if library.luaL_loadstring(state, script.encode("utf-8")) != 0:
        raise RuntimeError("the Lua script did not compile")
    if library.lua_pcall(state, 0, 1, 0) != 0:
        raise RuntimeError("the Lua script raised")
    length = ctypes.c_size_t()
    pointer = library.lua_tolstring(state, -1, ctypes.byref(length))
    text = ctypes.string_at(pointer, length.value).decode("utf-8")
    return text.splitlines()


def relay_format(values: list[float]) -> list[str]:
    """Renders the values with the relay's own implementation."""
    binary = WORKSPACE / "target" / "release" / "x4lan-format.exe"
    if not binary.is_file():
        binary = WORKSPACE / "target" / "debug" / "x4lan-format.exe"
    if not binary.is_file():
        raise SystemExit("build the formatter first: cargo build -p x4lan-conformance")
    payload = "\n".join(repr(value) for value in values)
    result = subprocess.run(
        [str(binary)], input=payload, capture_output=True, text=True, check=True
    )
    return result.stdout.splitlines()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--lua",
        default=str(WORKSPACE / "target" / "lua" / "lua51_64.dll"),
        help="the Lua runtime used for the comparison",
    )
    arguments = parser.parse_args()

    lua_path = Path(arguments.lua)
    if not lua_path.is_file():
        print(
            f"error: {lua_path} not found; build it with tests/build_lua_reference.bat",
            file=sys.stderr,
        )
        return 2

    values = build_values()
    print(f"comparing {len(values)} values")

    actual = relay_format(values)
    if len(actual) != len(values):
        print(
            f"error: the relay produced {len(actual)} renderings for {len(values)} values",
            file=sys.stderr,
        )
        return 1

    # The reference implementation the relay replaces.
    python_expected = ["%.9g" % value for value in values]
    mismatches = 0
    for value, expected, got in zip(values, python_expected, actual):
        if expected != got:
            mismatches += 1
            if mismatches <= 20:
                print(
                    f"  {value!r}: python {expected!r} != relay {got!r}",
                    file=sys.stderr,
                )
    if mismatches:
        print(
            f"\nFAIL  {mismatches} of {len(values)} values differ from the reference",
            file=sys.stderr,
        )
        return 1
    print("the relay renders every value exactly as the reference does")

    # The other end of the wire.
    lua_expected = lua_format(lua_path, values)
    if len(lua_expected) != len(values):
        print(
            f"error: Lua produced {len(lua_expected)} renderings for {len(values)} values",
            file=sys.stderr,
        )
        return 1
    mismatches = 0
    for value, expected, got in zip(values, lua_expected, actual):
        if expected == got:
            continue
        # Python and Lua disagree about negative zero. The relay follows
        # Python, which is the implementation it replaces.
        if math.copysign(1.0, value) < 0 and value == 0.0 and expected == "0" and got == "-0":
            continue
        mismatches += 1
        if mismatches <= 20:
            print(
                f"  {value!r}: lua {expected!r} != relay {got!r}",
                file=sys.stderr,
            )
    if mismatches:
        print(
            f"\nFAIL  {mismatches} of {len(values)} values differ from Lua",
            file=sys.stderr,
        )
        return 1
    print("the relay renders every value exactly as Lua does")
    print("\nPASS  the relay's float rendering matches both implementations")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
