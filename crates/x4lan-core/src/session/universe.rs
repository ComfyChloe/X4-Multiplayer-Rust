//! Whole-sector world synchronisation: baselines, cargo, identities and NPC combat.
//!
//! Port of `x4lan/universe.py` together with the `cargo`, `identity` and
//! `npc_combat` helpers it composes. The host's X4 process supplies state;
//! absence means removal from a sector view, not proof of global destruction.

use super::{Position, Session};
use crate::format::py_float;
use crate::ordered::OrderedMap;
use crate::protocol::{is_atom, is_lease, is_ware, sequence, PeerAddress, MAX_PACKET};

/// Largest number of objects in one sector baseline.
const MAX_OBJECTS: i64 = 4096;
/// Largest single record.
const MAX_RECORD: usize = 512;
/// Acknowledge window size.
const WINDOW: usize = 12;
/// How many motion samples are relayed per tick.
const MOTION_BUDGET: usize = 32;
/// Retransmission interval, in seconds.
const RETRY: f64 = 0.25;
/// How long a staged transfer may sit idle.
const STAGE_TIMEOUT: f64 = 15.0;

/// A verified sector baseline.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub epoch: String,
    pub revision: i64,
    pub sector: String,
    pub rows: Vec<String>,
    pub digest: u32,
}

impl Baseline {
    /// Creates a baseline, rejecting duplicate or unordered identifiers.
    pub fn create(epoch: &str, revision: i64, sector: &str, rows: Vec<String>) -> Option<Baseline> {
        if !is_lease(epoch) || !(1..=2_147_483_647).contains(&revision) {
            return None;
        }
        if !is_atom(sector) || !sector.ends_with("_macro") {
            return None;
        }
        if rows.len() as i64 > MAX_OBJECTS {
            return None;
        }
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            ids.push(parse_record(row)?);
        }
        // Records must have unique increasing identifiers.
        for pair in ids.windows(2) {
            if pair[0] >= pair[1] {
                return None;
            }
        }
        let digest = checksum(&rows);
        Some(Baseline {
            epoch: epoch.to_string(),
            revision,
            sector: sector.to_string(),
            rows,
            digest,
        })
    }

    /// Renders one packet of the transfer.
    pub fn packet(&self, lease: &str, index: usize) -> Option<Vec<u8>> {
        let head = format!("|1|{lease}|{}|{}", self.epoch, self.revision);
        let body = if index == 0 {
            format!(
                "UBEGIN{head}|{}|{}|{}",
                self.sector,
                self.rows.len(),
                self.digest
            )
        } else if index == self.rows.len() + 1 {
            format!("UCOMMIT{head}")
        } else {
            format!("UOBJECT{head}|{index}|{}", self.rows[index - 1])
        };
        let data = body.into_bytes();
        if data.len() > MAX_PACKET {
            return None;
        }
        Some(data)
    }
}

/// Parses and bounds-checks one object record.
pub fn parse_record(value: &str) -> Option<i64> {
    if value.len() > MAX_RECORD || !value.is_ascii() || value.chars().any(char::is_whitespace) {
        return None;
    }
    let fields: Vec<&str> = value.split(',').collect();
    if fields.len() != 12 {
        return None;
    }
    let id = sequence(fields[0])?;
    if fields[1] != "ship" && fields[1] != "drop" {
        return None;
    }
    if !is_atom(fields[2]) || !fields[2].ends_with("_macro") {
        return None;
    }
    if !is_atom(fields[3]) || !is_atom(fields[10]) {
        return None;
    }
    for (index, field) in fields[4..10].iter().enumerate() {
        let value = py_float(field)?;
        if !value.is_finite() {
            return None;
        }
        let limit = if index < 3 {
            50_000_000.0
        } else {
            std::f64::consts::TAU
        };
        if value.abs() > limit {
            return None;
        }
    }
    let amount = sequence(fields[11])?;
    if amount > 1_000_000 {
        return None;
    }
    if fields[1] == "ship" && (fields[10] != "none" || amount != 0) {
        return None;
    }
    if fields[1] == "drop" && (fields[10] == "none" || amount == 0) {
        return None;
    }
    Some(id)
}

/// Computes the assembly checksum.
///
/// This is an integrity check for assembly, not authentication; the lease
/// authenticates the peer.
pub fn checksum(rows: &[String]) -> u32 {
    let joined = rows.join("\n");
    adler32(joined.as_bytes())
}

/// Computes an Adler-32 checksum, matching Python's `zlib.adler32`.
pub fn adler32(data: &[u8]) -> u32 {
    const BASE: u32 = 65521;
    let mut low: u32 = 1;
    let mut high: u32 = 0;
    for &byte in data {
        low = (low + byte as u32) % BASE;
        high = (high + low) % BASE;
    }
    (high << 16) | low
}

/// Reassembles a baseline that arrives in numbered parts.
#[derive(Debug, Clone, Default)]
pub struct Receiver {
    pub latest: Option<Baseline>,
    pub pending: Option<PendingTransfer>,
}

/// A transfer in progress.
#[derive(Debug, Clone)]
pub struct PendingTransfer {
    pub identity: (String, i64, String, i64, u32),
    pub rows: OrderedMap<i64, String>,
    pub at: f64,
}

impl Receiver {
    pub fn new() -> Self {
        Self {
            latest: None,
            pending: None,
        }
    }

    /// Accepts one transfer packet.
    ///
    /// Returns the acknowledged index and, once complete, the committed
    /// baseline. Only a verified commit replaces visible state.
    pub fn receive(&mut self, fields: &[&str], now: f64) -> (Option<i64>, Option<Baseline>) {
        if fields.len() < 5 || !is_lease(fields[3]) {
            return (None, None);
        }
        let epoch = fields[3];
        let Some(revision) = sequence(fields[4]) else {
            return (None, None);
        };
        if revision == 0 {
            return (None, None);
        }
        if let Some(old) = &self.latest {
            if epoch != old.epoch || revision < old.revision {
                return (None, None);
            }
            if revision == old.revision {
                // A lost final acknowledgement: retransmit completion, not effects.
                return (Some(old.rows.len() as i64 + 1), None);
            }
        }
        if let Some(pending) = &self.pending {
            if now - pending.at > STAGE_TIMEOUT {
                self.pending = None;
            }
        }
        if fields[0] == "UBEGIN" && fields.len() == 8 {
            let sector = fields[5];
            if !is_atom(sector) || !sector.ends_with("_macro") {
                return (None, None);
            }
            let Some(count) = sequence(fields[6]) else {
                return (None, None);
            };
            if count > MAX_OBJECTS {
                return (None, None);
            }
            if !fields[7].is_ascii() || !fields[7].bytes().all(|b| b.is_ascii_digit()) {
                return (None, None);
            }
            let Ok(digest) = fields[7].parse::<u64>() else {
                return (None, None);
            };
            if digest > u32::MAX as u64 {
                return (None, None);
            }
            let identity = (
                epoch.to_string(),
                revision,
                sector.to_string(),
                count,
                digest as u32,
            );
            if let Some(pending) = &self.pending {
                if pending.identity != identity {
                    // A newer begin cannot evict in-flight work.
                    return (None, None);
                }
            } else {
                self.pending = Some(PendingTransfer {
                    identity,
                    rows: OrderedMap::new(),
                    at: now,
                });
            }
            return (Some(0), None);
        }
        let Some(pending) = &self.pending else {
            // The sender must restart this transfer from its begin.
            return (Some(-1), None);
        };
        if pending.identity.0 != epoch || pending.identity.1 != revision {
            return (Some(-1), None);
        }
        let (sector, count, digest) = (
            pending.identity.2.clone(),
            pending.identity.3,
            pending.identity.4,
        );
        if fields[0] == "UOBJECT" && fields.len() == 7 {
            let Some(index) = sequence(fields[5]) else {
                return (None, None);
            };
            if parse_record(fields[6]).is_none() {
                return (None, None);
            }
            if !(1..=count).contains(&index) {
                return (None, None);
            }
            if let Some(pending) = &mut self.pending {
                if let Some(existing) = pending.rows.get(&index) {
                    if existing != fields[6] {
                        self.pending = None;
                        return (Some(-1), None);
                    }
                } else {
                    pending.rows.insert(index, fields[6].to_string());
                    pending.at = now;
                }
            }
            return (Some(index), None);
        }
        if fields[0] == "UCOMMIT" && fields.len() == 5 {
            let Some(pending) = &self.pending else {
                return (Some(-1), None);
            };
            if pending.rows.len() as i64 != count {
                return (None, None);
            }
            let mut rows = Vec::with_capacity(count as usize);
            for index in 1..=count {
                match pending.rows.get(&index) {
                    Some(row) => rows.push(row.clone()),
                    None => return (None, None),
                }
            }
            let Some(baseline) = Baseline::create(epoch, revision, &sector, rows) else {
                self.pending = None;
                return (Some(-1), None);
            };
            if baseline.digest != digest {
                self.pending = None;
                return (Some(-1), None);
            }
            self.latest = Some(baseline.clone());
            self.pending = None;
            return (Some(count + 1), Some(baseline));
        }
        (None, None)
    }
}

/// Sends a baseline with selective acknowledgement and a bounded window.
#[derive(Debug, Clone)]
pub struct Sender {
    pub baseline: Baseline,
    pub acked: Vec<usize>,
    pub sent_at: OrderedMap<usize, f64>,
    pub done: bool,
}

impl Sender {
    pub fn new(baseline: Baseline) -> Self {
        Self {
            baseline,
            acked: Vec::new(),
            sent_at: OrderedMap::new(),
            done: false,
        }
    }

    /// Records an acknowledgement.
    pub fn acknowledge(&mut self, epoch: &str, revision: i64, index: i64) {
        if epoch != self.baseline.epoch || revision != self.baseline.revision {
            return;
        }
        let end = self.baseline.rows.len() + 1;
        if index == -1 {
            self.acked.clear();
            self.sent_at = OrderedMap::new();
        } else if index as usize == end {
            if self.sent_at.contains_key(&end) {
                self.done = true;
            }
        } else if index >= 0 && (index as usize) < end {
            let slot = index as usize;
            if self.sent_at.contains_key(&slot) {
                self.acked.push(slot);
            }
        }
    }

    /// Produces the packets to send now.
    pub fn packets(&mut self, lease: &str, now: f64) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        let indices: Vec<usize> = if !self.acked.contains(&0) {
            vec![0]
        } else {
            let outstanding: Vec<usize> = (1..=self.baseline.rows.len())
                .filter(|index| !self.acked.contains(index))
                .collect();
            if outstanding.is_empty() {
                vec![self.baseline.rows.len() + 1]
            } else {
                outstanding.into_iter().take(WINDOW).collect()
            }
        };
        let mut result = Vec::new();
        for index in indices {
            let last = self
                .sent_at
                .get(&index)
                .copied()
                .unwrap_or(f64::NEG_INFINITY);
            if now - last >= RETRY {
                self.sent_at.insert(index, now);
                if let Some(packet) = self.baseline.packet(lease, index) {
                    result.push(packet);
                }
            }
        }
        result
    }
}

/// Reliable native cargo intentions.
#[derive(Debug, Clone)]
pub struct CargoTransactions {
    pub requests: OrderedMap<String, CargoRequest>,
    pub inventory: OrderedMap<String, (i64, f64, Vec<String>)>,
    pub next_inventory: f64,
    pub inventory_cursor: usize,
}

/// A pending cargo request.
#[derive(Debug, Clone)]
pub struct CargoRequest {
    pub signature: (String, String, i64, i64, String, i64, String, i64),
    pub ship: i64,
    pub host: String,
    pub requester: String,
    pub result: String,
    pub next_send: f64,
    pub fence: i64,
}

impl Default for CargoTransactions {
    fn default() -> Self {
        Self::new()
    }
}

impl CargoTransactions {
    pub fn new() -> Self {
        Self {
            requests: OrderedMap::new(),
            inventory: OrderedMap::new(),
            next_inventory: 0.0,
            inventory_cursor: 0,
        }
    }
}

/// Host display identity and shared sensor observations.
#[derive(Debug, Clone, Default)]
pub struct Identities {
    pub rows: OrderedMap<(String, i64), (i64, f64, Vec<String>)>,
    pub cursor: usize,
    pub next_send: f64,
}

impl Identities {
    pub fn new() -> Self {
        Self {
            rows: OrderedMap::new(),
            cursor: 0,
            next_send: 0.0,
        }
    }
}

impl Session {
    /// Handles a packet in the universe layer.
    pub(super) fn receive_universe(
        &mut self,
        raw: &[u8],
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        if raw.starts_with(b"NHP|") || raw.starts_with(b"NCLAIM|") || raw.starts_with(b"NACK|") {
            return self.receive_npc_combat(raw, address, now);
        }
        if raw.starts_with(b"IDENT|") {
            return self.receive_identities(raw, address, now);
        }
        if raw.starts_with(b"CARGOREQ|")
            || raw.starts_with(b"CARGODONE|")
            || raw.starts_with(b"CINV|")
        {
            return self.receive_cargo(raw, address, now);
        }
        if !(raw.starts_with(b"UBEGIN|")
            || raw.starts_with(b"UOBJECT|")
            || raw.starts_with(b"UCOMMIT|")
            || raw.starts_with(b"UACK|")
            || raw.starts_with(b"UINTEREST|")
            || raw.starts_with(b"UPOS|")
            || raw.starts_with(b"UMOVE|"))
        {
            return self.receive_dock(raw, address, now);
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
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let peer_lease = self
            .peers
            .get(&slot)
            .map(|p| p.lease.clone())
            .unwrap_or_default();

        if fields[0] == "UINTEREST" && slot == "p2" && fields.len() == 5 {
            let Some(seq) = sequence(fields[3]) else {
                return Vec::new();
            };
            let sector = fields[4];
            if !is_atom(sector) || !sector.ends_with("_macro") {
                return Vec::new();
            }
            let old = self.interest.clone();
            if let Some((old_lease, old_seq, _)) = &old {
                if old_lease == &peer_lease && seq <= *old_seq {
                    return Vec::new();
                }
            }
            self.interest = Some((peer_lease.clone(), seq, sector.to_string()));
            let changed = match &old {
                None => true,
                Some((old_lease, _, old_sector)) => {
                    old_lease != &peer_lease || old_sector != sector
                }
            };
            if changed {
                self.delivery = None;
                self.delivered = None;
                // The guest's receiver has a global monotonic revision. On
                // sector return an older cached baseline would be rejected
                // forever, so require a capture newer than anything already
                // sent to this guest.
                self.interest_after_revision = match &old {
                    Some((old_lease, _, _)) if old_lease == &peer_lease => self
                        .universe
                        .latest
                        .as_ref()
                        .map(|baseline| baseline.revision)
                        .unwrap_or(0),
                    _ => 0,
                };
            }
            return Vec::new();
        }

        if fields[0] == "UACK" && slot == "p2" && fields.len() == 6 {
            let Some(revision) = sequence(fields[4]) else {
                return Vec::new();
            };
            let index = if fields[5] == "-1" {
                -1
            } else {
                match sequence(fields[5]) {
                    Some(value) => value,
                    None => return Vec::new(),
                }
            };
            if self.delivery.is_some()
                && self.delivery_lease.as_deref() == Some(peer_lease.as_str())
            {
                let epoch = fields[3].to_string();
                if let Some(delivery) = self.delivery.as_mut() {
                    delivery.acknowledge(&epoch, revision, index);
                    if delivery.done {
                        self.delivered = Some(delivery.baseline.revision);
                    }
                }
            }
            return Vec::new();
        }

        if slot != "p1" || fields.len() < 5 || fields[3] != peer_lease {
            return Vec::new();
        }
        if let Some(epoch) = &self.universe_epoch {
            if epoch != &peer_lease {
                // A fresh host game needs a new relay and a fresh guest.
                return Vec::new();
            }
        }

        if (fields[0] == "UPOS" && fields.len() == 13)
            || (fields[0] == "UMOVE" && fields.len() == 20)
        {
            let (Some(entity), Some(seq)) = (sequence(fields[4]), sequence(fields[5])) else {
                return Vec::new();
            };
            let sector = fields[6];
            if !is_atom(sector) || !sector.ends_with("_macro") {
                return Vec::new();
            }
            let mut sample_time = f64::NEG_INFINITY;
            if fields[0] == "UPOS" {
                if super::coop::parse_pose(&fields[7..13]).is_none() {
                    return Vec::new();
                }
            } else {
                let Some(value) = py_float(fields[7]) else {
                    return Vec::new();
                };
                if !value.is_finite() || !(0.0..=1e12).contains(&value) {
                    return Vec::new();
                }
                sample_time = value;
                if super::coop::parse_pose(&fields[8..14]).is_none() {
                    return Vec::new();
                }
                for (index, field) in fields[14..20].iter().enumerate() {
                    let Some(value) = py_float(field) else {
                        return Vec::new();
                    };
                    let limit = if index < 3 {
                        100_000.0
                    } else {
                        std::f64::consts::TAU
                    };
                    if !value.is_finite() || value.abs() > limit {
                        return Vec::new();
                    }
                }
            }
            let Some(baseline) = self.sectors.get(sector) else {
                return Vec::new();
            };
            if !baseline
                .rows
                .iter()
                .any(|row| parse_record(row) == Some(entity))
            {
                return Vec::new();
            }
            if let Some(old) = self.positions.get(&entity) {
                if seq <= old.seq {
                    return Vec::new();
                }
                if old.kind == "UMOVE" && fields[0] == "UMOVE" {
                    let old_time = old
                        .fields
                        .get(4)
                        .and_then(|value| py_float(value))
                        .unwrap_or(f64::INFINITY);
                    if sample_time <= old_time {
                        return Vec::new();
                    }
                }
            }
            self.positions.insert(
                entity,
                Position {
                    seq,
                    at: now,
                    sector: sector.to_string(),
                    fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    kind: fields[0].to_string(),
                },
            );
            return Vec::new();
        }

        let (ack, committed) = self.universe.receive(&fields, now);
        let Some(ack) = ack else {
            return Vec::new();
        };
        self.universe_epoch = Some(peer_lease.clone());
        if let Some(committed) = committed {
            self.npc_combat.committed(&committed);
            self.sectors
                .insert(committed.sector.clone(), committed.clone());
            while self.sectors.len() > 8 {
                let oldest = self.sectors.keys().next().cloned().unwrap_or_default();
                self.sectors.remove(&oldest);
            }
            self.entity_ids = OrderedMap::new();
            let mut live: Vec<i64> = Vec::new();
            for (sector, baseline) in self.sectors.iter() {
                let mut ids = Vec::with_capacity(baseline.rows.len());
                for row in &baseline.rows {
                    if let Some(id) = parse_record(row) {
                        ids.push(id);
                    }
                }
                self.entity_ids.insert(sector.clone(), ids.clone());
                live.extend(ids);
            }
            self.positions.retain(|entity, _| live.contains(entity));
        }
        vec![format!("UACK|1|{peer_lease}|{}|{}|{ack}", fields[3], fields[4]).into_bytes()]
    }

    /// Handles a native cargo packet.
    fn receive_cargo(&mut self, raw: &[u8], address: PeerAddress, now: f64) -> Vec<Vec<u8>> {
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
        if fields.len() < 4 || fields[1] != "1" {
            return Vec::new();
        }
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let peer_lease = self
            .peers
            .get(&slot)
            .map(|p| p.lease.clone())
            .unwrap_or_default();

        if fields[0] == "CINV" && fields.len() == 8 && slot == "p1" && fields[3] == peer_lease {
            let (Some(seq), Some(first), Some(second)) = (
                sequence(fields[4]),
                sequence(fields[6]),
                sequence(fields[7]),
            ) else {
                return Vec::new();
            };
            let ware = fields[5];
            if !is_ware(ware) || first.max(second) > 1_000_000 {
                return Vec::new();
            }
            if let Some(old) = self.cargo.inventory.get(ware) {
                if old.2.first().map(String::as_str) == Some(peer_lease.as_str()) && seq <= old.0 {
                    return Vec::new();
                }
            } else if self.cargo.inventory.len() >= 256 {
                return Vec::new();
            }
            self.cargo.inventory.insert(
                ware.to_string(),
                (
                    seq,
                    now,
                    fields[3..].iter().map(|s| s.to_string()).collect(),
                ),
            );
            return Vec::new();
        }

        let request = fields[3];
        if !is_lease(request) {
            return Vec::new();
        }
        if fields[0] == "CARGODONE" && fields.len() == 6 && slot == "p1" {
            const RESULTS: [&str; 6] = ["OK", "REJECTED", "CAPACITY", "BUSY", "STALE", "UNCERTAIN"];
            if !RESULTS.contains(&fields[4]) {
                return Vec::new();
            }
            let Some(fence) = sequence(fields[5]) else {
                return Vec::new();
            };
            if let Some(item) = self.cargo.requests.get_mut(request) {
                if item.host == peer_lease && item.result == "PENDING" {
                    item.result = fields[4].to_string();
                    item.fence = fence;
                }
            }
            return Vec::new();
        }
        if fields[0] != "CARGOREQ" || fields.len() != 11 {
            return Vec::new();
        }
        let (host, ship_field, epoch_field, verb, object_field, ware, amount_field) = (
            fields[4], fields[5], fields[6], fields[7], fields[8], fields[9], fields[10],
        );
        let (Some(ship), Some(epoch), Some(object), Some(amount)) = (
            sequence(ship_field),
            sequence(epoch_field),
            sequence(object_field),
            sequence(amount_field),
        ) else {
            return Vec::new();
        };
        if !is_lease(host)
            || (ship != 1 && ship != 2)
            || epoch < 1
            || (verb != "DROP" && verb != "PICKUP")
            || !is_ware(ware)
            || amount == 0
            || amount > 1_000_000
            || (verb == "DROP" && object != 0)
            || (verb == "PICKUP" && object == 0)
        {
            return Vec::new();
        }
        let signature = (
            slot.clone(),
            host.to_string(),
            ship,
            epoch,
            verb.to_string(),
            object,
            ware.to_string(),
            amount,
        );
        if let Some(old) = self.cargo.requests.get(request).cloned() {
            if old.signature != signature {
                return vec![cargo_reply(&peer_lease, request, "REJECTED", 0)];
            }
            return vec![cargo_reply(&peer_lease, request, &old.result, old.fence)];
        }
        let authority = self.peers.get("p1").cloned();
        let Some(authority) = authority else {
            return vec![cargo_reply(&peer_lease, request, "STALE", 0)];
        };
        if authority.lease != host {
            return vec![cargo_reply(&peer_lease, request, "STALE", 0)];
        }
        if self.owners.get(&ship).map(String::as_str) != Some(slot.as_str())
            || self.epochs.get(&ship).copied() != Some(epoch)
            || self.pilot(ship) != slot
            || self.dead(ship)
        {
            return vec![cargo_reply(&peer_lease, request, "REJECTED", 0)];
        }
        if self.cargo_busy(ship) {
            return vec![cargo_reply(&peer_lease, request, "BUSY", 0)];
        }
        // Reject new requests at the cap; never evict a completed ID into reuse.
        if self.cargo.requests.len() >= 8192 {
            return vec![cargo_reply(&peer_lease, request, "BUSY", 0)];
        }
        self.cargo.requests.insert(
            request.to_string(),
            CargoRequest {
                signature,
                ship,
                host: host.to_string(),
                requester: slot.clone(),
                result: "PENDING".to_string(),
                next_send: now,
                fence: 0,
            },
        );
        vec![cargo_reply(&peer_lease, request, "PENDING", 0)]
    }

    /// Whether a ship already has a cargo request in flight.
    fn cargo_busy(&self, ship: i64) -> bool {
        self.cargo
            .requests
            .values()
            .any(|request| request.ship == ship && request.result == "PENDING")
    }

    /// Handles a host identity observation.
    fn receive_identities(&mut self, raw: &[u8], address: PeerAddress, now: f64) -> Vec<Vec<u8>> {
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
        if fields.len() != 11 || fields[0] != "IDENT" || fields[1] != "1" {
            return Vec::new();
        }
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        if slot != "p1" {
            return Vec::new();
        }
        let peer_lease = self
            .peers
            .get(&slot)
            .map(|p| p.lease.clone())
            .unwrap_or_default();
        if fields[3] != peer_lease {
            return Vec::new();
        }
        if let Some(epoch) = &self.universe_epoch {
            if epoch != &peer_lease {
                return Vec::new();
            }
        }
        let Some(seq) = sequence(fields[4]) else {
            return Vec::new();
        };
        let scope = fields[5];
        let Some(entity) = sequence(fields[6]) else {
            return Vec::new();
        };
        match scope {
            "P" => {
                if entity != 1 && entity != 2 {
                    return Vec::new();
                }
            }
            "F" => {
                if entity != 1 && entity != 2 && entity != 3 && entity != 4 {
                    return Vec::new();
                }
            }
            "N" => {
                let known = self.entity_ids.values().any(|ids| ids.contains(&entity));
                if !known {
                    return Vec::new();
                }
            }
            _ => return Vec::new(),
        }
        if !is_display_code(fields[7]) || !is_display_name(fields[8]) {
            return Vec::new();
        }
        if fields[9] != "0" && fields[9] != "1" {
            return Vec::new();
        }
        if fields[10] != "0" && fields[10] != "1" {
            return Vec::new();
        }
        let Some(decoded) = decode_hex_utf8(fields[8]) else {
            return Vec::new();
        };
        if decoded.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
            return Vec::new();
        }
        let key = (scope.to_string(), entity);
        if let Some(old) = self.identities.rows.get(&key) {
            if seq <= old.0 {
                return Vec::new();
            }
        }
        self.identities.rows.insert(
            key,
            (
                seq,
                now,
                fields[3..].iter().map(|s| s.to_string()).collect(),
            ),
        );
        Vec::new()
    }

    /// Produces universe-layer snapshots.
    pub(super) fn snapshots_universe(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = self.snapshots_dock(now);
        result.extend(self.snapshots_cargo(now));
        result.extend(self.snapshots_identities(now));
        result.extend(self.snapshots_npc_combat(now));
        let host = self.peers.get("p1").cloned();
        let guest = self.peers.get("p2").cloned();
        let (Some(host), Some(guest)) = (host, guest) else {
            return result;
        };
        let Some((interest_lease, seq, sector)) = self.interest.clone() else {
            return result;
        };
        if interest_lease != guest.lease {
            return result;
        }
        result.push((
            host.address,
            format!("UINTEREST|1|{}|{seq}|{sector}", host.lease).into_bytes(),
        ));
        if self.delivery_lease.as_deref() != Some(guest.lease.as_str()) {
            self.delivery = None;
            self.delivered = None;
            self.delivery_lease = Some(guest.lease.clone());
        }
        let latest = self.sectors.get(&sector).cloned();
        if let Some(latest) = latest {
            if latest.revision > self.interest_after_revision
                && (self.delivery.is_none() || self.delivery.as_ref().is_some_and(|d| d.done))
                && Some(latest.revision) != self.delivered
            {
                self.delivery = Some(Sender::new(latest));
            }
        }
        if let Some(mut delivery) = self.delivery.take() {
            let packets = delivery.packets(&guest.lease, now);
            self.delivery = Some(delivery);
            for packet in packets {
                result.push((guest.address, packet));
            }
        }
        // Independent, lossy motion cannot create entities or commit a baseline.
        let live: Vec<(i64, Position)> = self
            .positions
            .iter()
            .map(|(entity, position)| (*entity, position.clone()))
            .filter(|(_, position)| position.sector == sector && now - position.at < 0.75)
            .collect();
        if !live.is_empty() {
            let count = live.len();
            for offset in 0..count.min(MOTION_BUDGET) {
                let (_, position) = &live[(self.position_cursor + offset) % count];
                result.push((
                    guest.address,
                    format!(
                        "{}|1|{}|{}",
                        position.kind,
                        guest.lease,
                        position.fields.join("|")
                    )
                    .into_bytes(),
                ));
            }
            self.position_cursor = (self.position_cursor + MOTION_BUDGET) % count;
        }
        result
    }

    /// Produces cargo snapshots.
    fn snapshots_cargo(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = Vec::new();
        let host = self.peers.get("p1").cloned();
        let guest = self.peers.get("p2").cloned();
        let (Some(host), Some(guest)) = (host, guest) else {
            return result;
        };
        if now >= self.cargo.next_inventory {
            self.cargo.next_inventory = now + 0.2;
            let values: Vec<(i64, f64, Vec<String>)> =
                self.cargo.inventory.values().cloned().collect();
            if !values.is_empty() {
                let selected: Vec<&(i64, f64, Vec<String>)> = (0..values.len().min(32))
                    .map(|offset| {
                        let index = (self.cargo.inventory_cursor + offset) % values.len();
                        &values[index]
                    })
                    .collect();
                self.cargo.inventory_cursor =
                    (self.cargo.inventory_cursor + selected.len()) % values.len();
                for (_, at, fields) in selected {
                    if fields.first().map(String::as_str) == Some(host.lease.as_str())
                        && now - at < 2.0
                    {
                        result.push((
                            guest.address,
                            format!("CINV|1|{}|{}", guest.lease, fields.join("|")).into_bytes(),
                        ));
                    }
                }
            }
        }
        let requests: Vec<(String, CargoRequest)> = self
            .cargo
            .requests
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        for (request, mut item) in requests {
            if item.result != "PENDING" {
                continue;
            }
            // A timeout is uncertainty, never permission to replay into a new host.
            let host_matches = host.lease == item.host;
            if !host_matches {
                item.result = "UNCERTAIN".to_string();
                if let Some(stored) = self.cargo.requests.get_mut(&request) {
                    stored.result = "UNCERTAIN".to_string();
                }
                continue;
            }
            if now < item.next_send {
                continue;
            }
            item.next_send = now + 0.25;
            if let Some(stored) = self.cargo.requests.get_mut(&request) {
                stored.next_send = item.next_send;
            }
            let (_, _, ship, epoch, verb, object, ware, amount) = &item.signature;
            result.push((
                host.address,
                format!(
                    "CARGOEXEC|1|{}|{request}|{ship}|{epoch}|{verb}|{object}|{ware}|{amount}|{}",
                    host.lease, item.requester
                )
                .into_bytes(),
            ));
        }
        result
    }

    /// Produces identity snapshots.
    fn snapshots_identities(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        if now < self.identities.next_send {
            return Vec::new();
        }
        self.identities.next_send = now + 0.1;
        let host = self.peers.get("p1").cloned();
        let guest = self.peers.get("p2").cloned();
        let (Some(host), Some(guest)) = (host, guest) else {
            return Vec::new();
        };
        let mut live: Vec<i64> = Vec::new();
        for ids in self.entity_ids.values() {
            live.extend(ids.iter().copied());
        }
        self.identities
            .rows
            .retain(|key, _| key.0 == "P" || key.0 == "F" || live.contains(&key.1));
        // Only forward fresh observations; the guest expires forced visibility.
        let rows: Vec<Vec<String>> = self
            .identities
            .rows
            .values()
            .filter(|row| {
                row.2.first().map(String::as_str) == Some(host.lease.as_str()) && now - row.1 < 2.0
            })
            .map(|row| row.2.clone())
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }
        // The reference sends at most 256 rows, at least 32, and at most a
        // tenth of the rows it holds.
        let limit = rows.len().min(256.min(32.max(rows.len().div_ceil(10))));
        let mut result = Vec::new();
        for offset in 0..limit {
            let fields = &rows[(self.identities.cursor + offset) % rows.len()];
            result.push((
                guest.address,
                format!("IDENT|1|{}|{}", guest.lease, fields.join("|")).into_bytes(),
            ));
        }
        self.identities.cursor = (self.identities.cursor + result.len()) % rows.len();
        result
    }

    /// Releases universe-layer state when peers expire.
    pub(super) fn expire_universe(&mut self, now: f64) {
        self.expire_dock(now);
    }
}

/// Builds a cargo reply packet.
fn cargo_reply(lease: &str, request: &str, status: &str, fence: i64) -> Vec<u8> {
    format!("CARGOREPLY|1|{lease}|{request}|{status}|{fence}").into_bytes()
}

/// Validates a display code: 1..=32 characters of `[A-Z0-9-]`.
fn is_display_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

/// Validates a display name: 1..=160 bytes of lowercase hex.
fn is_display_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 320
        && value.len() % 2 == 0
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decodes a hex string into UTF-8.
fn decode_hex_utf8(value: &str) -> Option<String> {
    if value.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let chars: Vec<char> = value.chars().collect();
    for pair in chars.chunks(2) {
        let high = (pair[0] as u8 as char).to_digit(16)?;
        let low = (pair[1] as u8 as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    String::from_utf8(bytes).ok()
}
