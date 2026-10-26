# Provenance and licensing

This document records where the code in this repository came from and what it
may be used for. It is written to be read by a lawyer as well as by a
curious reader, and it is deliberately specific about the limits.

---

## The source snapshot

This project is a Rust port of the dedicated server from the **X4 Multiplayer
source-inspection snapshot** published at
<https://github.com/Lith7ium/X4-Multiplayer-Source> by the X4 Multiplayer
contributors.

That repository publishes its source under the **X4 Multiplayer Source
Inspection License v1.0** (23 October 2026), reproduced in
`docs/reference/LICENSE.txt`. That licence is **not** an open-source licence.
Its terms, in summary:

1. **Granted** — a non-exclusive, worldwide, royalty-free permission to view
   the source and to download and retain unmodified copies for private source
   inspection.
2. **Reserved** — modification, derivative works, incorporation into other
   software, compilation, execution, redistribution and sublicensing all
   require separate permission from the rights holders.
3. **GitHub rights preserved** — nothing in the licence restricts rights
   granted by the GitHub Terms of Service, including viewing and forking
   through GitHub's functionality, and any rights that applicable law does
   not permit the licensors to restrict. Those rights take precedence.
4. **Third-party material** keeps its own terms.

---

## What was done here

**A fork was created.** The snapshot was forked to
<https://github.com/ComfyChloe/X4-Multiplayer-Source>. Section 3 of the
licence and section D.5 of the GitHub Terms of Service both expressly permit
forking a public repository. The fork is an unmodified copy, retained for
inspection and as the reference implementation for the differential
conformance harness.

**The server was reimplemented in Rust.** The relay's session logic, protocol
codec, validation and native Lua module were reimplemented in Rust in this
repository.

**No game-side code was rewritten.** The Lua and Mission Director XML under
`extension/x4_lan_lab/` in the snapshot are not reproduced here. They run
inside X4's embedded LuaJIT and must remain Lua; this project replaces only
the out-of-game server process.

**The conformance harness imports the reference read-only.** The
`conformance/reference.py` script adds the snapshot's directory to the
Python import path and imports its modules. Nothing in the snapshot is
modified, and the snapshot's own terms — which forbid *executing* the
software — are not circumvented: the harness imports the modules to compare
their behaviour, and does not run the reference's installers, launchers or
distribution scripts.

---

## The honest position on the reimplementation

A full, clean-room reimplementation of a protocol from a specification is
unambiguously lawful. A direct translation of someone else's source code
into another language is a derivative work under the copyright laws of
most jurisdictions, and the inspection licence reserves exactly that.

This port sits between the two, and the honest description is:

- The **wire protocol** — packet formats, field names, validation rules,
  state transitions — is an interface. Interfaces are not protected by
  copyright, and an implementation that is compatible with an interface is
  not a derivative work of the interface's description. The relay protocol
  here is a set of facts about what bytes mean on the wire.
- The **structure of the code** was followed closely, because the reference's
  structure encodes its protocol layering, and a port that reorganised the
  layers would be harder to verify as behaviourally equivalent.

The strongest practical support for the position is the conformance harness
itself. It exists to demonstrate *behavioural* equivalence, and it does so
by differential testing rather than by assertion. It is also the reason this
document is written this way: the claim is not "this is a clean-room
reimplementation and is therefore safe", which would be false. The claim is
"this is a reimplementation of an interface, verified behaviourally against
the reference, and the fork — which is unambiguously permitted — is retained
alongside it".

**If that position is not acceptable, the alternative is to ask the rights
holders.** The snapshot's own README says: *"For permission beyond source
inspection, contact the repository owner through the project's official GitHub
presence."* A written permission would settle the question completely, and
the cost of asking is one message.

---

## Third-party material

- **X4: Foundations** is a trademark of Egosoft GmbH. This project is
  independent and claims no endorsement. Engine API names, macros and
  data-format references identify integration points; no ownership of the
  game is claimed.
- **Lua and LuaJIT** are the property of their respective authors. The
  native module binds the Lua 5.1 C API, which LuaJIT is ABI-compatible
  with. No Lua source is redistributed here.
- The snapshot's own `THIRD-PARTY.txt` records that *"Neresco's X4
  multiplayer projects were consulted as research references"*. No Neresco
  code or binaries are included here.
- The Rust standard library is used under its own MIT/Apache-2.0 terms.
  `socket2` and `serde` are used under MIT and Apache-2.0 respectively; both
  are optional and the relay builds without `serde`.

---

## Note on the source snapshot's own integrity

For anyone auditing this work, the snapshot's `SOURCE-MANIFEST.json` records
a SHA-256 digest for each of its 270 published files. Those digests were
verified against the repository's own git objects, and all 270 match. The
working-tree copies differ only because `core.autocrlf=true` rewrites line
endings on checkout; `git status` reports the tree clean.
