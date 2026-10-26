//! Host-owned NPC health and cumulative, acknowledged guest hit claims.
//!
//! Port of `x4lan/npc_combat.py`. No HP deltas are inferred from replica
//! motion or disappearance: native X4 owns damage, regeneration and death.
//! The relay only fences identity and retries.

use super::Session;
use crate::format::py_float;
use crate::ordered::OrderedMap;
use crate::protocol::{is_atom, sequence, PeerAddress, MAX_PACKET};

/// How long an NPC health observation stays fresh.
const FRESH: f64 = 5.0;
/// The ledger refuses further admission rather than evicting an entry.
const LIMIT: usize = 32768;

/// A known NPC: where it is and what it is.
#[derive(Debug, Clone)]
pub struct Descriptor {
    pub sector: String,
    pub macro_name: String,
    pub owner: String,
    pub active: bool,
}

/// The host's latest observation of an NPC.
#[derive(Debug, Clone)]
pub struct NpcState {
    pub generation: i64,
    pub sequence: i64,
    /// The full identity: sector, code, macro and owner.
    pub identity: (String, String, String, String),
    /// The descriptor the identity must match: sector, macro and owner.
    pub descriptor: (String, String, String),
    pub alive: bool,
    pub at: f64,
    pub fields: Vec<String>,
}

/// A guest's claim that it hit an NPC.
#[derive(Debug, Clone)]
pub struct NpcClaim {
    pub sequence: i64,
    pub hits: i64,
    pub digest: String,
    pub fields: Vec<String>,
    pub ack: i64,
}

/// The NPC combat ledger.
#[derive(Debug, Clone)]
pub struct NpcCombat {
    pub descriptors: OrderedMap<i64, Descriptor>,
    pub states: OrderedMap<i64, NpcState>,
    pub claims: OrderedMap<(String, i64, i64, i64, i64), NpcClaim>,
    pub cursor: usize,
    pub next_send: f64,
}

impl Default for NpcCombat {
    fn default() -> Self {
        Self::new()
    }
}

impl NpcCombat {
    pub fn new() -> Self {
        Self {
            descriptors: OrderedMap::new(),
            states: OrderedMap::new(),
            claims: OrderedMap::new(),
            cursor: 0,
            next_send: 0.0,
        }
    }

    /// Folds a committed baseline into the known NPCs.
    ///
    /// Absence suspends admission, it is not a destruction event.
    pub fn committed(&mut self, baseline: &super::universe::Baseline) {
        for descriptor in self.descriptors.values_mut() {
            if descriptor.sector == baseline.sector {
                descriptor.active = false;
            }
        }
        for row in &baseline.rows {
            let fields: Vec<&str> = row.split(',').collect();
            if fields.len() < 4 || fields[1] != "ship" {
                continue;
            }
            let Ok(entity) = fields[0].parse::<i64>() else {
                continue;
            };
            if !self.descriptors.contains_key(&entity) && self.descriptors.len() >= LIMIT {
                continue;
            }
            self.descriptors.insert(
                entity,
                Descriptor {
                    sector: baseline.sector.clone(),
                    macro_name: fields[2].to_string(),
                    owner: fields[3].to_string(),
                    active: true,
                },
            );
        }
    }

    /// Whether an NPC's state is current and usable.
    fn current(&self, entity: i64, generation: i64, now: f64, live: bool) -> bool {
        let (Some(state), Some(descriptor)) =
            (self.states.get(&entity), self.descriptors.get(&entity))
        else {
            return false;
        };
        state.generation == generation
            && now - state.at < FRESH
            && state.descriptor
                == (
                    descriptor.sector.clone(),
                    descriptor.macro_name.clone(),
                    descriptor.owner.clone(),
                )
            && (!live || (descriptor.active && state.alive))
    }
}

impl Session {
    /// Whether a guest may claim hits against an NPC as a co-op pilot.
    fn npc_pilot(&mut self, entity: i64, generation: i64) -> bool {
        if entity != 1 && entity != 2 {
            return false;
        }
        let owner = self.owners.get(&entity).cloned().unwrap_or_default();
        let current_generation = self.health_generation(entity);
        let live = self.health_live.get(&entity).copied().unwrap_or(0);
        owner == "p2"
            && self.pilot(entity) == "p2"
            && generation == current_generation
            && current_generation == live
            && !self.dead(entity)
    }

    /// Handles an NPC combat packet.
    pub(super) fn receive_npc_combat(
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
        let host = self.peers.get("p1").cloned();
        let Some(host) = host else {
            return Vec::new();
        };
        if fields[3] != host.lease || self.universe_epoch.as_deref() != Some(host.lease.as_str()) {
            return Vec::new();
        }

        if fields[0] == "NHP" && slot == "p1" && fields.len() == 14 {
            let (Some(entity), Some(generation), Some(seq)) = (
                sequence(fields[4]),
                sequence(fields[5]),
                sequence(fields[6]),
            ) else {
                return Vec::new();
            };
            let (sector, code, macro_name, owner) = (fields[7], fields[8], fields[9], fields[10]);
            let (Some(hull), Some(shield)) = (py_float(fields[11]), py_float(fields[12])) else {
                return Vec::new();
            };
            let Some(descriptor) = self.npc_combat.descriptors.get(&entity).cloned() else {
                return Vec::new();
            };
            if entity == 0
                || generation == 0
                || seq == 0
                || owner == "player"
                || !is_display_code(code)
                || !is_atom(macro_name)
                || (sector, macro_name, owner)
                    != (
                        descriptor.sector.as_str(),
                        descriptor.macro_name.as_str(),
                        descriptor.owner.as_str(),
                    )
                || !(hull.is_finite() && shield.is_finite())
                || !(0.0..=100.0).contains(&hull)
                || !(0.0..=100.0).contains(&shield)
                || (fields[13] != "0" && fields[13] != "1")
                || (fields[13] == "0" && hull != 0.0)
                || (fields[13] == "1" && (hull <= 0.0 || !descriptor.active))
            {
                return Vec::new();
            }
            let identity = (
                sector.to_string(),
                code.to_string(),
                macro_name.to_string(),
                owner.to_string(),
            );
            let old = self.npc_combat.states.get(&entity).cloned();
            if let Some(old) = &old {
                if seq <= old.sequence
                    || generation != old.generation
                    || identity != old.identity
                    || (!old.alive && fields[13] == "1")
                {
                    return Vec::new();
                }
            }
            self.npc_combat.states.insert(
                entity,
                NpcState {
                    generation,
                    sequence: seq,
                    identity,
                    descriptor: (
                        descriptor.sector.clone(),
                        descriptor.macro_name.clone(),
                        descriptor.owner.clone(),
                    ),
                    alive: fields[13] == "1",
                    at: now,
                    fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                },
            );
        } else if fields[0] == "NCLAIM" && slot == "p2" && fields.len() == 11 {
            let (
                Some(entity),
                Some(generation),
                Some(shooter),
                Some(shooter_generation),
                Some(seq),
                Some(hits),
            ) = (
                sequence(fields[4]),
                sequence(fields[5]),
                sequence(fields[6]),
                sequence(fields[7]),
                sequence(fields[8]),
                sequence(fields[9]),
            )
            else {
                return Vec::new();
            };
            if seq == 0 || hits == 0 || hits > 1_000_000 {
                return Vec::new();
            }
            if !self.npc_combat.current(entity, generation, now, true) {
                return Vec::new();
            }
            if !self.npc_pilot(shooter, shooter_generation) {
                return Vec::new();
            }
            if !super::coop::is_shot_digest(fields[10]) {
                return Vec::new();
            }
            let Some((interest_lease, _, interest_sector)) = self.interest.clone() else {
                return Vec::new();
            };
            if interest_lease != peer_lease {
                return Vec::new();
            }
            let Some(state) = self.npc_combat.states.get(&entity) else {
                return Vec::new();
            };
            if interest_sector != state.descriptor.0 {
                return Vec::new();
            }
            let key = (
                peer_lease.clone(),
                entity,
                generation,
                shooter,
                shooter_generation,
            );
            let old = self.npc_combat.claims.get(&key).cloned();
            if let Some(old) = &old {
                if seq <= old.sequence || hits < old.hits || fields[10] != old.digest {
                    return Vec::new();
                }
            } else if self.npc_combat.claims.len() >= LIMIT {
                return Vec::new();
            }
            self.npc_combat.claims.insert(
                key,
                NpcClaim {
                    sequence: seq,
                    hits,
                    digest: fields[10].to_string(),
                    fields: fields[3..].iter().map(|s| s.to_string()).collect(),
                    ack: old.map(|claim| claim.ack).unwrap_or(0),
                },
            );
        } else if fields[0] == "NACK" && slot == "p1" && fields.len() == 10 {
            let (
                Some(entity),
                Some(generation),
                Some(shooter),
                Some(shooter_generation),
                Some(count),
            ) = (
                sequence(fields[5]),
                sequence(fields[6]),
                sequence(fields[7]),
                sequence(fields[8]),
                sequence(fields[9]),
            )
            else {
                return Vec::new();
            };
            let key = (
                fields[4].to_string(),
                entity,
                generation,
                shooter,
                shooter_generation,
            );
            if let Some(claim) = self.npc_combat.claims.get_mut(&key) {
                if claim.ack <= count && count <= claim.hits {
                    claim.ack = count;
                }
            }
        }
        Vec::new()
    }

    /// Produces NPC combat snapshots.
    pub(super) fn snapshots_npc_combat(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        if now < self.npc_combat.next_send {
            return Vec::new();
        }
        self.npc_combat.next_send = now + 0.1;
        let host = self.peers.get("p1").cloned();
        let guest = self.peers.get("p2").cloned();
        let (Some(host), Some(guest)) = (host, guest) else {
            return Vec::new();
        };
        if self.universe_epoch.as_deref() != Some(host.lease.as_str()) {
            return Vec::new();
        }
        let interest = self.interest.clone();
        let mut result = Vec::new();

        let states: Vec<NpcState> = self
            .npc_combat
            .states
            .values()
            .filter(|state| {
                now - state.at < FRESH
                    && interest.is_some()
                    && interest.as_ref().unwrap().0 == guest.lease
                    && interest.as_ref().unwrap().2 == state.descriptor.0
            })
            .cloned()
            .collect();
        // The modulo is applied before indexing, including after a fleet shrinks.
        let budget = states.len().min(32.max(256.min(states.len().div_ceil(10))));
        for offset in 0..budget {
            let state = &states[(self.npc_combat.cursor + offset) % states.len()];
            result.push((
                guest.address,
                format!("NHP|1|{}|{}", guest.lease, state.fields.join("|")).into_bytes(),
            ));
        }
        self.npc_combat.cursor += budget;

        let keys: Vec<(String, i64, i64, i64, i64)> = self
            .npc_combat
            .claims
            .iter()
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            let (lease, entity, generation, shooter, shooter_generation) = key.clone();
            let Some(claim) = self.npc_combat.claims.get(&key).cloned() else {
                continue;
            };
            if lease != guest.lease || !self.npc_pilot(shooter, shooter_generation) {
                // The closed pilot generation can never be re-admitted.
                self.npc_combat.claims.remove(&key);
                continue;
            }
            if !self.npc_combat.current(entity, generation, now, true) {
                continue;
            }
            result.push((
                guest.address,
                format!(
                    "NACK|1|{}|{}|{lease}|{entity}|{generation}|{shooter}|{shooter_generation}|{}",
                    guest.lease, host.lease, claim.ack
                )
                .into_bytes(),
            ));
            if claim.ack < claim.hits {
                result.push((
                    host.address,
                    format!("NCLAIM|1|{}|{lease}|{}", host.lease, claim.fields.join("|"))
                        .into_bytes(),
                ));
            }
        }
        result
    }
}

/// Validates a display code: 1..=32 characters of `[A-Z0-9-]`.
pub fn is_display_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}
