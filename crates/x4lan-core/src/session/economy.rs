//! Bounded authoritative market snapshots and idempotent trading requests.
//!
//! Port of `x4lan/economy.py`. Only the host's X4 can publish inventory or
//! balances, and only it can complete a transaction. The relay validates and
//! routes; it never fabricates a trade's result.

use super::{Order, Session};
use crate::protocol::{is_lease, is_ware, sequence, PeerAddress, MAX_PACKET};

/// Largest trade quantity.
pub const MAX_AMOUNT: i64 = 1_000_000;
/// Largest money value.
pub const MAX_MONEY: i64 = 1_000_000_000_000;

/// Terminal order results.
const RESULTS: [&str; 11] = [
    "OK",
    "REJECTED",
    "STOCK",
    "FUNDS",
    "CAPACITY",
    "PRICE",
    "DOCK",
    "BUSY",
    "PARTIAL",
    "UNCERTAIN",
    "UNSUPPORTED",
];

/// A pending part: the revision, the time it arrived, the expected part count
/// and the parts seen so far.
type PendingPart = (i64, (f64, i64, Vec<Option<String>>));

/// Reassembles a market snapshot that arrives in numbered parts.
#[derive(Debug, Clone, Default)]
pub struct Assembly {
    pending: Vec<PendingPart>,
    revision: i64,
}

impl Assembly {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            revision: -1,
        }
    }

    /// Adds one part and returns the complete payload once every part is in.
    pub fn add(
        &mut self,
        revision: i64,
        part: i64,
        total: i64,
        payload: &str,
        now: f64,
    ) -> Option<String> {
        if revision <= self.revision || !(1..=8).contains(&total) || !(1..=total).contains(&part) {
            return None;
        }
        if payload.len() > 500 {
            return None;
        }
        // Drop parts that are stale or superseded.
        self.pending
            .retain(|(rev, (at, _, _))| now - *at < 3.0 && *rev > self.revision);
        if !self.pending.iter().any(|(rev, _)| *rev == revision) {
            if self.pending.len() >= 4 {
                self.pending.remove(0);
            }
            self.pending
                .push((revision, (now, total, vec![None; total as usize])));
        }
        let value = {
            let (_, entry) = self.pending.iter_mut().find(|(rev, _)| *rev == revision)?;
            let expected = entry.1;
            let parts = &mut entry.2;
            if expected != total {
                return None;
            }
            let index = (part - 1) as usize;
            if let Some(existing) = parts[index].as_ref() {
                if existing != payload {
                    return None;
                }
            } else {
                parts[index] = Some(payload.to_string());
            }
            if parts.iter().any(|part| part.is_none()) {
                return None;
            }
            let mut value = String::new();
            for part in parts.iter() {
                value.push_str(part.as_ref().expect("every part present"));
            }
            value
        };
        parse_snapshot(&value)?;
        self.revision = revision;
        self.pending.retain(|(rev, _)| *rev > revision);
        Some(value)
    }
}

/// A ware and its quantities: volume, the two players' holdings, the two
/// markets' stock, and the bid, ask and limit prices.
type WareTable = Vec<(String, Vec<i64>)>;

/// Parses a market snapshot payload into balances and ware rows.
pub fn parse_snapshot(payload: &str) -> Option<(Vec<i64>, WareTable)> {
    let rows: Vec<&str> = payload.split(';').collect();
    let funds: Vec<&str> = rows[0].split(',').collect();
    if funds.len() != 5 || funds[0] != "M" {
        return None;
    }
    let mut balances = Vec::with_capacity(4);
    for value in &funds[1..] {
        balances.push(integer(value, MAX_MONEY, false)?);
    }
    if !(2..=17).contains(&rows.len()) {
        return None;
    }
    let mut wares: Vec<(String, Vec<i64>)> = Vec::new();
    for row in &rows[1..] {
        let fields: Vec<&str> = row.split(',').collect();
        if fields.len() != 13 || fields[0] != "W" || !is_ware(fields[1]) {
            return None;
        }
        if wares.iter().any(|(name, _)| name == fields[1]) {
            return None;
        }
        let mut values = Vec::with_capacity(11);
        for field in &fields[2..] {
            values.push(integer(field, MAX_MONEY, false)?);
        }
        if !(1..=100_000).contains(&values[0]) {
            return None;
        }
        for index in [1, 2, 3, 4, 7, 10] {
            if values[index] > MAX_AMOUNT {
                return None;
            }
        }
        for index in [5, 6, 8, 9] {
            if values[index] < 1 {
                return None;
            }
        }
        wares.push((fields[1].to_string(), values));
    }
    Some((balances, wares))
}

/// Parses a trade basket.
pub fn parse_lines(payload: &str) -> Option<Vec<(String, i64, i64)>> {
    let rows: Vec<&str> = payload.split(';').collect();
    if rows.is_empty() || rows.len() > 16 {
        return None;
    }
    let mut result = Vec::with_capacity(rows.len());
    let mut seen: Vec<&str> = Vec::new();
    for row in rows {
        let fields: Vec<&str> = row.split(',').collect();
        if fields.len() != 3 || !is_ware(fields[0]) || seen.contains(&fields[0]) {
            return None;
        }
        let quantity = integer(fields[1], MAX_AMOUNT, true)?;
        let price = integer(fields[2], MAX_MONEY, false)?;
        if quantity == 0 || price == 0 || quantity.abs().saturating_mul(price) > MAX_MONEY {
            return None;
        }
        seen.push(fields[0]);
        result.push((fields[0].to_string(), quantity, price));
    }
    Some(result)
}

/// Parses a bounded integer.
pub fn integer(value: &str, limit: i64, signed: bool) -> Option<i64> {
    crate::protocol::integer(value, limit, signed)
}

impl Session {
    /// Handles a packet in the economy layer.
    pub(super) fn receive_economy(
        &mut self,
        raw: &[u8],
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
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
            "ECON" | "ORDER" | "ORDER_DONE" | "DOCKSTATE" => {
                self.receive_economy_state(fields, address, now)
            }
            _ => self.receive_bubble(raw, address, now),
        }
    }

    fn receive_economy_state(
        &mut self,
        fields: Vec<&str>,
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let mut reply: Vec<Vec<u8>> = Vec::new();
        match fields[0] {
            "ECON" if fields.len() == 7 && slot == "p1" => {
                let (Some(revision), Some(part), Some(total)) = (
                    sequence(fields[3]),
                    integer(fields[4], 8, false),
                    integer(fields[5], 8, false),
                ) else {
                    return Vec::new();
                };
                let host_lease = self
                    .peers
                    .get("p1")
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if self.economy_owner.as_deref() != Some(host_lease.as_str()) {
                    self.economy_owner = Some(host_lease);
                    self.assembly = super::economy::Assembly::new();
                }
                if let Some(payload) = self.assembly.add(revision, part, total, fields[6], now) {
                    self.economy = Some(payload);
                }
            }
            "ORDER" if fields.len() == 7 => {
                let (request, owner, market, lines) = (fields[3], fields[4], fields[5], fields[6]);
                if !is_lease(request) || !is_lease(owner) || (market != "m1" && market != "m2") {
                    return Vec::new();
                }
                if parse_lines(lines).is_none() {
                    return Vec::new();
                }
                let peer_lease = self
                    .peers
                    .get(&slot)
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if let Some(old) = self.orders.get(request).cloned() {
                    if old.slot != slot
                        || old.owner != owner
                        || old.market != market
                        || old.lines != lines
                    {
                        return Vec::new();
                    }
                    reply.push(self.order_reply(&peer_lease, request, &old.result));
                    return reply;
                }
                let host = self.peers.get("p1").cloned();
                let host_lease = host.as_ref().map(|p| p.lease.clone()).unwrap_or_default();
                if host.is_none() || owner != host_lease || self.economy.is_none() {
                    reply.push(self.order_reply(&peer_lease, request, "UNCERTAIN"));
                    return reply;
                }
                let pending = self
                    .orders
                    .values()
                    .filter(|order| order.result == "PENDING")
                    .count();
                if self.orders.len() >= 1024 || pending >= 8 {
                    reply.push(self.order_reply(&peer_lease, request, "BUSY"));
                    return reply;
                }
                self.orders.insert(
                    request.to_string(),
                    Order {
                        slot: slot.clone(),
                        lease: peer_lease.clone(),
                        owner: owner.to_string(),
                        market: market.to_string(),
                        lines: lines.to_string(),
                        result: "PENDING".to_string(),
                    },
                );
                reply.push(self.order_reply(&peer_lease, request, "PENDING"));
            }
            "ORDER_DONE" if fields.len() == 5 && slot == "p1" && RESULTS.contains(&fields[4]) => {
                if let Some(order) = self.orders.get_mut(fields[3]) {
                    if order.result == "PENDING" {
                        let peer_lease = self
                            .peers
                            .get("p1")
                            .map(|p| p.lease.clone())
                            .unwrap_or_default();
                        if order.owner == peer_lease {
                            order.result = fields[4].to_string();
                        }
                    }
                }
            }
            "DOCKSTATE" if fields.len() == 5 && ["none", "m1", "m2"].contains(&fields[4]) => {
                let Some(seq) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let lease = self
                    .peers
                    .get(&slot)
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                let old = self
                    .docks
                    .get(&lease)
                    .map(|value| (value.0, value.1.clone()))
                    .unwrap_or((-1, String::new()));
                if seq > old.0 {
                    self.docks.insert(lease, (seq, fields[4].to_string()));
                }
            }
            _ => {}
        }
        reply
    }

    /// Builds an order reply packet.
    pub(super) fn order_reply(&self, lease: &str, request: &str, status: &str) -> Vec<u8> {
        format!("ORDER_RESULT|1|{lease}|{request}|{status}").into_bytes()
    }

    /// Produces economy-layer snapshots.
    pub(super) fn snapshots_economy(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = self.snapshots_bubble(now);
        let Some(host) = self.peers.get("p1").cloned() else {
            return result;
        };
        let publish_economy = now >= self.next_economy;
        let publish_docks = now >= self.next_docks;
        if publish_economy {
            self.next_economy = now + 0.5;
        }
        if publish_docks {
            self.next_docks = now + 0.2;
        }
        let revision = self.assembly.revision;
        let economy = self.economy.clone();
        let slots: Vec<String> = self.peers.keys().cloned().collect();
        for slot in slots {
            let Some(peer) = self.peers.get(&slot).cloned() else {
                continue;
            };
            if publish_economy {
                if let Some(economy) = &economy {
                    let chunks: Vec<&str> = economy
                        .as_bytes()
                        .chunks(500)
                        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
                        .collect();
                    let total = chunks.len();
                    for (index, payload) in chunks.into_iter().enumerate() {
                        result.push((
                            peer.address,
                            format!(
                                "ECON|1|{}|{}|{revision}|{}|{total}|{payload}",
                                peer.lease,
                                host.lease,
                                index + 1
                            )
                            .into_bytes(),
                        ));
                    }
                }
            }
            let other_slot = if slot == "p1" { "p2" } else { "p1" };
            if publish_docks {
                if let Some(other) = self.peers.get(other_slot) {
                    if let Some((seq, market)) = self.docks.get(&other.lease) {
                        result.push((
                            peer.address,
                            format!("DOCKSTATE|1|{}|{}|{seq}|{market}", peer.lease, other.lease)
                                .into_bytes(),
                        ));
                    }
                }
            }
        }
        for (request, order) in self.orders.iter() {
            if order.result == "PENDING" {
                result.push((
                    host.address,
                    format!(
                        "ORDER_EXEC|1|{}|{request}|{}|{}|{}",
                        host.lease, order.slot, order.market, order.lines
                    )
                    .into_bytes(),
                ));
                break;
            }
        }
        result
    }

    /// Releases economy-layer state when peers expire.
    pub(super) fn expire_economy(&mut self, now: f64) {
        let host_left = self.peers.get("p1").is_some();
        self.expire_bubble(now);
        if host_left && !self.peers.contains_key("p1") {
            self.economy = None;
            self.economy_owner = None;
            self.assembly = super::economy::Assembly::new();
            for order in self.orders.values_mut() {
                if order.result == "PENDING" {
                    order.result = "UNCERTAIN".to_string();
                }
            }
        }
        let live: Vec<String> = self.peers.values().map(|peer| peer.lease.clone()).collect();
        self.docks.retain(|lease, _| live.contains(lease));
        for order in self.orders.values_mut() {
            if !live.contains(&order.lease) && order.result == "PENDING" {
                order.result = "UNCERTAIN".to_string();
            }
        }
    }
}
