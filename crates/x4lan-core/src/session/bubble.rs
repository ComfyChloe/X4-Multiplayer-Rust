//! The experimental test bubble: host-authoritative action routing.
//!
//! Port of `x4lan/bubble.py`. The relay never invents cargo or damage; only
//! the host's X4 may publish world state and finish commands.

use super::{Command, Session};
use crate::protocol::{constant_time_eq, is_lease, sequence, PeerAddress, MAX_PACKET};

/// Number of fields in a `WORLD` payload.
const WORLD_FIELDS: usize = 17;

/// Actions the bubble understands.
const VERBS: [&str; 2] = ["PICKUP", "TRADE"];

/// Terminal command results.
const RESULTS: [&str; 4] = ["OK", "REJECTED", "BUSY", "CANCELLED"];

impl Session {
    /// Handles a packet in the bubble layer, falling back to the base layer.
    pub(super) fn receive_bubble(
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
            "ACTION" if fields.len() == 7 => {
                let (token, slot, request, verb, target) =
                    (fields[2], fields[3], fields[4], fields[5], fields[6]);
                if !constant_time_eq(token.as_bytes(), self.token.as_bytes()) || !is_lease(request)
                {
                    return Vec::new();
                }
                if (slot != "p1" && slot != "p2")
                    || !VERBS.contains(&verb)
                    || target != "energycells"
                {
                    return Vec::new();
                }
                let peer = self.peers.get(slot).cloned();
                let old = self.commands.get(request).cloned();
                if let Some(old) = old {
                    if old.slot != slot || old.verb != verb || old.ip != address.ip_text() {
                        return Vec::new();
                    }
                    return vec![format!("RESULT|1|{request}|{}", old.result).into_bytes()];
                }
                let Some(peer) = peer else {
                    return vec![format!("RESULT|1|{request}|NO_PLAYER").into_bytes()];
                };
                if peer.address.ip_text() != address.ip_text() {
                    return vec![format!("RESULT|1|{request}|NO_PLAYER").into_bytes()];
                }
                if self.world.is_none() || !self.peers.contains_key("p1") {
                    return vec![format!("RESULT|1|{request}|NO_WORLD").into_bytes()];
                }
                let pending = self
                    .commands
                    .values()
                    .filter(|command| command.result == "PENDING")
                    .count();
                if pending >= 8 {
                    return vec![format!("RESULT|1|{request}|BUSY").into_bytes()];
                }
                if self.commands.len() >= 512 {
                    // Never forget an ID in this host world: reject further work
                    // at the bound rather than reusing an identifier.
                    return vec![format!("RESULT|1|{request}|SESSION_FULL").into_bytes()];
                }
                self.commands.insert(
                    request.to_string(),
                    Command {
                        slot: slot.to_string(),
                        lease: peer.lease.clone(),
                        ip: address.ip_text(),
                        verb: verb.to_string(),
                        result: "PENDING".to_string(),
                        created: now,
                        ship: None,
                    },
                );
                vec![format!("RESULT|1|{request}|PENDING").into_bytes()]
            }
            "WORLD" | "FIRE" | "DONE" | "BODY" | "SUITKILL" | "WALK" => {
                self.receive_bubble_state(fields, address, now)
            }
            _ => self.receive_base(raw, address, now),
        }
    }

    fn receive_bubble_state(
        &mut self,
        fields: Vec<&str>,
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        match fields[0] {
            "WALK" if fields.len() == 11 => {
                let (Some(seq), Some(context)) = (sequence(fields[3]), sequence(fields[4])) else {
                    return Vec::new();
                };
                if context > 4 {
                    return Vec::new();
                }
                let mut pose = [0.0f64; 6];
                for (index, field) in fields[5..].iter().enumerate() {
                    let Some(value) = crate::format::py_float(field) else {
                        return Vec::new();
                    };
                    pose[index] = value;
                }
                let limit = |index: usize| if index < 3 { 10_000.0 } else { 360.0 };
                if !pose
                    .iter()
                    .enumerate()
                    .all(|(index, value)| value.is_finite() && value.abs() <= limit(index))
                {
                    return Vec::new();
                }
                let lease = self
                    .peers
                    .get(&slot)
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if let Some(old) = self.walkers.get(&lease) {
                    if seq <= old.0 {
                        return Vec::new();
                    }
                }
                self.walkers.insert(
                    lease,
                    (
                        seq,
                        now,
                        context,
                        fields[5..].iter().map(|s| s.to_string()).collect(),
                    ),
                );
            }
            "BODY" if fields.len() == 13 => {
                let (Some(seq), Some(dead), Some(generation)) = (
                    sequence(fields[3]),
                    sequence(fields[5]),
                    sequence(fields[6]),
                ) else {
                    return Vec::new();
                };
                let mode = fields[4];
                if !["ship", "foot", "suit"].contains(&mode) || dead > 1 {
                    return Vec::new();
                }
                if mode == "suit" && generation == 0 {
                    return Vec::new();
                }
                let mut pose = [0.0f64; 6];
                for (index, field) in fields[7..].iter().enumerate() {
                    let Some(value) = crate::format::py_float(field) else {
                        return Vec::new();
                    };
                    pose[index] = value;
                }
                let limit = |index: usize| {
                    if index < 3 {
                        50_000.0
                    } else {
                        std::f64::consts::TAU
                    }
                };
                if !pose
                    .iter()
                    .enumerate()
                    .all(|(index, value)| value.is_finite() && value.abs() <= limit(index))
                {
                    return Vec::new();
                }
                let lease = self
                    .peers
                    .get(&slot)
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if let Some(old) = self.bodies.get(&lease) {
                    let old_dead: i64 = old[2].parse().unwrap_or(0);
                    let old_generation: i64 = old[3].parse().unwrap_or(0);
                    if seq <= old[0].parse().unwrap_or(i64::MIN)
                        || dead < old_dead
                        || generation < old_generation
                    {
                        return Vec::new();
                    }
                }
                let mut row = vec![
                    seq.to_string(),
                    mode.to_string(),
                    dead.to_string(),
                    generation.to_string(),
                ];
                row.extend(fields[7..].iter().map(|s| s.to_string()));
                self.bodies.insert(lease, row);
            }
            "SUITKILL" if fields.len() == 4 && slot == "p1" => {
                let Some(generation) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let Some(guest) = self.peers.get("p2").cloned() else {
                    return Vec::new();
                };
                let host_lease = self
                    .peers
                    .get("p1")
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if let Some(body) = self.bodies.get(&guest.lease) {
                    if body[1] == "suit" && body[3].parse::<i64>().unwrap_or(-1) == generation {
                        self.suit_kills
                            .insert(guest.lease.clone(), (host_lease, generation));
                    }
                }
            }
            "WORLD" if fields.len() == 4 + WORLD_FIELDS && slot == "p1" => {
                let Some(revision) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let mut values = Vec::with_capacity(WORLD_FIELDS);
                for field in &fields[4..] {
                    let Some(value) = crate::format::py_float(field) else {
                        return Vec::new();
                    };
                    values.push(value);
                }
                if !values
                    .iter()
                    .all(|v| v.is_finite() && (0.0..=1_000_000.0).contains(v))
                {
                    return Vec::new();
                }
                for index in [0, 1, 3, 4, 6, 7, 8, 9] {
                    if values[index] > 100.0 {
                        return Vec::new();
                    }
                }
                for index in [11, 12, 13, 14] {
                    if values[index] != 0.0 && values[index] != 1.0 {
                        return Vec::new();
                    }
                }
                for index in [2, 5, 10, 15, 16] {
                    if values[index] != values[index].trunc() {
                        return Vec::new();
                    }
                }
                let host_lease = self
                    .peers
                    .get("p1")
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                if self.world_owner.as_deref() != Some(host_lease.as_str()) {
                    self.world_revision = -1;
                    self.world_owner = Some(host_lease);
                }
                if revision <= self.world_revision {
                    return Vec::new();
                }
                self.world_revision = revision;
                self.world = Some(values);
            }
            "FIRE" if fields.len() == 4 => {
                let Some(seq) = sequence(fields[3]) else {
                    return Vec::new();
                };
                let lease = self
                    .peers
                    .get(&slot)
                    .map(|p| p.lease.clone())
                    .unwrap_or_default();
                let old = self.fire.get(&lease).copied().unwrap_or((-1, 0.0));
                if seq > old.0 {
                    self.fire.insert(lease, (seq, now));
                }
            }
            "DONE" if fields.len() == 5 && slot == "p1" && RESULTS.contains(&fields[4]) => {
                if let Some(command) = self.commands.get_mut(fields[3]) {
                    if command.result == "PENDING" {
                        command.result = fields[4].to_string();
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Produces bubble-layer snapshots.
    pub(super) fn snapshots_bubble(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = self.snapshots_base(now);
        let host = self.peers.get("p1").cloned();
        let slots: Vec<String> = self.peers.keys().cloned().collect();
        for slot in slots {
            let Some(peer) = self.peers.get(&slot).cloned() else {
                continue;
            };
            if let (Some(world), Some(host)) = (&self.world, &host) {
                let payload: Vec<String> =
                    world.iter().map(|v| crate::format::py_g(*v, 9)).collect();
                result.push((
                    peer.address,
                    format!(
                        "WORLD|1|{}|{}|{}|{}",
                        peer.lease,
                        host.lease,
                        self.world_revision,
                        payload.join("|")
                    )
                    .into_bytes(),
                ));
            }
            let other_slot = if slot == "p1" { "p2" } else { "p1" };
            let other = self.peers.get(other_slot).cloned();
            if let Some(other) = other {
                if let Some(walker) = self.walkers.get(&other.lease) {
                    if now - walker.1 < 1.0 {
                        let mut payload = vec![walker.0.to_string()];
                        payload.push(walker.2.to_string());
                        payload.extend(walker.3.iter().cloned());
                        result.push((
                            peer.address,
                            format!(
                                "WALK|1|{}|{}|{}",
                                peer.lease,
                                other.lease,
                                payload.join("|")
                            )
                            .into_bytes(),
                        ));
                    }
                }
                if let Some(body) = self.bodies.get(&other.lease) {
                    result.push((
                        peer.address,
                        format!("BODY|1|{}|{}|{}", peer.lease, other.lease, body.join("|"))
                            .into_bytes(),
                    ));
                }
                if let Some((seq, at)) = self.fire.get(&other.lease).copied() {
                    if now - at < 0.5 {
                        result.push((
                            peer.address,
                            format!("FIRE|1|{}|{}|{seq}", peer.lease, other.lease).into_bytes(),
                        ));
                    }
                }
            }
            let kill = self.suit_kills.get(&peer.lease).cloned();
            let body = self.bodies.get(&peer.lease).cloned();
            if let (Some((host_lease, generation)), Some(body), Some(host)) = (kill, body, &host) {
                if host_lease == host.lease
                    && body[1] == "suit"
                    && body[3].parse::<i64>().unwrap_or(-1) == generation
                {
                    result.push((
                        peer.address,
                        format!("SUITKILL|1|{}|{}|{generation}", peer.lease, host.lease)
                            .into_bytes(),
                    ));
                }
            }
        }
        if let Some(host) = host {
            // One transaction at a time. The receiver deduplicates before
            // dispatching to the mission director.
            for (request, command) in self.commands.iter() {
                if command.result == "PENDING" {
                    result.push((
                        host.address,
                        format!(
                            "COMMAND|1|{}|{request}|{}|{}",
                            host.lease, command.slot, command.verb
                        )
                        .into_bytes(),
                    ));
                    break;
                }
            }
        }
        result
    }

    /// Releases bubble-layer state when peers expire.
    pub(super) fn expire_bubble(&mut self, now: f64) {
        let host_left = self.peers.get("p1").is_some();
        self.expire_base(now);
        if host_left && !self.peers.contains_key("p1") {
            self.world = None;
            self.world_owner = None;
            self.world_revision = -1;
            for command in self.commands.values_mut() {
                if command.result == "PENDING" {
                    command.result = "CANCELLED".to_string();
                }
            }
        }
        let live: Vec<String> = self.peers.values().map(|peer| peer.lease.clone()).collect();
        let retained: Vec<String> = self
            .resumable
            .values()
            .map(|(peer, _)| peer.lease.clone())
            .chain(live.iter().cloned())
            .collect();
        self.bodies.retain(|lease, _| retained.contains(lease));
        self.suit_kills.retain(|lease, _| retained.contains(lease));
        self.fire.retain(|lease, _| live.contains(lease));
        self.walkers.retain(|lease, _| live.contains(lease));
        for command in self.commands.values_mut() {
            if !live.contains(&command.lease) && command.result == "PENDING" {
                command.result = "CANCELLED".to_string();
            }
        }
    }
}
