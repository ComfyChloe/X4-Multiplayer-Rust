//! Stable cooperative ships, independent of player slots and sector boundaries.
//!
//! Port of `x4lan/coop.py`. The relay arbitrates pilot ownership, not game
//! physics. The host's X4 remains the sole authority for damage, cargo,
//! treasury and mission outcomes.

use super::{FireFrame, HealthFrame, Navigation, PerfFrame, Session, Shot};
use crate::format::py_float;
use crate::ordered::OrderedMap;
use crate::protocol::{is_atom, is_lease, sequence, PeerAddress, MAX_PACKET};

/// Largest position coordinate.
const POSITION_LIMIT: f64 = 50_000_000.0;
/// Seconds without a pilot frame before a live health generation ends.
const HEALTH_STALE: f64 = 0.75;
/// Largest velocity component.
const VELOCITY_LIMIT: f64 = 100_000.0;

/// A weapon macro with a per-shot strength, e.g. `weapon_gen_s_laser_01_mk1_macro:12.500`.
pub fn is_shot_digest(value: &str) -> bool {
    let Some((macro_name, strength)) = value.split_once(':') else {
        return false;
    };
    if macro_name.is_empty() || macro_name.len() > 48 {
        return false;
    }
    if !macro_name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    let (whole, fraction) = match strength.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => return false,
    };
    if whole.is_empty() || whole.len() > 9 || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if fraction.len() != 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    true
}

/// Parses and bounds-checks a six-value pose.
pub fn parse_pose(values: &[&str]) -> Option<[f64; 6]> {
    if values.len() != 6 {
        return None;
    }
    let mut pose = [0.0f64; 6];
    for (index, field) in values.iter().enumerate() {
        let value = py_float(field)?;
        if !value.is_finite() {
            return None;
        }
        let limit = if index < 3 { POSITION_LIMIT } else { std::f64::consts::TAU };
        if value.abs() > limit {
            return None;
        }
        pose[index] = value;
    }
    Some(pose)
}

/// Host-native faction mission journal state.
#[derive(Debug, Clone, Default)]
pub struct QuestState {
    pub owner: Option<String>,
    pub progress: Option<(i64, i64, i64, i64, i64)>,
    pub next_send: f64,
}

impl QuestState {
    pub fn new() -> Self {
        Self {
            owner: None,
            progress: None,
            next_send: 0.0,
        }
    }

    /// Parses a quest progress snapshot.
    pub fn parse_progress(fields: &[&str]) -> Option<(i64, i64, i64, i64, i64)> {
        if fields.len() != 5 {
            return None;
        }
        let revision = sequence(fields[0])?;
        let stage = super::economy::integer(fields[1], 3, false)?;
        let delivered = super::economy::integer(fields[2], 20, false)?;
        let destroyed = super::economy::integer(fields[3], 2, false)?;
        let paid = super::economy::integer(fields[4], 1, false)?;
        if stage < 1 {
            return None;
        }
        if stage == 1 && (delivered == 20 || paid != 0) {
            return None;
        }
        if stage == 2 && (delivered != 20 || destroyed == 2 || paid != 0) {
            return None;
        }
        if stage == 3 && (delivered, destroyed, paid) != (20, 2, 1) {
            return None;
        }
        Some((revision, stage, delivered, destroyed, paid))
    }

    /// Accepts a monotonic progress snapshot.
    pub fn receive(&mut self, lease: &str, fields: &[&str]) {
        let Some(progress) = Self::parse_progress(fields) else {
            return;
        };
        let old = if self.owner.as_deref() == Some(lease) {
            self.progress
        } else {
            None
        };
        if let Some(old) = old {
            if progress.0 <= old.0 {
                return;
            }
            let new_tail: [i64; 4] = [progress.1, progress.2, progress.3, progress.4];
            let old_tail: [i64; 4] = [old.1, old.2, old.3, old.4];
            for (new, previous) in new_tail.iter().zip(old_tail.iter()) {
                if new < previous {
                    return;
                }
            }
        }
        self.owner = Some(lease.to_string());
        self.progress = Some(progress);
    }

    /// Produces quest snapshots for every peer.
    pub fn snapshots(&mut self, peers: &OrderedMap<String, super::Peer>, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let Some(host) = peers.get("p1") else {
            return Vec::new();
        };
        if self.owner.as_deref() != Some(host.lease.as_str()) {
            return Vec::new();
        }
        let Some(progress) = self.progress else {
            return Vec::new();
        };
        if now < self.next_send {
            return Vec::new();
        }
        self.next_send = now + 0.5;
        let payload = format!(
            "{}|{}|{}|{}|{}",
            progress.0, progress.1, progress.2, progress.3, progress.4
        );
        peers
            .values()
            .map(|peer| {
                (
                    peer.address,
                    format!("QUEST|1|{}|{}|{payload}", peer.lease, host.lease).into_bytes(),
                )
            })
            .collect()
    }
}

/// A host-native faction mission record.
#[derive(Debug, Clone)]
pub struct MissionRecord {
    pub kind: String,
    pub state: String,
    pub done: i64,
    pub total: i64,
    pub reward: i64,
    pub faction: String,
    pub title: String,
    pub description: String,
    pub objective: String,
}

/// A mission record in the journal.
#[derive(Debug, Clone)]
pub struct MissionEntry {
    pub revision: i64,
    pub row: MissionRecord,
    pub payload: String,
    pub digest: u32,
}

/// A mission transfer in progress.
#[derive(Debug, Clone)]
pub struct MissionPending {
    pub revision: i64,
    pub total: i64,
    pub digest: u32,
    pub parts: OrderedMap<i64, String>,
    pub at: f64,
}

/// The host-native faction mission journal.
#[derive(Debug, Clone)]
pub struct MissionJournal {
    pub epoch: Option<String>,
    pub records: OrderedMap<i64, MissionEntry>,
    pub pending: OrderedMap<i64, MissionPending>,
    pub requests: OrderedMap<i64, (String, i64, String)>,
    pub cursor: usize,
    pub next_send: f64,
    pub changed: Vec<i64>,
}

impl MissionJournal {
    pub fn new() -> Self {
        Self {
            epoch: None,
            records: OrderedMap::new(),
            pending: OrderedMap::new(),
            requests: OrderedMap::new(),
            cursor: 0,
            next_send: 0.0,
            changed: Vec::new(),
        }
    }

    /// Forgets everything when the host's lease changes.
    ///
    /// Takes the lease rather than a peer reference so the caller does not
    /// have to hold a borrow of the session while this mutates it.
    pub fn current_host(&mut self, host_lease: Option<&str>) -> Option<String> {
        if let Some(lease) = host_lease {
            if self.epoch.as_deref() != Some(lease) {
                self.epoch = Some(lease.to_string());
                self.records = OrderedMap::new();
                self.pending = OrderedMap::new();
                self.requests = OrderedMap::new();
                self.cursor = 0;
                self.changed.clear();
            }
        }
        host_lease.map(|lease| lease.to_string())
    }
}

impl Session {
    /// Handles a packet in the co-op layer.
    pub(super) fn receive_coop(
        &mut self,
        raw: &[u8],
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        if raw.starts_with(b"MSTATE|") || raw.starts_with(b"MREQ|") {
            return self.receive_mission(raw, address, now);
        }
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
        if fields.len() < 3 || fields[1] != "1" {
            return Vec::new();
        }
        let kind = fields[0];

        const COOP_KINDS: [&str; 14] = [
            "CREW", "NAV", "NAV2", "FIRE2", "HEALTH", "HACK", "HSTALE", "SHOT", "BODY2", "PERF",
            "COOPORDER", "QUEST", "BOLT", "BEND",
        ];
        if !COOP_KINDS.contains(&kind) {
            // Legacy movement, fire and dock frames must never override
            // cooperative authority.
            if ["STATE", "FIRE", "DOCKSTATE", "BODY", "ORDER"].contains(&kind) {
                return Vec::new();
            }
            let old_leases: Vec<(String, String)> = self
                .peers
                .iter()
                .map(|(slot, peer)| (slot.clone(), peer.lease.clone()))
                .collect();
            let result = self.receive_economy(raw, address, now);
            if kind == "HELLO" {
                for (slot, peer) in self.peers.iter() {
                    let changed = old_leases
                        .iter()
                        .find(|(old_slot, _)| old_slot == slot)
                        .map(|(_, old_lease)| old_lease != &peer.lease)
                        .unwrap_or(true);
                    if changed {
                        for entity in [1, 2] {
                            let owner = self.owners.get(&entity).cloned().unwrap_or_default();
                            if owner == *slot {
                                if let Some(nav) = self.navigation.get(&entity) {
                                    if nav.lease != peer.lease {
                                        if let Some(epoch) = self.epochs.get_mut(&entity) {
                                            *epoch += 1;
                                        }
                                        self.navigation.remove(&entity);
                                        self.authority_revision += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if kind == "ACTION" && fields.len() == 7 {
                let request = fields[4];
                let mut entity = 0;
                if let Some(command) = self.commands.get(request) {
                    if command.ship.is_none() {
                        if let Some(peer) = self.peers.get(&command.slot) {
                            entity = self
                                .crew
                                .get(&peer.lease)
                                .map(|crew| crew.1)
                                .unwrap_or(0);
                        }
                        if let Some(command) = self.commands.get_mut(request) {
                            command.ship = Some(entity);
                        }
                    }
                }
                let owner = self.owners.get(&entity).cloned().unwrap_or_default();
                let slot = self
                    .commands
                    .get(request)
                    .map(|command| command.slot.clone())
                    .unwrap_or_default();
                if entity != 1 && entity != 2 || owner != slot || self.dead(entity) {
                    if let Some(command) = self.commands.get_mut(request) {
                        command.result = "REJECTED".to_string();
                    }
                    return vec![format!("RESULT|1|{request}|REJECTED").into_bytes()];
                }
            }
            return result;
        }

        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let peer_lease = self.peers.get(&slot).map(|p| p.lease.clone()).unwrap_or_default();
        self.receive_coop_state(kind, &fields, &slot, &peer_lease, address, now)
    }

    #[allow(clippy::too_many_arguments)]
    fn receive_coop_state(
        &mut self,
        kind: &str,
        fields: &[&str],
        slot: &str,
        peer_lease: &str,
        _address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        match kind {
            "QUEST" if fields.len() == 8 && slot == "p1" => {
                self.quest.receive(peer_lease, &fields[3..]);
            }
            "CREW" if fields.len() == 6 => {
                let (Some(seq), Some(controlled), Some(aboard)) = (
                    sequence(fields[3]),
                    super::economy::integer(fields[4], 2, false),
                    super::economy::integer(fields[5], 2, false),
                ) else {
                    return Vec::new();
                };
                let old = self.crew.get(peer_lease).copied().unwrap_or((-1, 0, 0));
                if seq <= old.0 || (controlled != 0 && controlled != aboard) {
                    return Vec::new();
                }
                let mut granted = controlled;
                if controlled != 0 {
                    let occupied = self.pilot(controlled);
                    let owner = self.owners.get(&controlled).cloned().unwrap_or_default();
                    if self.dead(controlled)
                        || (occupied != "0" && occupied != slot)
                        || (owner != slot && self.busy(controlled))
                    {
                        granted = 0;
                    } else if owner != slot {
                        if let Some(entry) = self.owners.get_mut(&controlled) {
                            *entry = slot.to_string();
                        }
                        if let Some(epoch) = self.epochs.get_mut(&controlled) {
                            *epoch += 1;
                        }
                        self.navigation.remove(&controlled);
                        self.authority_revision += 1;
                    }
                }
                self.crew
                    .insert(peer_lease.to_string(), (seq, granted, aboard));
                if old.1 != granted || old.2 != aboard {
                    self.authority_revision += 1;
                }
                if controlled != 0 && granted == 0 {
                    return vec![format!(
                        "PILOT_DENIED|1|{peer_lease}|{seq}|{controlled}"
                    )
                    .into_bytes()];
                }
            }
            "NAV" if fields.len() == 15 => {
                self.receive_navigation("NAV", fields, slot, peer_lease, now);
            }
            "NAV2" if fields.len() == 22 => {
                self.receive_navigation("NAV2", fields, slot, peer_lease, now);
            }
            "FIRE2" if fields.len() == 11 => {
                let (Some(entity), Some(epoch), Some(seq)) = (
                    super::economy::integer(fields[3], 2, false),
                    sequence(fields[4]),
                    sequence(fields[5]),
                ) else {
                    return Vec::new();
                };
                if entity != 1 && entity != 2 {
                    return Vec::new();
                }
                if self.owners.get(&entity).map(String::as_str) != Some(slot) {
                    return Vec::new();
                }
                if self.epochs.get(&entity).copied() != Some(epoch) {
                    return Vec::new();
                }
                let Some(source_time) = py_float(fields[6]) else {
                    return Vec::new();
                };
                if !source_time.is_finite() || !(0.0..=1e12).contains(&source_time) {
                    return Vec::new();
                }
                let mut groups = [0i64; 2];
                for (index, field) in fields[7..9].iter().enumerate() {
                    let Some(value) = super::economy::integer(field, 4, false) else {
                        return Vec::new();
                    };
                    groups[index] = value;
                }
                if groups[0] == 0 || groups[1] == 0 {
                    return Vec::new();
                }
                let mut shots = [0i64; 2];
                for (index, field) in fields[9..11].iter().enumerate() {
                    let Some(value) = sequence(field) else {
                        return Vec::new();
                    };
                    shots[index] = value;
                }
                if let Some(old) = self.fire2.get(&entity) {
                    if old.lease == peer_lease && old.epoch == epoch {
                        if seq <= old.seq || source_time <= old.time {
                            return Vec::new();
                        }
                        if shots[0] < old.shots[0] || shots[1] < old.shots[1] {
                            return Vec::new();
                        }
                    }
                }
                if self.dead(entity) {
                    return Vec::new();
                }
                self.fire2.insert(
                    entity,
                    FireFrame {
                        lease: peer_lease.to_string(),
                        epoch,
                        seq,
                        time: source_time,
                        shots,
                        at: now,
                        fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    },
                );
            }
            "HEALTH" if fields.len() == 8 => {
                let (Some(entity), Some(generation), Some(seq)) = (
                    super::economy::integer(fields[3], 2, false),
                    sequence(fields[4]),
                    sequence(fields[5]),
                ) else {
                    return Vec::new();
                };
                if entity != 1 && entity != 2 {
                    return Vec::new();
                }
                if self.owners.get(&entity).map(String::as_str) != Some(slot) {
                    return Vec::new();
                }
                if self.pilot(entity) != slot {
                    return Vec::new();
                }
                if generation != self.health_generation(entity) {
                    return Vec::new();
                }
                if self.health_live.get(&entity).copied() != Some(generation) {
                    return Vec::new();
                }
                if self.dead(entity) {
                    return Vec::new();
                }
                let mut values = [0.0f64; 2];
                for (index, field) in fields[6..8].iter().enumerate() {
                    let Some(value) = py_float(field) else {
                        return Vec::new();
                    };
                    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                        return Vec::new();
                    }
                    values[index] = value;
                }
                if let Some(old) = self.health.get(&entity) {
                    if seq <= old.seq {
                        return Vec::new();
                    }
                }
                self.health.insert(
                    entity,
                    HealthFrame {
                        lease: peer_lease.to_string(),
                        seq,
                        at: now,
                        fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    },
                );
                if let Some(seen) = self.health_seen.get_mut(&entity) {
                    *seen = now;
                }
            }
            "SHOT" if fields.len() == 10 => {
                let (Some(shooter), Some(shooter_generation), Some(victim), Some(victim_generation)) = (
                    super::economy::integer(fields[3], 2, false),
                    sequence(fields[4]),
                    super::economy::integer(fields[5], 4, false),
                    sequence(fields[6]),
                ) else {
                    return Vec::new();
                };
                let (Some(seq), Some(hits)) = (
                    sequence(fields[7]),
                    super::economy::integer(fields[8], 1_000_000, false),
                ) else {
                    return Vec::new();
                };
                if shooter != 1 && shooter != 2 {
                    return Vec::new();
                }
                if self.owners.get(&shooter).map(String::as_str) != Some(slot) {
                    return Vec::new();
                }
                if self.pilot(shooter) != slot {
                    return Vec::new();
                }
                if shooter_generation != self.health_generation(shooter) {
                    return Vec::new();
                }
                if victim == 0 || victim == shooter {
                    return Vec::new();
                }
                if victim_generation != self.victim_generation(victim) {
                    return Vec::new();
                }
                if self.victim_authority(victim) == slot {
                    return Vec::new();
                }
                if slot != "p1" && self.health_live.get(&shooter).copied() != Some(shooter_generation) {
                    return Vec::new();
                }
                if !is_shot_digest(fields[9]) {
                    return Vec::new();
                }
                let key = (shooter, shooter_generation, victim, victim_generation);
                if let Some(old) = self.shots.get(&key) {
                    if old.lease != peer_lease
                        || seq <= old.seq
                        || hits < old.hits
                        || old.fields.last().map(String::as_str) != Some(fields[9])
                    {
                        return Vec::new();
                    }
                }
                self.shots.insert(
                    key,
                    Shot {
                        lease: peer_lease.to_string(),
                        seq,
                        hits,
                        at: now,
                        fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    },
                );
            }
            "HACK" if fields.len() == 5 => {
                let (Some(entity), Some(generation)) =
                    (super::economy::integer(fields[3], 2, false), sequence(fields[4]))
                else {
                    return Vec::new();
                };
                if (entity == 1 || entity == 2)
                    && self.owners.get(&entity).map(String::as_str) == Some(slot)
                    && self.pilot(entity) == slot
                    && generation == self.health_generation(entity)
                    && self.health_live.get(&entity).copied() != Some(generation)
                {
                    if let Some(live) = self.health_live.get_mut(&entity) {
                        *live = generation;
                    }
                    if let Some(seen) = self.health_seen.get_mut(&entity) {
                        *seen = now;
                    }
                }
            }
            "HSTALE" if fields.len() == 5 => {
                let (Some(entity), Some(generation)) =
                    (super::economy::integer(fields[3], 2, false), sequence(fields[4]))
                else {
                    return Vec::new();
                };
                if slot == "p1"
                    && (entity == 1 || entity == 2)
                    && generation == self.health_generation(entity)
                {
                    self.end_health(entity);
                }
            }
            "BODY2" if fields.len() == 14 => {
                let (Some(seq), Some(dead), Some(generation)) = (
                    sequence(fields[3]),
                    super::economy::integer(fields[5], 1, false),
                    sequence(fields[6]),
                ) else {
                    return Vec::new();
                };
                let mode = fields[4];
                if !["ship", "foot", "suit"].contains(&mode) || (mode == "suit" && generation == 0) {
                    return Vec::new();
                }
                if !is_atom(fields[7]) || !fields[7].ends_with("_macro") {
                    return Vec::new();
                }
                if parse_pose(&fields[8..]).is_none() {
                    return Vec::new();
                }
                if let Some(old) = self.bodies.get(peer_lease) {
                    let old_seq: i64 = old[0].parse().unwrap_or(i64::MIN);
                    let old_dead: i64 = old[2].parse().unwrap_or(0);
                    let old_generation: i64 = old[3].parse().unwrap_or(0);
                    if seq <= old_seq || dead < old_dead || generation < old_generation {
                        return Vec::new();
                    }
                }
                let mut row = vec![
                    seq.to_string(),
                    mode.to_string(),
                    dead.to_string(),
                    generation.to_string(),
                ];
                row.extend(fields[8..].iter().map(|s| s.to_string()));
                self.bodies.insert(peer_lease.to_string(), row);
                self.body_sectors
                    .insert(peer_lease.to_string(), fields[7].to_string());
            }
            "PERF" if fields.len() == 9 => {
                let Some(seq) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let mut values = [0.0f64; 3];
                for (index, field) in fields[4..7].iter().enumerate() {
                    let Some(value) = py_float(field) else {
                        return Vec::new();
                    };
                    if !value.is_finite() || !(0.0..=60_000.0).contains(&value) {
                        return Vec::new();
                    }
                    values[index] = value;
                }
                if !["ship", "foot", "suit", "paused", "waiting"].contains(&fields[7]) {
                    return Vec::new();
                }
                if !is_atom(fields[8]) {
                    return Vec::new();
                }
                if let Some(old) = self.game_metrics.get(peer_lease) {
                    if seq <= old.seq {
                        return Vec::new();
                    }
                }
                self.game_metrics.insert(
                    peer_lease.to_string(),
                    PerfFrame {
                        seq,
                        at: now,
                        callbacks_hz: values[0],
                        interval_avg_ms: values[1],
                        interval_max_ms: values[2],
                        state: fields[7].to_string(),
                        sector: fields[8].to_string(),
                    },
                );
            }
            "COOPORDER" if fields.len() == 9 => {
                let (request, host, entity_field, epoch_field, market, lines) = (
                    fields[3], fields[4], fields[5], fields[6], fields[7], fields[8],
                );
                let (Some(entity), Some(epoch)) = (
                    super::economy::integer(entity_field, 2, false),
                    sequence(epoch_field),
                ) else {
                    return Vec::new();
                };
                if entity != 1 && entity != 2 {
                    return Vec::new();
                }
                if !is_lease(request) {
                    return Vec::new();
                }
                if super::economy::parse_lines(lines).is_none() {
                    return Vec::new();
                }
                if let Some(existing) = self.order_ships.get(request).copied() {
                    if existing != (entity, epoch) {
                        return Vec::new();
                    }
                } else if self.owners.get(&entity).map(String::as_str) != Some(slot)
                    || self.epochs.get(&entity).copied() != Some(epoch)
                    || self.pilot(entity) != slot
                    || self.dead(entity)
                {
                    return vec![self.order_reply(peer_lease, request, "REJECTED")];
                }
                let rewritten = format!(
                    "ORDER|1|{peer_lease}|{request}|{host}|{market}|{lines}"
                );
                let reply = self.receive_economy(rewritten.as_bytes(), _address, now);
                if self.orders.contains_key(request) {
                    self.order_ships.insert(request.to_string(), (entity, epoch));
                }
                return reply;
            }
            _ => {}
        }
        Vec::new()
    }

    fn receive_navigation(
        &mut self,
        kind: &str,
        fields: &[&str],
        slot: &str,
        peer_lease: &str,
        now: f64,
    ) {
        let (Some(entity), Some(epoch), Some(seq)) = (
            super::economy::integer(fields[3], 2, false),
            sequence(fields[4]),
            sequence(fields[5]),
        ) else {
            return;
        };
        if entity != 1 && entity != 2 {
            return;
        }
        if self.owners.get(&entity).map(String::as_str) != Some(slot) {
            return;
        }
        if self.epochs.get(&entity).copied() != Some(epoch) {
            return;
        }
        if !is_atom(fields[6]) || !fields[6].ends_with("_macro") {
            return;
        }
        if parse_pose(&fields[7..13]).is_none() {
            return;
        }
        if super::economy::integer(fields[13], 2, false).is_none() {
            return;
        }
        if sequence(fields[14]).is_none() {
            return;
        }
        if kind == "NAV2" {
            let Some(source_time) = py_float(fields[15]) else {
                return;
            };
            if !source_time.is_finite() || !(0.0..=1e12).contains(&source_time) {
                return;
            }
            let mut velocity = [0.0f64; 6];
            for (index, field) in fields[16..22].iter().enumerate() {
                let Some(value) = py_float(field) else {
                    return;
                };
                let limit = if index < 3 { VELOCITY_LIMIT } else { std::f64::consts::TAU };
                if !value.is_finite() || value.abs() > limit {
                    return;
                }
                velocity[index] = value;
            }
            if let Some(old) = self.navigation.get(&entity) {
                if old.lease == peer_lease && seq <= old.seq {
                    return;
                }
                if old.kind == "NAV2"
                    && old.fields.len() > 12
                    && py_float(&old.fields[12]).map(|t| source_time <= t).unwrap_or(false)
                {
                    return;
                }
            }
            if self.dead(entity) {
                return;
            }
            self.navigation.insert(
                entity,
                Navigation {
                    lease: peer_lease.to_string(),
                    seq,
                    at: now,
                    fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    kind: kind.to_string(),
                },
            );
            return;
        }
        if let Some(old) = self.navigation.get(&entity) {
            if old.lease == peer_lease && seq <= old.seq {
                return;
            }
        }
        if self.dead(entity) {
            return;
        }
        self.navigation.insert(
            entity,
            Navigation {
                lease: peer_lease.to_string(),
                seq,
                at: now,
                fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                kind: kind.to_string(),
            },
        );
    }

    /// Returns the slot currently flying a co-op ship, or `"0"`.
    pub fn pilot(&self, entity: i64) -> String {
        for (slot, peer) in self.peers.iter() {
            if self.crew.get(&peer.lease).map(|crew| crew.1) == Some(entity) {
                return slot.clone();
            }
        }
        "0".to_string()
    }

    /// Whether a ship already has an order in flight.
    pub fn busy(&self, entity: i64) -> bool {
        self.commands
            .values()
            .any(|command| command.result == "PENDING" && command.ship == Some(entity))
    }

    /// Whether a co-op ship is destroyed.
    pub fn dead(&self, entity: i64) -> bool {
        match &self.world {
            Some(world) => {
                let index = ((entity - 1) * 3) as usize;
                index + 2 < world.len() && world[index] <= 1.01
            }
            None => false,
        }
    }

    /// Ends a ship's health generation.
    pub fn end_health(&mut self, entity: i64) {
        if let Some(generation) = self.health_gen.get_mut(&entity) {
            *generation += 1;
        }
        if let Some(live) = self.health_live.get_mut(&entity) {
            *live = 0;
        }
        self.health.remove(&entity);
    }

    /// The health generation a co-op ship currently carries.
    pub fn victim_generation(&mut self, victim: i64) -> i64 {
        if victim == 1 || victim == 2 {
            self.health_generation(victim)
        } else {
            0
        }
    }

    /// The slot whose world decides a victim's state.
    pub fn victim_authority(&mut self, victim: i64) -> String {
        if victim == 1 || victim == 2 {
            let slot = self.owners.get(&victim).cloned().unwrap_or_default();
            let generation = self.health_generation(victim);
            if slot != "p1" && self.pilot(victim) == slot && self.health_live.get(&victim).copied() == Some(generation) {
                return slot;
            }
        }
        "p1".to_string()
    }

    /// The current health generation of a co-op ship, bumping it when the
    /// (owner, epoch, pilot) key changes.
    pub fn health_generation(&mut self, entity: i64) -> i64 {
        let key = (
            self.owners.get(&entity).cloned().unwrap_or_default(),
            self.epochs.get(&entity).copied().unwrap_or(0),
            self.pilot(entity),
        );
        let current = self.health_key.get(&entity).cloned();
        if current.as_ref() != Some(&key) {
            self.health_key.insert(entity, key);
            if let Some(generation) = self.health_gen.get_mut(&entity) {
                *generation += 1;
            }
            self.health.remove(&entity);
        }
        self.health_gen.get(&entity).copied().unwrap_or(0)
    }

    /// Produces co-op-layer snapshots.
    pub(super) fn snapshots_coop(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = Vec::new();
        for (address, packet) in self.snapshots_economy(now) {
            let text = String::from_utf8_lossy(&packet).to_string();
            let mut fields: Vec<String> = text.split('|').map(|s| s.to_string()).collect();
            if ["PEER", "BODY", "FIRE", "DOCKSTATE"].contains(&fields[0].as_str()) {
                continue;
            }
            if fields[0] == "ORDER_EXEC" {
                let Some(ship) = self.order_ships.get(&fields[3]).map(|value| value.0) else {
                    continue;
                };
                if ship == 0 {
                    continue;
                }
                fields[0] = "COOP_EXEC".to_string();
                fields[4] = ship.to_string();
                result.push((address, fields.join("|").into_bytes()));
                continue;
            }
            if fields[0] == "COMMAND" {
                let ship = self
                    .commands
                    .get(&fields[3])
                    .and_then(|command| command.ship)
                    .unwrap_or(0);
                if ship == 0 {
                    continue;
                }
                fields[4] = format!("p{ship}");
                result.push((address, fields.join("|").into_bytes()));
                continue;
            }
            result.push((address, packet));
        }

        let slots: Vec<String> = self.peers.keys().cloned().collect();
        for slot in slots {
            let Some(peer) = self.peers.get(&slot).cloned() else {
                continue;
            };
            let mut authority = vec![self.authority_revision.to_string()];
            for entity in [1, 2] {
                authority.push(self.owners.get(&entity).cloned().unwrap_or_default());
                authority.push(self.epochs.get(&entity).copied().unwrap_or(0).to_string());
                authority.push(self.pilot(entity));
            }
            result.push((
                peer.address,
                format!("AUTH|1|{}|{}", peer.lease, authority.join("|")).into_bytes(),
            ));
            for (entity, nav) in self.navigation.iter() {
                if self.owners.get(entity).map(String::as_str) == Some(slot.as_str())
                    || now - nav.at > 0.75
                    || self.dead(*entity)
                {
                    continue;
                }
                result.push((
                    peer.address,
                    format!(
                        "{}|1|{}|{}|{}",
                        nav.kind,
                        peer.lease,
                        nav.lease,
                        nav.fields.join("|")
                    )
                    .into_bytes(),
                ));
            }
            for (entity, fire) in self.fire2.iter() {
                if self.owners.get(entity).map(String::as_str) == Some(slot.as_str())
                    || fire.epoch != self.epochs.get(entity).copied().unwrap_or(0)
                    || now - fire.at > 0.75
                    || self.dead(*entity)
                {
                    continue;
                }
                result.push((
                    peer.address,
                    format!("FIRE2|1|{}|{}|{}", peer.lease, fire.lease, fire.fields.join("|"))
                        .into_bytes(),
                ));
            }
            let generation_one = self.health_generation(1);
            let generation_two = self.health_generation(2);
            result.push((
                peer.address,
                format!(
                    "HGEN|1|{}|{}|{}|{}",
                    peer.lease, self.authority_revision, generation_one, generation_two
                )
                .into_bytes(),
            ));
            for entity in [1, 2] {
                let generation = self.health_generation(entity);
                let health = self.health.get(&entity).cloned();
                if let Some(health) = health {
                    if self.owners.get(&entity).map(String::as_str) != Some(slot.as_str())
                        && now - health.at <= 0.75
                        && !self.dead(entity)
                    {
                        result.push((
                            peer.address,
                            format!(
                                "HEALTH|1|{}|{}|{}",
                                peer.lease,
                                health.lease,
                                health.fields.join("|")
                            )
                            .into_bytes(),
                        ));
                    }
                }
                if slot != "p1"
                    && self.owners.get(&entity).map(String::as_str) == Some(slot.as_str())
                    && self.pilot(entity) == slot
                    && self.health_live.get(&entity).copied() != Some(generation)
                    && self.world.is_some()
                    && !self.dead(entity)
                {
                    let world = self.world.as_ref().unwrap();
                    let index = ((entity - 1) * 3) as usize;
                    let hull = world[index];
                    let shield = world[index + 1];
                    result.push((
                        peer.address,
                        format!(
                            "HBASE|1|{}|{entity}|{generation}|{hull:.3}|{shield:.3}",
                            peer.lease
                        )
                        .into_bytes(),
                    ));
                }
            }
            let shot_keys: Vec<(i64, i64, i64, i64)> = self.shots.keys().copied().collect();
            for key in shot_keys {
                let (shooter, shooter_generation, victim, victim_generation) = key;
                if shooter_generation != self.health_generation(shooter)
                    || victim_generation != self.victim_generation(victim)
                {
                    // An ended generation never forwards.
                    self.shots.remove(&key);
                    continue;
                }
                let Some(shot) = self.shots.get(&key).cloned() else {
                    continue;
                };
                if self.victim_authority(victim) == slot
                    && self.owners.get(&shooter).map(String::as_str) != Some(slot.as_str())
                    && now - shot.at <= 0.75
                    && !self.dead(shooter)
                {
                    result.push((
                        peer.address,
                        format!("SHOT|1|{}|{}|{}", peer.lease, shot.lease, shot.fields.join("|"))
                            .into_bytes(),
                    ));
                }
            }
            let other_slot = if slot == "p1" { "p2" } else { "p1" };
            if let Some(other) = self.peers.get(other_slot) {
                if let (Some(body), Some(sector)) = (
                    self.bodies.get(&other.lease).cloned(),
                    self.body_sectors.get(&other.lease).cloned(),
                ) {
                    let mut fields = vec![other.lease.clone()];
                    fields.extend(body.iter().take(4).cloned());
                    fields.push(sector);
                    fields.extend(body.iter().skip(4).cloned());
                    result.push((
                        peer.address,
                        format!("BODY2|1|{}|{}", peer.lease, fields.join("|")).into_bytes(),
                    ));
                }
            }
        }
        result.extend(self.quest.snapshots(&self.peers, now));
        result.extend(self.snapshots_mission(now));
        result
    }

    /// Releases co-op-layer state when peers expire.
    pub(super) fn expire_coop(&mut self, now: f64) {
        self.expire_economy(now);
        let live: Vec<String> = self.peers.values().map(|peer| peer.lease.clone()).collect();
        let expired: Vec<String> = self
            .crew
            .keys()
            .filter(|lease| !live.contains(lease))
            .cloned()
            .collect();
        if !expired.is_empty() {
            self.authority_revision += 1;
        }
        self.crew.retain(|lease, _| live.contains(lease));
        let retained: Vec<String> = live
            .iter()
            .cloned()
            .chain(
                self.resumable
                    .values()
                    .map(|(peer, _)| peer.lease.clone()),
            )
            .collect();
        self.body_sectors.retain(|lease, _| retained.contains(lease));
        self.game_metrics.retain(|lease, _| live.contains(lease));
        self.health.retain(|_, frame| live.contains(&frame.lease));
        // Every message and snapshot starts here, so each authority transition
        // gets its own generation even when no snapshot falls between a leave
        // and a re-entry.
        for entity in [1, 2] {
            let generation = self.health_generation(entity);
            if self.health_live.get(&entity).copied() == Some(generation)
                && now - self.health_seen.get(&entity).copied().unwrap_or(0.0) > HEALTH_STALE
            {
                self.end_health(entity);
            }
        }
    }
}
