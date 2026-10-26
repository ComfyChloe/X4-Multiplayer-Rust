//! Differential conformance harness.
//!
//! Reads a scenario file, replays it through the Rust session, and writes a
//! transcript of every response and snapshot. The Python reference produces a
//! transcript from the same scenario, and the two are compared byte for byte.
//!
//! That comparison is the only honest way to claim the Rust port is 1:1 with
//! the Python one. Reading both implementations side by side and asserting
//! they "look the same" is not evidence; running the same inputs through
//! both and diffing the outputs is.
//!
//! # Scenario format
//!
//! ```json
//! {
//!   "mode": "simple",
//!   "token": "aaaa...",
//!   "build": "test",
//!   "bubble": "bubble",
//!   "steps": [
//!     {"at": 0.0, "packet": "HELLO|1|...", "from": "127.0.0.1:10001"},
//!     {"at": 0.1, "packet": "STATE|1|...", "from": "127.0.0.1:10001"},
//!     {"at": 0.2, "snapshot": true}
//!   ]
//! }
//! ```

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use x4lan_core::protocol::PeerAddress;
use x4lan_core::session::{Mode, Session};

/// A scenario to replay.
#[derive(Debug, Deserialize)]
struct Scenario {
    mode: String,
    token: String,
    build: String,
    bubble: String,
    #[serde(default)]
    steps: Vec<Step>,
}

/// One step of a scenario.
#[derive(Debug, Deserialize)]
struct Step {
    /// The time, in seconds, at which the step happens.
    at: f64,
    /// A packet to deliver, if this step carries one.
    #[serde(default)]
    packet: Option<String>,
    /// The sender's address.
    #[serde(default)]
    from: Option<String>,
    /// Whether to collect the snapshot tick.
    #[serde(default)]
    snapshot: bool,
}

/// One recorded event.
#[derive(Debug, Serialize)]
struct Event {
    at: f64,
    kind: &'static str,
    to: String,
    payload: String,
}

fn main() -> std::process::ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 2 {
        eprintln!("usage: x4lan-conformance <scenario.json> <transcript.json>");
        return std::process::ExitCode::FAILURE;
    }
    let scenario_path = &arguments[0];
    let transcript_path = &arguments[1];

    let mut contents = String::new();
    if let Err(error) = std::fs::File::open(scenario_path)
        .and_then(|mut file| file.read_to_string(&mut contents))
    {
        eprintln!("error: could not read {scenario_path}: {error}");
        return std::process::ExitCode::FAILURE;
    }

    let scenario: Scenario = match serde_json::from_str(&contents) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("error: could not parse {scenario_path}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let Some(mode) = parse_mode(&scenario.mode) else {
        eprintln!("error: unknown mode {:?}", scenario.mode);
        return std::process::ExitCode::FAILURE;
    };

    let mut session = Session::new(mode, &scenario.token, &scenario.build, &scenario.bubble);
    // Leases must match the reference's sequence, so both sides generate them
    // the same way.
    session.use_deterministic_leases();

    let mut events: Vec<Event> = Vec::new();

    for step in scenario.steps {
        if let Some(packet) = &step.packet {
            let from = step.from.as_deref().unwrap_or("127.0.0.1:10001");
            let Some(address) = parse_address(from) else {
                eprintln!("error: bad address {from}");
                return std::process::ExitCode::FAILURE;
            };
            let raw = packet.as_bytes();
            for response in session.receive(raw, address, step.at) {
                events.push(Event {
                    at: step.at,
                    kind: "reply",
                    to: from.to_string(),
                    payload: String::from_utf8_lossy(&response).to_string(),
                });
            }
        }
        if step.snapshot {
            for (peer, packet) in session.snapshots(step.at) {
                events.push(Event {
                    at: step.at,
                    kind: "snapshot",
                    to: peer.to_string(),
                    payload: String::from_utf8_lossy(&packet).to_string(),
                });
            }
        }
    }

    let transcript = serde_json::to_string_pretty(&events).unwrap_or_else(|_| "[]".to_string());
    if let Err(error) = std::fs::File::create(transcript_path)
        .and_then(|mut file| file.write_all(transcript.as_bytes()))
    {
        eprintln!("error: could not write {transcript_path}: {error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// Maps a scenario mode name onto a protocol layer.
fn parse_mode(name: &str) -> Option<Mode> {
    match name {
        "simple" => Some(Mode::Simple),
        "bubble" => Some(Mode::Bubble),
        "economy" => Some(Mode::Economy),
        "coop" => Some(Mode::Coop),
        "dock" => Some(Mode::Dock),
        "universe" => Some(Mode::Universe),
        _ => None,
    }
}

/// Parses `127.0.0.1:10001` into a peer address.
fn parse_address(text: &str) -> Option<PeerAddress> {
    let (host, port) = text.rsplit_once(':')?;
    let address: std::net::Ipv4Addr = host.parse().ok()?;
    let port: u16 = port.parse().ok()?;
    Some(PeerAddress {
        ip: address.octets(),
        port,
    })
}
