# Two Rust ports: this one and the one already in the project

There are now two independent Rust ports of the same relay:

| | This port | `contrib/ferrite-rust-relay` (upstream) |
|---|---|---|
| Where | `ComfyChloe/X4-Multiplayer-Rust` | inside `Lith7ium/X4-Multiplayer` |
| Base | the 7.5.0 source snapshot | `contrib/astra-7-5-delivery` @ 8a56415 (7.5.0) |
| Crates | `x4lan-core`, `x4lan-relay`, `x4lan-native`, `x4lan-prepare`, `x4lan-conformance` | `rust/x4lan-net`, `rust/x4lan-relay` |
| Third-party crates | none | none |
| Native module | `x4lan-native` (cdylib) | `rust/x4lan-net` (cdylib) |
| Lua ABI resolution | `GetModuleHandleA("lua51_64.dll")` + `GetProcAddress`, then a scan of the process's modules | `GetModuleHandleA("lua51_64.dll")` + `GetProcAddress`; `dlsym(RTLD_DEFAULT, ...)` elsewhere |
| Verification | 22 differential scenarios + a randomised fuzzer against the Python reference; the real `client.lua` against the Rust relay in real LuaJIT 2.1 | `parity/native_parity.py` (17 scenarios, C DLL vs Rust DLL), `parity/lua_e2e.py` (4 combinations), `parity/relay_parity.py` (deterministic replay) |
| Tested in X4 | no | no — "nothing in this directory has been tested in X4" |
| CI | GitHub Actions: build, test, clippy `-D warnings`, fmt | — |

They implement the same protocol, and they agree about the awkward parts: no third-party
crates, the Lua ABI resolved from the running interpreter rather than linked, and
`x4lan_net_64.dll` / `x4lan_net.so` as the installed names.

## Where the upstream port is stronger

**The native module parity harness is much broader.** `parity/native_parity.py` compares the
shipped C DLL against the Rust one across 17 scenarios in a real LuaJIT 2.1 runtime
registered as `lua51_64.dll`: GC, closed connections, ICMP reset after the peer closes, an
unresolvable ABI, datagrams of 0/4/768/769/2000 bytes, foreign senders. This port's
`tests/test_native.py` covers a real round trip, repeated loads, a closed socket, and invalid
arguments — useful, but narrower.

**The protocol lives in the type system.** Upstream parses fields into `Hex32`, `Atom`, `Slot`
and `Ship` newtypes, makes the lifecycles enums, and models `CargoVerb::Drop` /
`CargoVerb::Pickup` so the Python rule is structural. It also states the `"C-unwind"` ABI at
every Lua entry point, because `luaL_error` unwinds through the frame — a question Rust
forces you to answer and C lets you ignore. This port keeps the protocol as validated
strings and integers, which is faithful but less expressive.

**It is already in the project.** It can be merged, extended and shipped by the person who
already owns the tree.

## Where this port is stronger

**The differential harness diffs the reference's own transcripts.** `conformance/compare.py`
replays a scenario through the unmodified Python reference and through the Rust port and
diffs the two transcripts byte for byte. `conformance/fuzz.py` does the same with randomly
generated command streams. Upstream's `relay_parity.py` is a comparable idea, with a
deterministic replay driver; the fuzzer is the part that is missing.

**The float rendering is checked against two implementations.** `tests/test_float_format.py`
compares the relay's rendering against the reference's *and* against Lua's, over 546 values
covering rounding, exponential notation and denormals. A pose that renders differently on the
two ends is a silent disagreement about where a ship is, so this is worth more than it
looks.

**The end-to-end test uses the real game client.** `tests/test_end_to_end.py` loads the
extension's own `client.lua` and `transport.lua` into a real LuaJIT 2.1 runtime, points them
at the Rust relay, and checks that both clients get a lease and each renders the other's
position. Upstream's `lua_e2e.py` does the same thing, and does it across all four
C/Rust combinations — which is better coverage than this port's single combination.

**CI holds the line.** Clippy with `-D warnings`, `cargo fmt --check`, and the conformance
suite run on every push.

**Bugs this port found in itself, that the differential harness caught:**

- The native module resolved Lua symbols with a non-NUL-terminated slice. `GetProcAddress`
  read past the end and returned a garbage address; calling it was an access violation, in
  the game process, not in a test.
- The resolved Lua API was cached with `OnceLock::set`, which fails on a second call, so
  loading the module into a second Lua state returned nothing.
- The identity snapshot limit had its bounds in the wrong order, so it sent a tenth of the
  rows the reference sends. The universe layer had no conformance scenario, which is why it
  survived.
- The reference refuses to start a universe session without a `world1-` build label. This
  port accepted any label.
- The co-op action fallback returned `REJECTED` where the reference returns `NO_PLAYER`.

## The recommendation

**The project should end up with one Rust port, and it should be the one inside the
repository.** Two ports of the same relay, by two authors, on a protocol where a byte
difference is a bug, is a liability rather than an asset.

The pieces worth carrying across from this port are small and specific:

1. the randomised fuzzer (`conformance/fuzz.py`) against the existing parity harness,
2. the float rendering cross-check against Lua,
3. the `world1-` build label check, if upstream's port does not already have it,
4. the CI, which nothing else in the project has.

The pieces worth carrying across from upstream's port are larger: the type-level protocol,
the `"C-unwind"` ABI discipline, and the 17-scenario native parity harness.

This port stays where it is because it is a working, verifiable implementation and because
the archive it came from is separately preserved. It is a reference, not a claim on the
project.

## One correction to a claim made earlier

This port's README originally said the game-side half "must stay Lua", on the grounds that
X4 runs its own embedded LuaJIT and Mission Director XML.

That is no longer a safe claim. Upstream's `contrib/ferrite-rust-core` builds a Rust core
that binds X4's own FFI at runtime — 25 exports resolved by `dlsym` on Linux and
`GetProcAddress` on Windows, bound only when the address and the 16-byte code head match
build 23660954 — and the `contrib/ferrite-rw-*` family (ecs-core, physics, transforms, lane,
walk, combat, economy, ai-driver, ai-orders, frame-diet) is rewriting the simulation side in
Rust behind that core.

So the game-side half is *not* off the table. The constraint is that it has to bind a
specific game build's FFI, and it has to be pinned to it.
