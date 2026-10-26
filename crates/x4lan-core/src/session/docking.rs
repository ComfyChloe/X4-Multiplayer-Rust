//! Co-op saved-dock observations and two-player reservation arbitration.
//!
//! Port of `x4lan/docking.py`. Native handles never cross the wire. A dock
//! index belongs to the session's shared pristine save, and the recipient
//! must still validate the actual native bay.

use super::{DockClaim, Session};
use crate::protocol::{sequence, PeerAddress, MAX_PACKET};

/// Largest dock index.
const MAX_DOCK: i64 = 250_000;
/// How long a dock observation stays fresh.
const FRESH: f64 = 0.75;
/// Dock states that hold a claim.
const ATTACHED: [&str; 3] = ["APPROACH", "EXTERNAL", "STORED"];

impl Session {
    /// Handles a packet in the docking layer.
    pub(super) fn receive_dock(
        &mut self,
        raw: &[u8],
        address: PeerAddress,
        now: f64,
    ) -> Vec<Vec<u8>> {
        if !raw.starts_with(b"CDOCK|") {
            return self.receive_coop(raw, address, now);
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
        if fields.len() != 8 || fields[1] != "1" {
            return Vec::new();
        }
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let peer_lease = self.peers.get(&slot).map(|p| p.lease.clone()).unwrap_or_default();
        let (Some(entity), Some(epoch), Some(seq)) = (
            super::economy::integer(fields[3], 2, false),
            sequence(fields[4]),
            sequence(fields[5]),
        ) else {
            return Vec::new();
        };
        let mode = fields[6];
        let Some(dock) = super::economy::integer(fields[7], MAX_DOCK, false) else {
            return Vec::new();
        };
        if entity != 1 && entity != 2 {
            return Vec::new();
        }
        if self.owners.get(&entity).map(String::as_str) != Some(slot.as_str()) {
            return Vec::new();
        }
        if self.epochs.get(&entity).copied() != Some(epoch) {
            return Vec::new();
        }
        if self.dead(entity) {
            return Vec::new();
        }
        let attached = ATTACHED.contains(&mode) && dock > 0;
        let released = (mode == "FREE" || mode == "UNKNOWN") && dock == 0;
        if !attached && !released {
            return Vec::new();
        }
        let old = self.coop_docks.get(&entity).cloned();
        let seen = self.coop_dock_seen.get(&entity).cloned();
        let key = (slot.to_string(), peer_lease.clone(), epoch);
        if let Some((seen_key, seen_seq)) = seen {
            if seen_key == key && seq <= seen_seq {
                return Vec::new();
            }
        }
        // A denied newer request must also fence older FREE/attachment
        // packets. The last accepted claim is kept until an explicit newer
        // state releases or replaces it; denial cannot refresh its freshness.
        self.coop_dock_seen.insert(entity, (key, seq));
        if dock != 0 {
            let conflict = self.coop_docks.iter().any(|(other, row)| {
                *other != entity
                    && self.current_dock(*other, row)
                    && row.claim == dock
            });
            if conflict {
                return vec![format!(
                    "CDOCK_DENIED|1|{peer_lease}|{entity}|{epoch}|{seq}|{dock}"
                )
                .into_bytes()];
            }
        }
        let mut claim = dock;
        if mode == "UNKNOWN" {
            if let Some(old) = old {
                if self.current_dock(entity, &old) {
                    claim = old.claim;
                }
            }
        }
        self.coop_docks.insert(
            entity,
            DockClaim {
                owner: slot.to_string(),
                lease: peer_lease,
                epoch,
                seq,
                mode: mode.to_string(),
                dock,
                claim,
                at: now,
            },
        );
        Vec::new()
    }

    /// Whether a dock claim is still the current one for a ship.
    fn current_dock(&self, entity: i64, row: &DockClaim) -> bool {
        row.owner == self.owners.get(&entity).cloned().unwrap_or_default()
            && row.epoch == self.epochs.get(&entity).copied().unwrap_or(0)
            && !self.dead(entity)
    }

    /// Produces docking-layer snapshots.
    pub(super) fn snapshots_dock(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let mut result = self.snapshots_coop(now);
        let slots: Vec<String> = self.peers.keys().cloned().collect();
        for slot in slots {
            let Some(peer) = self.peers.get(&slot).cloned() else {
                continue;
            };
            let claims: Vec<(i64, DockClaim)> = self
                .coop_docks
                .iter()
                .map(|(entity, row)| (*entity, row.clone()))
                .collect();
            for (entity, row) in claims {
                let owner = self.peers.get(&row.owner).cloned();
                if row.owner == slot
                    || !self.current_dock(entity, &row)
                    || owner.is_none()
                    || owner.as_ref().map(|p| p.lease.clone()) != Some(row.lease.clone())
                    || now - row.at > FRESH
                {
                    continue;
                }
                result.push((
                    peer.address,
                    format!(
                        "CDOCK|1|{}|{}|{entity}|{}|{}|{}|{}",
                        peer.lease, row.lease, row.epoch, row.seq, row.mode, row.dock
                    )
                    .into_bytes(),
                ));
            }
        }
        result
    }

    /// Releases docking-layer state when peers expire.
    pub(super) fn expire_dock(&mut self, now: f64) {
        self.expire_coop(now);
    }
}
