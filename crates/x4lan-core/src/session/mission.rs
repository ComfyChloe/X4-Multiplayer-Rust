//! Host-native faction mission journal.
//!
//! The journal never awards money or invents progress. Complete, checksummed
//! records survive packet loss and late joins. Identifiers are scoped to the
//! host lease, and terminal records are retained rather than evicted or
//! reused.

use super::Session;
use crate::ordered::OrderedMap;
use crate::protocol::{sequence, PeerAddress, MAX_PACKET};

/// Largest number of retained mission records.
const LIMIT: i64 = 256;
/// Largest size of one transfer part.
const PART_SIZE: usize = 500;
/// Largest number of parts in one transfer.
const MAX_PARTS: i64 = 24;
/// Terminal mission states.
const TERMINAL: [&str; 4] = ["C", "F", "X", "W"];
/// Which states may follow which.
const TRANSITIONS: [(&str, &[&str]); 2] = [
    ("O", &["O", "A", "C", "F", "X", "W"]),
    ("A", &["A", "C", "F", "X"]),
];

/// Parses a hex-encoded UTF-8 text field.
fn text_field(value: &str, maximum: usize) -> Option<String> {
    if value.is_empty()
        || value.len() > maximum * 2
        || value.len() % 2 != 0
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return None;
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let characters: Vec<char> = value.chars().collect();
    for pair in characters.chunks(2) {
        let high = (pair[0] as u8 as char).to_digit(16)?;
        let low = (pair[1] as u8 as char).to_digit(16)?;
        bytes.push((high * 16 + low) as u8);
    }
    let text = String::from_utf8(bytes).ok()?;
    if text
        .chars()
        .any(|c| (c as u32) < 32 && c != '\n' && c != '\t')
        || text.contains('\u{7f}')
    {
        return None;
    }
    Some(text)
}

/// Parses one mission record.
pub fn parse_record(payload: &str) -> Option<super::coop::MissionRecord> {
    let fields: Vec<&str> = payload.split(',').collect();
    if fields.len() != 9 || fields[0] != "quota" {
        return None;
    }
    let state = fields[1];
    let known = state == "O" || state == "A" || TERMINAL.contains(&state);
    if !known {
        return None;
    }
    let done = sequence(fields[2])?;
    let total = sequence(fields[3])?;
    let reward = sequence(fields[4])?;
    if done > total || total > 1_000_000 || total == 0 {
        return None;
    }
    let faction = fields[5];
    if faction.is_empty()
        || faction.len() > 64
        || !faction
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || !faction.starts_with(|c: char| c.is_ascii_lowercase())
    {
        return None;
    }
    if state == "O" && done != 0 {
        return None;
    }
    if state == "C" && done != total {
        return None;
    }
    Some(super::coop::MissionRecord {
        kind: fields[0].to_string(),
        state: state.to_string(),
        done,
        total,
        reward,
        faction: faction.to_string(),
        title: text_field(fields[6], 256)?,
        description: text_field(fields[7], 4096)?,
        objective: text_field(fields[8], 512)?,
    })
}

/// Whether a new record may replace an old one.
pub fn advances(
    old: Option<&super::coop::MissionRecord>,
    new: &super::coop::MissionRecord,
) -> bool {
    let Some(old) = old else {
        // A terminal fact may be the first retained record after joining.
        return true;
    };
    let allowed = TRANSITIONS
        .iter()
        .find(|(from, _)| *from == old.state)
        .map(|(_, to)| to.contains(&new.state.as_str()))
        .unwrap_or(false);
    allowed
        && new.done >= old.done
        && new.kind == old.kind
        && new.total == old.total
        && new.reward == old.reward
        && new.faction == old.faction
        && new.title == old.title
        && new.description == old.description
        && new.objective == old.objective
}

impl Session {
    /// Handles a mission packet.
    pub(super) fn receive_mission(
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
        if fields.len() < 7 || fields[1] != "1" {
            return Vec::new();
        }
        let host_lease = self.peers.get("p1").map(|peer| peer.lease.clone());
        let Some(host) = self.missions.current_host(host_lease.as_deref()) else {
            return Vec::new();
        };
        let Some(slot) = self.peer_slot(fields[2], address) else {
            return Vec::new();
        };
        let peer_lease = self
            .peers
            .get(&slot)
            .map(|p| p.lease.clone())
            .unwrap_or_default();
        if fields[3] != host {
            return Vec::new();
        }
        let (Some(identifier), Some(revision)) = (sequence(fields[4]), sequence(fields[5])) else {
            return Vec::new();
        };
        if !(1..=LIMIT).contains(&identifier) || revision == 0 {
            return Vec::new();
        }
        let old = self.missions.records.get(&identifier).cloned();

        if fields[0] == "MREQ" && fields.len() == 7 && slot == "p2" {
            if let Some(old) = old {
                if old.revision == revision
                    && ((old.row.state == "O" && fields[6] == "ACCEPT")
                        || (old.row.state == "A" && fields[6] == "ABORT"))
                {
                    self.missions
                        .requests
                        .insert(identifier, (peer_lease, revision, fields[6].to_string()));
                }
            }
            return Vec::new();
        }

        if fields[0] == "MSTATE" && fields.len() == 10 && slot == "p1" {
            if let Some(old) = &old {
                if revision <= old.revision {
                    return Vec::new();
                }
            }
            let (Ok(part), Ok(total), Ok(digest)) = (
                fields[6].parse::<i64>(),
                fields[7].parse::<i64>(),
                fields[8].parse::<i64>(),
            ) else {
                return Vec::new();
            };
            if !(1..=total).contains(&part)
                || total > MAX_PARTS
                || !(0..=u32::MAX as i64).contains(&digest)
                || fields[9].is_empty()
                || fields[9].len() > PART_SIZE
            {
                return Vec::new();
            }
            let digest = digest as u32;
            if !fields[9]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b',')
            {
                return Vec::new();
            }
            let pending = self.missions.pending.get(&identifier).cloned();
            if let Some(pending) = &pending {
                if revision < pending.revision {
                    return Vec::new();
                }
            }
            let mut pending = match pending {
                Some(pending) if pending.revision == revision && now - pending.at < 10.0 => pending,
                _ => super::coop::MissionPending {
                    revision,
                    total,
                    digest,
                    parts: OrderedMap::new(),
                    at: now,
                },
            };
            if pending.total != total || pending.digest != digest {
                return Vec::new();
            }
            if let Some(existing) = pending.parts.get(&part) {
                if existing != fields[9] {
                    self.missions.pending.remove(&identifier);
                    return Vec::new();
                }
            } else {
                pending.parts.insert(part, fields[9].to_string());
            }
            if pending.parts.len() as i64 != total {
                self.missions.pending.insert(identifier, pending);
                return Vec::new();
            }
            let mut payload = String::new();
            for index in 1..=total {
                match pending.parts.get(&index) {
                    Some(part) => payload.push_str(part),
                    None => return Vec::new(),
                }
            }
            if super::universe::adler32(payload.as_bytes()) != digest {
                self.missions.pending.remove(&identifier);
                return Vec::new();
            }
            let Some(row) = parse_record(&payload) else {
                self.missions.pending.remove(&identifier);
                return Vec::new();
            };
            if !advances(old.as_ref().map(|entry| &entry.row), &row) {
                return Vec::new();
            }
            self.missions.records.insert(
                identifier,
                super::coop::MissionEntry {
                    revision,
                    row,
                    payload,
                    digest,
                },
            );
            self.missions.pending.remove(&identifier);
            if !self.missions.changed.contains(&identifier) {
                self.missions.changed.push(identifier);
            }
            self.missions.requests.remove(&identifier);
        }
        Vec::new()
    }

    /// Produces mission snapshots.
    pub(super) fn snapshots_mission(&mut self, now: f64) -> Vec<(PeerAddress, Vec<u8>)> {
        let host_lease = self.peers.get("p1").map(|peer| peer.lease.clone());
        let Some(host) = self.missions.current_host(host_lease.as_deref()) else {
            return Vec::new();
        };
        let Some(host_peer) = self.peers.get("p1").cloned() else {
            return Vec::new();
        };
        if now < self.missions.next_send {
            return Vec::new();
        }
        self.missions.next_send = now + 0.25;
        let Some(guest) = self.peers.get("p2").cloned() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut ids: Vec<i64> = self.missions.records.keys().copied().collect();
        ids.sort_unstable();
        if !ids.is_empty() {
            self.missions.cursor %= ids.len();
            let identifier = if !self.missions.changed.is_empty() {
                self.missions.changed.remove(0)
            } else {
                ids[self.missions.cursor]
            };
            self.missions.cursor += 1;
            let Some(entry) = self.missions.records.get(&identifier).cloned() else {
                return result;
            };
            let parts: Vec<&str> = entry
                .payload
                .as_bytes()
                .chunks(PART_SIZE)
                .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
                .collect();
            let total = parts.len();
            for (index, part) in parts.into_iter().enumerate() {
                result.push((
                    guest.address,
                    format!(
                        "MSTATE|1|{}|{}|{identifier}|{}|{}|{total}|{}|{part}",
                        guest.lease,
                        host,
                        entry.revision,
                        index + 1,
                        entry.digest
                    )
                    .into_bytes(),
                ));
            }
        }
        let requests: Vec<(i64, (String, i64, String))> = self
            .missions
            .requests
            .iter()
            .map(|(key, value)| (*key, value.clone()))
            .collect();
        for (identifier, (lease, revision, action)) in requests {
            if guest.lease != lease {
                self.missions.requests.remove(&identifier);
            } else {
                result.push((
                    host_peer.address,
                    format!("MREQ|1|{host}|{host}|{identifier}|{revision}|{action}").into_bytes(),
                ));
            }
        }
        result
    }
}
