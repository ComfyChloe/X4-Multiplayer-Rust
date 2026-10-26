//! The dedicated relay server.
//!
//! This is the Rust replacement for `python -m x4lan.host`. It speaks the same
//! UDP protocol, so the game-side Lua extension works against it unchanged.
//!
//! # Why a dedicated binary
//!
//! The Python reference ships with a bundled CPython runtime: the Windows
//! package embeds a 3.14.8 interpreter and the Linux package unpacks a
//! standalone build, both verified against published SHA-256 digests. A Rust
//! binary removes that entire dependency, along with the interpreter's
//! start-up cost and the platform-specific launcher scripts.
//!
//! # Usage
//!
//! ```text
//! x4lan-relay --session session.json
//! x4lan-relay --token <32 hex> --build 9.00-611726 --bubble bubble-01
//! ```

use std::net::{SocketAddr, UdpSocket};
use std::process::ExitCode;
use std::time::Instant;

use x4lan_core::protocol::PeerAddress;
use x4lan_core::session::{Mode, Session, TICK_HZ};

/// Maximum size of a single relay packet.
const MAX_PACKET: usize = 768;

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("{}", Options::USAGE);
            return ExitCode::FAILURE;
        }
    };

    let mode = options.mode();
    if let Err(message) = Session::validate_build(mode, &options.build) {
        eprintln!("error: {message}");
        return ExitCode::FAILURE;
    }
    let session = Session::new(mode, &options.token, &options.build, &options.bubble);

    let socket = match bind(&options.bind, options.port) {
        Ok(socket) => socket,
        Err(message) => {
            eprintln!(
                "error: could not bind {}/{}: {message}",
                options.bind, options.port
            );
            return ExitCode::FAILURE;
        }
    };

    println!(
        "X4 LAN transform relay on {}/{}; build={}; bubble={}",
        options.bind, options.port, options.build, options.bubble
    );
    match mode {
        Mode::Bubble | Mode::Economy => {
            println!("Experimental host-authoritative test bubble.");
        }
        _ => println!("Experiment: client poses only; no shared physics, damage, AI or economy."),
    }
    println!("Tick rate {TICK_HZ} Hz. Press Ctrl+C to stop.");

    let mut session = session;
    let mut buffer = [0u8; MAX_PACKET + 1];
    let mut last_status = String::new();
    let started = Instant::now();

    loop {
        // Drain the socket, then publish one snapshot tick. A flood of
        // inbound packets must not starve the outbound side, so the receive
        // loop is bounded per iteration.
        let mut processed = 0;
        loop {
            match socket.recv_from(&mut buffer) {
                Ok((length, address)) => {
                    processed += 1;
                    if processed >= 128 {
                        break;
                    }
                    let peer = PeerAddress::from_socket_addr(address);
                    for response in session.receive(&buffer[..length], peer, now(started)) {
                        send(&socket, &response, address);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // A departed Windows UDP endpoint surfaces as a connection
                    // reset; that is not a relay failure.
                    if error.raw_os_error() == Some(10054) || error.raw_os_error() == Some(10052) {
                        continue;
                    }
                    eprintln!("error: recv failed: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }

        for (peer, packet) in session.snapshots(now(started)) {
            send(&socket, &packet, peer.to_socket_addr());
        }

        let status = if session.peers.is_empty() {
            "waiting".to_string()
        } else {
            session
                .peers
                .iter()
                .map(|(slot, peer)| format!("{slot}@{}", peer.address))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if status != last_status {
            println!("{status}");
            last_status = status;
        }

        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Seconds since the relay started.
fn now(started: Instant) -> f64 {
    started.elapsed().as_secs_f64()
}

/// Sends a packet, ignoring a peer that has already gone away.
fn send(socket: &UdpSocket, packet: &[u8], address: SocketAddr) {
    let _ = socket.send_to(packet, address);
}

/// Binds the UDP socket.
fn bind(address: &str, port: u16) -> std::io::Result<UdpSocket> {
    let parsed: std::net::IpAddr = address
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad bind address"))?;
    let socket = UdpSocket::bind(SocketAddr::new(parsed, port))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

/// Command-line options.
struct Options {
    bind: String,
    port: u16,
    token: String,
    build: String,
    bubble: String,
    coop: bool,
    trading: bool,
    bubble_actions: bool,
    world_sync: bool,
}

impl Options {
    const USAGE: &'static str = "\
Usage:
  x4lan-relay --session <session.json>
  x4lan-relay --token <32 hex> --build <label> [--bubble <id>] [--bind <addr>] [--port <n>]

Options:
  --session <path>      Session file written by prepare.py
  --token <hex>         Shared session token (32 lowercase hex characters)
  --build <label>       Shared build label, identical on both clients
  --bubble <id>         Shared universe identifier
  --bind <addr>         Address to bind (default 127.0.0.1)
  --port <n>            UDP port (default 45841)
  --bubble-actions      Enable the experimental test bubble
  --trading             Enable market snapshots and trading
  --coop                Enable cooperative ships
  --world-sync          Enable whole-sector world synchronisation";

    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Options {
            bind: "127.0.0.1".to_string(),
            port: 45841,
            token: String::new(),
            build: String::new(),
            bubble: "bubble-01".to_string(),
            coop: false,
            trading: false,
            bubble_actions: false,
            world_sync: false,
        };
        let mut session_file: Option<String> = None;
        let mut arguments = arguments.peekable();

        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--bind" => options.bind = arguments.next().ok_or("--bind needs a value")?,
                "--port" => {
                    options.port = arguments
                        .next()
                        .ok_or("--port needs a value")?
                        .parse()
                        .map_err(|_| "--port must be a number")?
                }
                "--token" => options.token = arguments.next().ok_or("--token needs a value")?,
                "--build" => options.build = arguments.next().ok_or("--build needs a value")?,
                "--bubble" => options.bubble = arguments.next().ok_or("--bubble needs a value")?,
                "--session" => {
                    session_file = Some(arguments.next().ok_or("--session needs a value")?)
                }
                "--bubble-actions" => options.bubble_actions = true,
                "--trading" => options.trading = true,
                "--coop" => options.coop = true,
                "--world-sync" => options.world_sync = true,
                "-h" | "--help" => {
                    println!("{}", Options::USAGE);
                    std::process::exit(0);
                }
                other => return Err(format!("unknown option {other}")),
            }
        }

        if let Some(path) = session_file {
            let contents = std::fs::read_to_string(&path)
                .map_err(|error| format!("could not read {path}: {error}"))?;
            let value = parse_session_json(&contents)?;
            if options.token.is_empty() {
                options.token = value.token;
            }
            if options.build.is_empty() {
                options.build = value.build;
            }
            if options.bubble == "bubble-01" {
                options.bubble = value.bubble;
            }
            if options.port == 45841 {
                options.port = value.port;
            }
            options.coop |= value.coop;
            options.world_sync |= value.world_sync;
        }

        if options.token.is_empty() || options.build.is_empty() {
            return Err("supply --session or both --token and --build".to_string());
        }
        Ok(options)
    }

    /// The protocol layer to run.
    fn mode(&self) -> Mode {
        if self.world_sync {
            Mode::Universe
        } else if self.coop {
            Mode::Dock
        } else if self.trading {
            Mode::Economy
        } else if self.bubble_actions {
            Mode::Bubble
        } else {
            Mode::Simple
        }
    }
}

/// The fields `prepare.py` writes into `session.json`.
struct SessionFile {
    token: String,
    build: String,
    bubble: String,
    port: u16,
    coop: bool,
    world_sync: bool,
}

/// Reads the handful of fields the relay needs from `session.json`.
///
/// The reference implementation uses `json.load`; this parses the same file
/// without pulling in a JSON dependency, keeping the binary small.
fn parse_session_json(contents: &str) -> Result<SessionFile, String> {
    let mut token = String::new();
    let mut build = String::new();
    let mut bubble = String::new();
    let mut port = 45841u16;
    let mut coop = false;
    let mut world_sync = false;

    for line in contents.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('"');
        let value = value.trim().trim_matches('"');
        match key {
            "token" => token = value.to_string(),
            "build" => build = value.to_string(),
            "bubble" => bubble = value.to_string(),
            "port" => {
                port = value
                    .parse()
                    .map_err(|_| "session.json has a non-numeric port")?
            }
            "coop" => coop = value == "true",
            "world_sync" => world_sync = value == "true",
            _ => {}
        }
    }
    Ok(SessionFile {
        token,
        build,
        bubble,
        port,
        coop,
        world_sync,
    })
}
