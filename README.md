# X4 Multiplayer — Rust

A Rust implementation of the dedicated server for the X4: Foundations LAN
multiplayer relay, together with the native Lua module the game-side
extension loads.

The game-side extension (the Lua and Mission Director XML under
`extension/x4_lan_lab/`) is **not** part of this repository's Rust code and is
not rewritten in Rust — it runs inside X4's embedded LuaJIT and has to stay
Lua. This project replaces the *server* half: the process that arbitrates
between the two players.

---

## What this is for

The reference implementation ships as a Python package plus a set of platform
installers. Each installer bundles a complete CPython runtime — the Windows
package embeds a 3.14.8 interpreter, the Linux package unpacks a standalone
build — and each is verified against published SHA-256 digests at install
time.

A Rust build removes that dependency entirely. The result is a single
self-contained binary with no runtime to download, no interpreter to start,
and no platform-specific launcher scripts to maintain.

The protocol itself is unchanged. The game-side Lua extension cannot tell
which server it is talking to.

---

## Layout

| Crate | What it is |
|---|---|
| `x4lan-core` | Protocol, validation and the session state machine |
| `x4lan-relay` | The dedicated server binary |
| `x4lan-native` | The Lua UDP module, replacing `native/net.c` |
| `x4lan-prepare` | Prepares the two client folders for a session |
| `x4lan-conformance` | Differential harness against the Python reference |

The protocol layers mirror the reference's class hierarchy:

```
Session          the plain two-slot pose relay
└── BubbleSession      host-authoritative action routing
    └── EconomySession market snapshots and trading
        └── CoopSession    cooperative ships, health, fire, missions
            └── DockSession    docking reservations
                └── UniverseSession whole-sector world synchronisation
```

Rust has no inheritance, so the chain is explicit: each layer owns its
`impl Session` block and calls the layer below it directly when a packet is
not its own.

---

## Building

```sh
cargo build --workspace --release
```

Produces:

- `target/release/x4lan-relay` — the server
- `target/release/x4lan-prepare` — the session preparation tool
- `target/release/x4lan_net.dll` (Windows) or `libx4lan_net.so` (Linux) —
  the native Lua module

The native module is loaded by the game through `package.loadlib`, which
requires **Protected UI mode** to be disabled in X4's settings. The module
resolves every Lua symbol from the already-loaded interpreter at load time
rather than linking against a Lua library, so it uses the game's own runtime
rather than a second copy.

The game's Lua extension looks for the module under a fixed name —
`x4lan_net_64.dll` on Windows and `x4lan_net.so` on Linux — so the built
module is copied to that name before it is installed alongside the
extension. Cargo's own naming (`x4lan_net.dll`, `libx4lan_net.so`) does not
match.

---

## Why Rust, and why only half of it

X4's modding stack decides this, and it is not a matter of taste.

**The game-side half is Lua today, and it is a constrained position rather than a
permanent one.** X4 runs its own embedded LuaJIT 2.1 and a Mission Director XML engine
inside the game process. The extension's Lua and XML are what the game will load.

But the project is already moving the game side into Rust: `contrib/ferrite-rust-core` in
the upstream repository builds a Rust core that binds X4's own FFI at runtime — 25 exports
resolved by `dlsym` on Linux and `GetProcAddress` on Windows, bound only when the address
and the 16-byte code head match build 23660954. The `contrib/ferrite-rw-*` family
(ecs-core, physics, transforms, lane, walk, combat, economy, ai-driver, ai-orders,
frame-diet) is rewriting the simulation behind that core. The constraint there is not the
language; it is that the binding is pinned to one game build.

**The server half is the right thing to rewrite in Rust.** The reference implementation is
a Python package, and each of its installers bundles a complete CPython runtime: the
Windows package embeds a 3.14.8 interpreter, the Linux package unpacks a standalone build,
and both are verified against published SHA-256 digests at install time. A Rust build
removes that dependency, the interpreter's start-up cost, and the platform-specific
launcher scripts, and leaves one static binary per platform.

The protocol does not change. The game-side Lua cannot tell which server it is talking to
— which is exactly what the conformance harness proves.

The other candidate language was C++, on the strength of the `X4Native` framework. That is
the right tool for a *native extension inside the game*, which is the same problem the
`ferrite-rust-core` branch is solving by hand in Rust.

---

## Running the server

```sh
x4lan-relay --session session.json
```

or without a session file:

```sh
x4lan-relay --token <32 hex> --build <label> --bubble <id>
```

| Option | Meaning |
|---|---|
| `--session <path>` | Session file written by `prepare.py` |
| `--token <hex>` | Shared session token |
| `--build <label>` | Shared build label, identical on both clients |
| `--bubble <id>` | Shared universe identifier |
| `--bind <addr>` | Address to bind (default `127.0.0.1`) |
| `--port <n>` | UDP port (default `45841`) |
| `--bubble-actions` | Enable the experimental test bubble |
| `--trading` | Enable market snapshots and trading |
| `--coop` | Enable cooperative ships |
| `--world-sync` | Enable whole-sector world synchronisation |

---

## Conformance: how "1:1" is actually established

The protocol is byte-exact. A pose is rendered as text, and the two players
must agree on that text or they disagree about where a ship is. A port that
is "nearly right" is wrong.

So the claim is not made by reading both implementations side by side. It is
made by **running identical inputs through both and diffing the outputs**.

```sh
# Build the Rust port and the harness.
cargo build -p x4lan-conformance

# Generate the scenarios.
python conformance/make_scenarios.py

# Compare, one scenario at a time.
python conformance/compare.py conformance/scenarios/simple-two-peers.json

# Or run everything.
for scenario in conformance/scenarios/*.json; do
    python conformance/compare.py "$scenario" || break
done

# And fuzz for the combinations nobody wrote down.
python conformance/fuzz.py 30 --seed 1
```

`conformance/reference.py` replays a scenario through the Python reference;
`x4lan-conformance` replays it through the Rust port; `compare.py` diffs the
two transcripts and prints the first divergence.

Current status: **20 scenarios and 150 fuzz iterations across five seeds pass
with byte-identical transcripts.**

### The other tests

The conformance harness proves the *server* matches the reference. These three
prove the parts that carry the risk of running inside the game.

| Test | What it proves |
|---|---|
| `tests/test_end_to_end.py` | The actual `client.lua` and `transport.lua` from the extension, driving a real LuaJIT 2.1 runtime, talk to the real relay over a real socket. Both clients get a lease and each renders the other player's position. |
| `tests/test_native.py` | The native module loads into LuaJIT through `package.loadlib`, as the game loads it, and completes a UDP round trip in both directions. |
| `tests/test_float_format.py` | The relay renders floats exactly as the reference does *and* exactly as Lua does, over 546 values covering rounding, exponential notation and denormals. |

The end-to-end test is the one that matters most: it is the game's own
client code against this relay, with no shim in between.

The reference project's own equivalent only runs on Linux, because lupa
links its own LuaJIT statically and does not export the C API the native
module needs. Here LuaJIT 2.1 is built from source as `lua51_64.dll` —
the name X4's Windows build ships, and the name the C reference resolves —
so the same code path is exercised as the game runs it.

```sh
# Build LuaJIT as the test runtime (needs the MSVC toolchain).
tests\\build_luajit_reference.bat
cp target/lua/lua51_64.dll ...   # wherever the test expects it

python tests/test_end_to_end.py
python tests/test_native.py
python tests/test_float_format.py
```

### The scenarios

| Scenario | What it pins down |
|---|---|
| `simple-join` | A peer joins and receives a lease |
| `simple-two-peers` | Each peer sees only the *remote* pose |
| `simple-auth-rejects` | Wrong token, build, bubble and slot are all refused |
| `simple-slot-conflicts` | One endpoint cannot take both slots |
| `simple-mismatched-sector` | A peer cannot join a different sector |
| `simple-ordering` | A state frame from the wrong address is ignored; sequence numbers must increase |
| `simple-bounds` | NaN, infinity and out-of-range poses are refused |
| `simple-malformed` | Empty, oversized and malformed packets change nothing |
| `simple-idle` | An idle peer loses its pose; a reordered frame cannot restore it |
| `simple-expiry` | A lease runs out after thirty seconds of silence |
| `simple-ping` | An authenticated readiness check claims no slot |
| `simple-resume-rejected` | A resume with a wrong lease is refused |
| `bubble-action` | An action is queued against the host |
| `bubble-world` | The host publishes world state |
| `economy-order` | A market order is queued and answered |
| `coop-crew` | A pilot claims a ship |
| `coop-nav` | A navigation frame is relayed |
| `coop-health` | A health generation is acknowledged and reported |
| `coop-legacy-frame-rejected` | A legacy movement frame never overrides co-op authority |
| `dock-claim` | A dock reservation is claimed |

---

## Notes on the port

Some details are not obvious, and each one is a place where a port could
silently diverge.

**Float formatting.** Python's `format(x, '.9g')` and Rust's `{}` do not
agree. `0.1 + 0.2` is `0.3` under `.9g` and `0.30000000000000004` under
Rust's default formatting. The port implements the C `printf` `%g` rules that
Python follows, in `x4lan-core::format`.

**Insertion order.** CPython dictionaries preserve insertion order, and the
protocol depends on it: snapshots are emitted in the order peers joined, and
a command is dispatched in the order it was accepted. The port uses an
insertion-ordered map rather than a hash map.

**Validation order.** The reference validates a packet field by field and
returns on the first failure. Several rules interact — a `HELLO` is refused
for the wrong token before the build is even compared — so the port preserves
the order, not just the rules.

**`float()` parsing.** Python's `float()` accepts surrounding whitespace,
underscores between digits, and `inf`/`nan` in any case. The port accepts the
same strings, so both sides agree about which messages are well formed.

**Randomness.** The reference draws a random lease for every new peer. That
is correct in production but makes byte-for-byte comparison impossible, so
the conformance harness turns on a deterministic lease sequence and both
implementations then produce identical output.

---

The upstream project already has a Rust port of its own, on
`contrib/ferrite-rust-relay`. Two ports of the same relay, by two authors, is a liability
rather than an asset; `docs/PORT-COMPARISON.md` sets out which parts of each are worth
keeping. This port's distinguishing property is that it is verified against the Python
reference by differential testing, and that the real game-side client has been run against
it in a real LuaJIT runtime.

## Third-party notices

X4: Foundations is a trademark of Egosoft GmbH. This project is independent
and claims no endorsement. X4, X4: Foundations and related names belong to
their respective owners.

The relay protocol implemented here is derived from the source-inspection
snapshot of the X4 Multiplayer project by the X4 Multiplayer contributors,
which is published under a restricted inspection licence. See
`docs/PROVENANCE.md` for the full position, including why the fork and port
are lawful and what they do *not* authorise.
