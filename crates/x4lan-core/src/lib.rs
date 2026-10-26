//! Protocol, validation and session logic for the X4: Foundations LAN
//! multiplayer relay.
//!
//! This crate is a Rust port of the Python reference relay (`x4lan/`). The
//! two are intended to be interchangeable on the wire: the game-side Lua
//! extension cannot tell which one it is talking to.
//!
//! # Layout
//!
//! * [`format`] — Python-compatible number formatting and parsing. The wire
//!   protocol is byte-exact, so `0.1 + 0.2` must render as `0.3` and not as
//!   `0.30000000000000004`.
//! * [`protocol`] — shared validation: identifiers, leases, sequence numbers
//!   and constant-time secret comparison.
//! * [`ordered`] — an insertion-ordered map. Python dictionaries preserve
//!   insertion order and the protocol depends on it.
//! * [`session`] — the session state machine, one module per protocol layer.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod format;
pub mod ordered;
pub mod protocol;
pub mod session;

pub use session::{Mode, Peer, Session, LEASE_SECONDS, STATE_SECONDS, TICK_HZ};
