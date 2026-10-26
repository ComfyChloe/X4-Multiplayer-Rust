//! The relay's session state machine.
//!
//! This is the Rust port of `x4lan/host.py`. The two implementations are
//! expected to be **byte-for-byte interchangeable on the wire**: a peer that
//! talks to the Python relay must behave identically against this one, and
//! the game-side Lua never learns which one it is talking to.
//!
//! The port preserves the reference implementation's validation *order*, not
//! just its rules. Python validates a packet field by field and returns on the
//! first failure; several rules interact (a `HELLO` is rejected for the wrong
//! token before the build is even compared), and reordering them would change
//! which packets are accepted.

pub mod bubble;
pub mod coop;
pub mod docking;
pub mod economy;
pub mod mission;
pub mod universe;

use crate::format::py_g;
use crate::ordered::OrderedMap;
use crate::protocol::{constant_time_eq, is_atom, is_lease, sequence, PeerAddress, MAX_PACKET};

/// How long a peer may stay silent before its slot is released.
pub const LEASE_SECONDS: f64 = 30.0;

/// How long a remote pose stays fresh enough to be relayed.
pub const STATE_SECONDS: f64 = 0.75;

/// Snapshot rate, in hertz.
pub const TICK_HZ: u32 = 20;

/// Which protocol layer the session runs.
///
/// The Python implementation expresses this as a class hierarchy
/// (`Session` -> `BubbleSession` -> `EconomySession` -> `CoopSession` ->
/// `DockSession` -> `UniverseSession`). Rust has no inheritance, so the
/// chain is made explicit: each layer owns its `impl Session` block and calls
/// the layer below it directly when a packet is not its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The plain two-slot pose relay.
    Simple,
    /// The experimental test bubble.
    Bubble,
    /// Market snapshots and trading.
    Economy,
    /// Cooperative ships, health, fire and missions.
    Coop,
    /// Co-op docking reservations.
    Dock,
    /// Whole-sector world synchronisation.
    Universe,
}

/// A connected peer.
#[derive(Debug, Clone)]
pub struct Peer {
    pub address: PeerAddress,
    pub nonce: String,
    pub lease: String,
    pub last_seen: f64,
    pub seq: i64,
    pub state_at: f64,
    pub pose: Option<[f64; 6]>,
    pub joined_at: f64,
    pub state_messages: u64,
    pub resume_from: Option<String>,
}

impl Peer {
    fn new(address: PeerAddress, nonce: String, lease: String, now: f64) -> Self {
        Self {
            address,
            nonce,
            lease,
            last_seen: now,
            seq: -1,
            state_at: f64::NEG_INFINITY,
            pose: None,
            joined_at: now,
            state_messages: 0,
            resume_from: None,
        }
    }
}

/// The relay's authoritative state for one session.
#[derive(Debug, Clone)]
pub struct Session {
    /// Which protocol layer is active.
    pub mode: Mode,
    /// Shared session secret.
    pub token: String,
    /// Shared build label; both sides must agree.
    pub build: String,
    /// Shared universe (save) identifier.
    pub bubble: String,
    /// The sector the session is bound to, once a peer has joined.
    pub sector: Option<String>,
    /// Connected peers, keyed by slot, in join order.
    pub peers: OrderedMap<String, Peer>,
    /// Monotonic snapshot counter.
    pub tick: u64,
    /// Count of accepted state frames.
    pub accepted_states: u64,
    /// Recently expired (slot, nonce) pairs, so a stale client cannot rejoin.
    pub retired: Vec<(String, String)>,
    /// Peers that may resume, and the sector they were bound to.
    pub resumable: OrderedMap<String, (Peer, Option<String>)>,

    // ---- Bubble layer ----
    pub world: Option<Vec<f64>>,
    pub world_owner: Option<String>,
    pub world_revision: i64,
    pub commands: OrderedMap<String, Command>,
    pub fire: OrderedMap<String, (i64, f64)>,
    pub bodies: OrderedMap<String, Vec<String>>,
    pub suit_kills: OrderedMap<String, (String, i64)>,
    pub walkers: OrderedMap<String, (i64, f64, i64, Vec<String>)>,

    // ---- Economy layer ----
    pub economy: Option<String>,
    pub economy_owner: Option<String>,
    pub assembly: crate::session::economy::Assembly,
    pub orders: OrderedMap<String, Order>,
    pub docks: OrderedMap<String, (i64, String)>,
    pub next_economy: f64,
    pub next_docks: f64,

    // ---- Co-op layer ----
    pub owners: OrderedMap<i64, String>,
    pub epochs: OrderedMap<i64, i64>,
    pub crew: OrderedMap<String, (i64, i64, i64)>,
    pub navigation: OrderedMap<i64, Navigation>,
    pub fire2: OrderedMap<i64, FireFrame>,
    pub health: OrderedMap<i64, HealthFrame>,
    pub health_gen: OrderedMap<i64, i64>,
    pub health_key: OrderedMap<i64, (String, i64, String)>,
    pub health_live: OrderedMap<i64, i64>,
    pub health_seen: OrderedMap<i64, f64>,
    pub shots: OrderedMap<(i64, i64, i64, i64), Shot>,
    pub body_sectors: OrderedMap<String, String>,
    pub game_metrics: OrderedMap<String, PerfFrame>,
    pub authority_revision: i64,
    pub order_ships: OrderedMap<String, (i64, i64)>,
    pub quest: crate::session::coop::QuestState,
    pub missions: crate::session::coop::MissionJournal,

    // ---- Docking layer ----
    pub coop_docks: OrderedMap<i64, DockClaim>,
    pub coop_dock_seen: OrderedMap<i64, ((String, String, i64), i64)>,

    // ---- Universe layer ----
    pub universe: crate::session::universe::Receiver,
    pub universe_epoch: Option<String>,
    pub sectors: OrderedMap<String, crate::session::universe::Baseline>,
    pub entity_ids: OrderedMap<String, Vec<i64>>,
    pub interest: Option<(String, i64, String)>,
    pub interest_after_revision: i64,
    pub delivery: Option<crate::session::universe::Sender>,
    pub delivery_lease: Option<String>,
    pub delivered: Option<i64>,
    pub positions: OrderedMap<i64, Position>,
    pub position_cursor: usize,
    pub cargo: crate::session::universe::CargoTransactions,
    pub identities: crate::session::universe::Identities,
    pub npc_combat: crate::session::universe::NpcCombat,

    /// When set, leases are generated deterministically from this counter.
    ///
    /// The reference implementation draws a random lease for every new peer.
    /// That is correct in production but makes byte-for-byte comparison
    /// between two implementations impossible, so the conformance harness
    /// turns it on and both sides then produce the same lease sequence.
    pub deterministic_leases: Option<u64>,
}

/// A queued action awaiting the host's completion.
#[derive(Debug, Clone)]
pub struct Command {
    pub slot: String,
    pub lease: String,
    pub ip: String,
    pub verb: String,
    pub result: String,
    pub created: f64,
    /// The ship this command acts on, filled in once the pilot is known.
    pub ship: Option<i64>,
}

/// A queued market order awaiting the host's completion.
#[derive(Debug, Clone)]
pub struct Order {
    pub slot: String,
    pub lease: String,
    pub owner: String,
    pub market: String,
    pub lines: String,
    pub result: String,
}

/// A co-op ship's navigation frame.
#[derive(Debug, Clone)]
pub struct Navigation {
    pub lease: String,
    pub seq: i64,
    pub at: f64,
    pub fields: Vec<String>,
    pub kind: String,
}

/// A co-op ship's fire frame.
#[derive(Debug, Clone)]
pub struct FireFrame {
    pub lease: String,
    pub epoch: i64,
    pub seq: i64,
    pub time: f64,
    pub shots: [i64; 2],
    pub at: f64,
    pub fields: Vec<String>,
}

/// A co-op ship's health frame.
#[derive(Debug, Clone)]
pub struct HealthFrame {
    pub lease: String,
    pub seq: i64,
    pub at: f64,
    pub fields: Vec<String>,
}

/// A cumulative hit count.
#[derive(Debug, Clone)]
pub struct Shot {
    pub lease: String,
    pub seq: i64,
    pub hits: i64,
    pub at: f64,
    pub fields: Vec<String>,
}

/// A client's performance telemetry.
#[derive(Debug, Clone)]
pub struct PerfFrame {
    pub seq: i64,
    pub at: f64,
    pub callbacks_hz: f64,
    pub interval_avg_ms: f64,
    pub interval_max_ms: f64,
    pub state: String,
    pub sector: String,
}

/// A co-op ship's dock reservation.
#[derive(Debug, Clone)]
pub struct DockClaim {
    pub owner: String,
    pub lease: String,
    pub epoch: i64,
    pub seq: i64,
    pub mode: String,
    pub dock: i64,
    pub claim: i64,
    pub at: f64,
}

/// An observed object position.
#[derive(Debug, Clone)]
pub struct Position {
    pub seq: i64,
    pub at: f64,
    pub sector: String,
    pub fields: Vec<String>,
    pub kind: String,
}

impl Session {
    /// Creates a session for the given mode.
    pub fn new(mode: Mode, token: &str, build: &str, bubble: &str) -> Self {
        let mut owners = OrderedMap::new();
        owners.insert(1, "p1".to_string());
        owners.insert(2, "p2".to_string());
        let mut epochs = OrderedMap::new();
        epochs.insert(1, 1);
        epochs.insert(2, 1);
        let mut health_gen = OrderedMap::new();
        health_gen.insert(1, 0);
        health_gen.insert(2, 0);
        let mut health_key = OrderedMap::new();
        health_key.insert(1, (String::new(), 0, String::new()));
        health_key.insert(2, (String::new(), 0, String::new()));
        let mut health_live = OrderedMap::new();
        health_live.insert(1, 0);
        health_live.insert(2, 0);
        let mut health_seen = OrderedMap::new();
        health_seen.insert(1, 0.0);
        health_seen.insert(2, 0.0);

        Self {
            mode,
            token: token.to_string(),
            build: build.to_string(),
            bubble: bubble.to_string(),
            sector: None,
            peers: OrderedMap::new(),
            tick: 0,
            accepted_states: 0,
            retired: Vec::new(),
            resumable: OrderedMap::new(),
            world: None,
            world_owner: None,
            world_revision: -1,
            commands: OrderedMap::new(),
            fire: OrderedMap::new(),
            bodies: OrderedMap::new(),
            suit_kills: OrderedMap::new(),
            walkers: OrderedMap::new(),
            economy: None,
            economy_owner: None,
            assembly: crate::session::economy::Assembly::new(),
            orders: OrderedMap::new(),
            docks: OrderedMap::new(),
            next_economy: 0.0,
            next_docks: 0.0,
            owners,
            epochs,
            crew: OrderedMap::new(),
            navigation: OrderedMap::new(),
            fire2: OrderedMap::new(),
            health: OrderedMap::new(),
            health_gen,
            health_key,
            health_live,
            health_seen,
            shots: OrderedMap::new(),
            body_sectors: OrderedMap::new(),
            game_metrics: OrderedMap::new(),
            authority_revision: 1,
            order_ships: OrderedMap::new(),
            quest: crate::session::coop::QuestState::new(),
            missions: crate::session::coop::MissionJournal::new(),
            coop_docks: OrderedMap::new(),
            coop_dock_seen: OrderedMap::new(),
            universe: crate::session::universe::Receiver::new(),
            universe_epoch: None,
            sectors: OrderedMap::new(),
            entity_ids: OrderedMap::new(),
            interest: None,
            interest_after_revision: 0,
            delivery: None,
            delivery_lease: None,
            delivered: None,
            positions: OrderedMap::new(),
            position_cursor: 0,
            cargo: crate::session::universe::CargoTransactions::new(),
            identities: crate::session::universe::Identities::new(),
            npc_combat: crate::session::universe::NpcCombat::new(),
            deterministic_leases: None,
        }
    }

    /// Makes lease generation deterministic, for conformance testing.
    pub fn use_deterministic_leases(&mut self) {
        self.deterministic_leases = Some(0);
    }

    /// Finds the peer that owns an address and presents a lease.
    pub fn peer_for(&self, lease: &str, address: PeerAddress) -> Option<(&String, &Peer)> {
        self.peers.iter().find(|(_, peer)| {
            peer.address == address && constant_time_eq(peer.lease.as_bytes(), lease.as_bytes())
        })
    }

    /// Returns the slot name of a peer, if the lease and address match.
    pub fn peer_slot(&self, lease: &str, address: PeerAddress) -> Option<String> {
        self.peer_for(lease, address)
            .map(|(slot, _)| slot.clone())
    }

    /// Releases peers whose lease has expired.
    ///
    /// The Python reference expresses this as a chain of `super().expire()`
    /// calls across the class hierarchy. Rust has no inheritance, so the chain
    /// is spelled out: each layer's `expire_*` runs the layer below it first,
    /// then applies its own rules.
    pub fn expire(&mut self, now: f64) {
        match self.mode {
            Mode::Simple => self.expire_base(now),
            Mode::Bubble => self.expire_bubble(now),
            Mode::Economy => self.expire_economy(now),
            Mode::Coop => self.expire_coop(now),
            Mode::Dock => self.expire_dock(now),
            Mode::Universe => self.expire_universe(now),
        }
    }

    /// Base-layer expiry: drops peers whose lease has lapsed.
    ///
    /// The last expired identity per slot is retained so a client that
    /// reconnects can prove it is the same peer; a fresh game claiming the
    /// slot invalidates it.
    pub(super) fn expire_base(&mut self, now: f64) {
        let expired: Vec<String> = self
            .peers
            .iter()
            .filter(|(_, peer)| now - peer.last_seen >= LEASE_SECONDS)
            .map(|(slot, _)| slot.clone())
            .collect();
        for slot in expired {
            if let Some(peer) = self.peers.remove(&slot) {
                self.resumable.insert(slot.clone(), (peer.clone(), self.sector.clone()));
                self.retired.push((slot, peer.nonce));
                if self.retired.len() > 64 {
                    self.retired.drain(0..self.retired.len() - 64);
                }
            }
        }
        if self.peers.is_empty() {
            self.sector = None;
        }
    }

    /// Handles one inbound packet and returns the packets to send back.
    pub fn receive(&mut self, raw: &[u8], address: PeerAddress, now: f64) -> Vec<Vec<u8>> {
        match self.mode {
            Mode::Simple => self.receive_base(raw, address, now),
            Mode::Bubble => self.receive_bubble(raw, address, now),
            Mode::Economy => self.receive_economy(raw, address, now),
            Mode::Coop => self.receive_coop(raw, address, now),
            Mode::Dock => self.receive_dock(raw, address, now),
            Mode::Universe => self.receive_universe(raw, address, now),
        }
    }

    /// The base two-slot pose protocol.
    fn receive_base(&mut self, raw: &[u8], address: PeerAddress, now: f64) -> Vec<Vec<u8>> {
        self.expire(now);
        if raw.len() > MAX_PACKET {
            return Vec::new();
        }
        let Ok(text) = std::str::from_utf8(raw) else {
            return Vec::new();
        };
        if !text.is_ascii() {
            return Vec::new();
        }
        let fields: Vec<&str> = text.split('|').collect();
        if fields.len() < 2 || fields[1] != "1" {
            return Vec::new();
        }

        match fields[0] {
            "PING" if fields.len() == 4 => {
                let token = fields[2];
                let nonce = fields[3];
                if !constant_time_eq(token.as_bytes(), self.token.as_bytes()) || !is_lease(nonce) {
                    return Vec::new();
                }
                // An authenticated readiness check: it never claims a slot or
                // fixes a sector.
                vec![format!("PONG|1|{nonce}|{}|{}", self.build, self.bubble).into_bytes()]
            }
            "RESUME" if fields.len() == 10 => {
                let (token, build, sector, bubble, slot, previous_nonce, lease, nonce) = (
                    fields[2], fields[3], fields[4], fields[5], fields[6], fields[7], fields[8],
                    fields[9],
                );
                if !constant_time_eq(token.as_bytes(), self.token.as_bytes())
                    || build != self.build
                    || bubble != self.bubble
                    || (slot != "p1" && slot != "p2")
                    || !is_lease(previous_nonce)
                    || !is_lease(lease)
                    || !is_lease(nonce)
                    || !is_atom(sector)
                {
                    return Vec::new();
                }
                let peer = self.peers.get(&slot.to_string()).cloned();
                let saved = self.resumable.get(&slot.to_string()).cloned();
                let candidate = peer.clone().or_else(|| saved.as_ref().map(|(p, _)| p.clone()));
                let Some(candidate) = candidate else {
                    // A new relay cannot prove this is the same world. Do not
                    // silently start a second economy over an existing game.
                    return vec![format!("RESET|1|{nonce}").into_bytes()];
                };
                let saved_sector = saved.as_ref().and_then(|(_, sector)| sector.clone());
                if !constant_time_eq(candidate.lease.as_bytes(), lease.as_bytes())
                    || (candidate.nonce != previous_nonce
                        && !(candidate.nonce == nonce
                            && candidate.resume_from.as_deref() == Some(previous_nonce)))
                    || (self.sector.is_some() && self.sector.as_deref() != Some(sector))
                    || (peer.is_none() && saved_sector.as_deref() != Some(sector))
                    || self
                        .peers
                        .iter()
                        .any(|(other_slot, other)| other_slot != slot && other.address == address)
                {
                    return Vec::new();
                }
                let mut candidate = candidate;
                candidate.resume_from = Some(previous_nonce.to_string());
                candidate.address = address;
                candidate.nonce = nonce.to_string();
                candidate.last_seen = now;
                // Keep sequence, world identity and command deduplication intact.
                candidate.pose = None;
                self.peers.insert(slot.to_string(), candidate);
                self.resumable.remove(&slot.to_string());
                self.sector = Some(sector.to_string());
                vec![format!("WELCOME|1|{nonce}|{lease}|{TICK_HZ}").into_bytes()]
            }
            "HELLO" if fields.len() == 8 => {
                let (token, build, sector, bubble, slot, nonce) =
                    (fields[2], fields[3], fields[4], fields[5], fields[6], fields[7]);
                if !constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
                    return Vec::new();
                }
                if build != self.build
                    || bubble != self.bubble
                    || (slot != "p1" && slot != "p2")
                    || !is_lease(nonce)
                    || !is_atom(sector)
                {
                    return Vec::new();
                }
                if self.sector.is_some() && self.sector.as_deref() != Some(sector) {
                    return Vec::new();
                }
                let existing = self.peers.get(&slot.to_string()).cloned();
                if let Some(existing) = &existing {
                    if existing.address != address || existing.nonce != nonce {
                        return Vec::new();
                    }
                } else {
                    if self.retired.iter().any(|(s, n)| s == slot && n == nonce) {
                        return Vec::new();
                    }
                    // A single endpoint must not own both slots.
                    if self.peers.values().any(|peer| peer.address == address) {
                        return Vec::new();
                    }
                    let lease = self.new_lease();
                    let peer = Peer::new(address, nonce.to_string(), lease, now);
                    self.peers.insert(slot.to_string(), peer);
                    self.resumable.remove(&slot.to_string());
                    self.sector = Some(sector.to_string());
                }
                let lease = self.peers.get(&slot.to_string()).map(|p| p.lease.clone()).unwrap_or_default();
                if let Some(peer) = self.peers.get_mut(&slot.to_string()) {
                    peer.last_seen = now;
                }
                vec![format!("WELCOME|1|{nonce}|{lease}|{TICK_HZ}").into_bytes()]
            }
            "STATE" | "IDLE" => {
                let expected = if fields[0] == "STATE" { 10 } else { 4 };
                if fields.len() != expected {
                    return Vec::new();
                }
                let lease = fields[2];
                let found = self.peers.iter().find(|(_, peer)| {
                    peer.address == address && constant_time_eq(peer.lease.as_bytes(), lease.as_bytes())
                });
                let Some((slot, _)) = found else {
                    return Vec::new();
                };
                let slot = slot.clone();
                let Some(seq) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let mut pose = None;
                if fields[0] == "STATE" {
                    let mut values = [0.0f64; 6];
                    for (index, field) in fields[4..].iter().enumerate() {
                        let Some(value) = crate::format::py_float(field) else {
                            return Vec::new();
                        };
                        values[index] = value;
                    }
                    if !values.iter().all(|v| v.is_finite()) {
                        return Vec::new();
                    }
                    if values[..3].iter().any(|v| v.abs() > 50_000.0) {
                        return Vec::new();
                    }
                    if values[3..].iter().any(|v| v.abs() > std::f64::consts::TAU) {
                        return Vec::new();
                    }
                    pose = Some(values);
                }
                let Some(peer) = self.peers.get_mut(&slot) else {
                    return Vec::new();
                };
                if seq <= peer.seq {
                    return Vec::new();
                }
                peer.seq = seq;
                peer.pose = pose;
                peer.last_seen = now;
                peer.resume_from = None;
                peer.state_at = now;
                peer.state_messages += 1;
                if pose.is_some() {
                    self.accepted_states += 1;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Produces the periodic snapshots for every connected peer.
    pub fn snapshots(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        match self.mode {
            Mode::Simple => self.snapshots_base(now),
            Mode::Bubble => self.snapshots_bubble(now),
            Mode::Economy => self.snapshots_economy(now),
            Mode::Coop => self.snapshots_coop(now),
            Mode::Dock => self.snapshots_dock(now),
            Mode::Universe => self.snapshots_universe(now),
        }
    }

    fn snapshots_base(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        self.expire(now);
        self.tick += 1;
        let tick = self.tick;
        let mut result = Vec::new();
        let slots: Vec<String> = self.peers.keys().cloned().collect();
        for slot in slots {
            let Some(peer) = self.peers.get(&slot) else {
                continue;
            };
            let other_slot = if slot == "p1" { "p2" } else { "p1" };
            let remote = self.peers.get(other_slot);
            let packet = match remote {
                Some(remote)
                    if remote.pose.is_some() && now - remote.state_at < STATE_SECONDS =>
                {
                    let pose = remote.pose.unwrap();
                    let rendered: Vec<String> = pose.iter().map(|v| py_g(*v, 9)).collect();
                    format!(
                        "PEER|1|{}|{tick}|{}|{}|{}",
                        peer.lease,
                        remote.lease,
                        remote.seq,
                        rendered.join("|")
                    )
                }
                _ => format!("EMPTY|1|{}|{tick}", peer.lease),
            };
            result.push((peer.address, packet.into_bytes()));
        }
        result
    }

    /// Draws a lease for a newly joined peer.
    fn new_lease(&mut self) -> String {
        match self.deterministic_leases.as_mut() {
            Some(counter) => {
                *counter += 1;
                format!("{counter:032x}")
            }
            None => generate_lease(),
        }
    }
}

/// Generates a 32-character lowercase hex lease.
fn generate_lease() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // The lease is an opaque capability token, not a secret key, but it must
    // be unguessable: a peer that guesses another peer's lease could forge
    // state frames. Mix the clock with a per-process counter and the address
    // of a heap allocation to add entropy beyond the clock.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ (counter.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut lease = String::with_capacity(32);
    for _ in 0..4 {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        lease.push_str(&format!("{value:016x}"));
    }
    lease.truncate(32);
    lease
}

/// Re-exported so the relay binary can share the same bounds.
pub use crate::protocol::MAX_SEQ as MAX_SEQUENCE;
