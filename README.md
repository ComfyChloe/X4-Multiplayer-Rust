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
- `target/release/x4lan_net.dll` (Windows) or `libx4lan_net.so` (Linux) —
  the native Lua module

The native module is loaded by the game through `package.loadlib`, which
requires **Protected UI mode** to be disabled in X4's settings. The module
resolves every Lua symbol from the already-loaded interpreter at load time
rather than linking against a Lua library, so it uses the game's own runtime
rather than a second copy.

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

## Third-party notices

X4: Foundations is a trademark of Egosoft GmbH. This project is independent
and claims no endorsement. X4, X4: Foundations and related names belong to
their respective owners.

The relay protocol implemented here is derived from the source-inspection
snapshot of the X4 Multiplayer project by the X4 Multiplayer contributors,
which is published under a restricted inspection licence. See
`docs/PROVENANCE.md` for the full position, including why the fork and port
are lawful and what they do *not* authorise.
