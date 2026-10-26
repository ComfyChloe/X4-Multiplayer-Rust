//! Shared validation primitives for the relay protocol.
//!
//! Every inbound packet is checked against the same small set of rules before
//! it can influence any state. The rules are deliberately strict: an
//! identifier is a short, boring ASCII token, a lease is exactly 32 lowercase
//! hex characters, and a sequence number is a bounded unsigned integer. The
//! reference implementation expresses these as regular expressions; the
//! patterns here match them exactly, including Python's `\Z` (absolute end of
//! string) rather than `$` (end of line), so a trailing newline is rejected
//! just as Python rejects it.

use std::net::SocketAddr;

/// Maximum size of a single relay packet, matching `MAX_PACKET` in the
/// reference implementation.
pub const MAX_PACKET: usize = 768;

/// Upper bound on a sequence number (`2**31 - 1`).
pub const MAX_SEQ: i64 = 2_147_483_647;

/// A lease is exactly 32 lowercase hexadecimal characters.
pub fn is_lease(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An identifier is 1..=96 characters of `[A-Za-z0-9_.-]`.
pub fn is_atom(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

/// A ware identifier is 1..=64 characters of `[a-z0-9_]`.
pub fn is_ware(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Parses a sequence number.
///
/// Mirrors `x4lan.host.sequence`: the text must be ASCII decimal digits and
/// must fit in the 31-bit bound.
pub fn sequence(value: &str) -> Option<i64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed: i64 = value.parse().ok()?;
    if !(0..=MAX_SEQ).contains(&parsed) {
        return None;
    }
    Some(parsed)
}

/// Parses a bounded integer, mirroring `x4lan.economy.integer`.
///
/// The reference accepts an optional leading `-` only when `signed` is set, and
/// always rejects values whose magnitude exceeds `limit`.
pub fn integer(value: &str, limit: i64, signed: bool) -> Option<i64> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(rest) if signed => (true, rest),
        Some(_) => return None,
        None => (false, value),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed: i64 = digits.parse().ok()?;
    if parsed > limit {
        return None;
    }
    if negative {
        Some(-parsed)
    } else {
        Some(parsed)
    }
}

/// Compares two secrets without an early exit, like `hmac.compare_digest`.
///
/// The token and lease both act as authentication material, so the comparison
/// must not leak how many leading bytes matched through timing.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// A peer's transport address, kept as raw bytes so it can be echoed back
/// without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerAddress {
    pub ip: [u8; 4],
    pub port: u16,
}

impl PeerAddress {
    pub fn from_socket_addr(address: SocketAddr) -> Self {
        let ip = match address.ip() {
            std::net::IpAddr::V4(v4) => v4.octets(),
            // The relay binds IPv4 only; a v6 address cannot reach it.
            std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(|m| m.octets()).unwrap_or([0; 4]),
        };
        Self {
            ip,
            port: address.port(),
        }
    }

    pub fn to_socket_addr(self) -> SocketAddr {
        SocketAddr::from((self.ip, self.port))
    }

    /// The dotted-quad form of the address, without the port.
    pub fn ip_text(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            self.ip[0], self.ip[1], self.ip[2], self.ip[3]
        )
    }
}

impl std::fmt::Display for PeerAddress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}.{}.{}.{}:{}",
            self.ip[0], self.ip[1], self.ip[2], self.ip[3], self.port
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_atoms() {
        assert!(is_atom("sector_macro"));
        assert!(is_atom("bubble-01"));
        assert!(is_atom("a.b_c-d"));
        assert!(!is_atom(""));
        assert!(!is_atom("has space"));
        assert!(!is_atom("has\nnewline"));
        assert!(!is_atom(&"x".repeat(97)));
    }

    #[test]
    fn validates_leases() {
        assert!(is_lease(&"a".repeat(32)));
        assert!(!is_lease(&"a".repeat(31)));
        assert!(!is_lease(&"A".repeat(32)));
        assert!(!is_lease(&"g".repeat(32)));
    }

    #[test]
    fn validates_sequences() {
        assert_eq!(sequence("0"), Some(0));
        assert_eq!(sequence("2147483647"), Some(MAX_SEQ));
        assert_eq!(sequence("2147483648"), None);
        assert_eq!(sequence("-1"), None);
        assert_eq!(sequence("1.0"), None);
        assert_eq!(sequence(""), None);
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
