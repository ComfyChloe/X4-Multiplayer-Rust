//! Prepares the two client folders for a session.
//!
//! This is the Rust replacement for `prepare.py`. It creates the output
//! directory, writes a `session.json` with a freshly generated token, and
//! copies the game-side extension into two slot folders, each with its own
//! `ui/config.lua`.
//!
//! The two slots are `p1` (the host) and `p2` (the guest). Both receive the
//! same token, build label and universe identifier; they differ in the slot
//! name and in the port they use.
//!
//! # Usage
//!
//! ```text
//! x4lan-prepare --host <lan-ip> --build <label> --output <dir>
//! x4lan-prepare --host <lan-ip> --build <label> --output <dir> --extension <path>
//! ```
//!
//! `--extension` points at the `x4_lan_lab` extension directory. It
//! defaults to a sibling checkout of the reference project, so the usual
//! layout needs no extra flags.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The default UDP port.
const DEFAULT_PORT: u16 = 45841;

/// The default universe identifier.
const DEFAULT_BUBBLE: &str = "bubble-01";

/// The game-side extension's directory name.
const EXTENSION_NAME: &str = "x4_lan_lab";

/// The X4 profile the extension expects.
const TEST_PROFILE: &str = "45841";

/// The ship the proxy uses in the test bubble.
const PROXY_MACRO: &str = "ship_arg_s_scout_01_a_macro";

fn main() -> ExitCode {
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!("{}", Options::USAGE);
            return ExitCode::FAILURE;
        }
    };

    if let Err(message) = run(&options) {
        eprintln!("error: {message}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Performs the preparation.
fn run(options: &Options) -> Result<(), String> {
    let source = match &options.extension {
        Some(path) => path.clone(),
        None => default_extension_path(),
    };
    if !source.join("content.xml").is_file() {
        return Err(format!(
            "no extension at {}; pass --extension <path to the x4_lan_lab directory>",
            source.display()
        ));
    }
    if options.output.exists() {
        return Err(format!(
            "{} already exists; remove it or choose another output directory",
            options.output.display()
        ));
    }

    let token = generate_token();
    let session = format!(
        "{{\n  \"token\": \"{token}\",\n  \"build\": \"{}\",\n  \"bubble\": \"{DEFAULT_BUBBLE}\",\n  \"port\": {}\n}}\n",
        options.build, options.port
    );

    fs::create_dir_all(&options.output)
        .map_err(|error| format!("could not create {}: {error}", options.output.display()))?;
    fs::write(options.output.join("session.json"), &session)
        .map_err(|error| format!("could not write session.json: {error}"))?;

    for slot in ["p1", "p2"] {
        let target = options
            .output
            .join(slot)
            .join("extensions")
            .join(EXTENSION_NAME);
        copy_tree(&source, &target)?;

        // Each slot gets its own configuration, pointing at the host.
        // The field order matches the reference's, so a generated config can
        // be compared with one the reference produced.
        let values: Vec<(&str, String)> = vec![
            ("enabled", "true".to_string()),
            ("test_profile", format!("\"{TEST_PROFILE}\"")),
            ("host", format!("\"{}\"", options.host)),
            ("token", format!("\"{token}\"")),
            ("build", format!("\"{}\"", options.build)),
            ("bubble", format!("\"{DEFAULT_BUBBLE}\"")),
            ("port", options.port.to_string()),
            ("slot", format!("\"{slot}\"")),
            ("mode", format!("\"{}\"", options.mode)),
            ("proxy_macro", format!("\"{PROXY_MACRO}\"")),
        ];

        let mut config = String::from("-- Generated X4 LAN Lab configuration\nX4LAN_CONFIG = {\n");
        for (key, value) in &values {
            config.push_str(&format!("    {key} = {value},\n"));
        }
        config.push_str("}\n");
        fs::write(target.join("ui").join("config.lua"), &config)
            .map_err(|error| format!("could not write the config for {slot}: {error}"))?;
    }

    println!(
        "Prepared {} and {}",
        options.output.join("p1").display(),
        options.output.join("p2").display()
    );
    println!("No game files were modified.");
    Ok(())
}

/// Copies a directory tree.
fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target)
        .map_err(|error| format!("could not create {}: {error}", target.display()))?;
    for entry in fs::read_dir(source)
        .map_err(|error| format!("could not read {}: {error}", source.display()))?
    {
        let entry = entry.map_err(|error| format!("could not read an entry: {error}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let destination = target.join(&name);
        if path.is_dir() {
            copy_tree(&path, &destination)?;
        } else {
            fs::copy(&path, &destination).map_err(|error| {
                format!(
                    "could not copy {} to {}: {error}",
                    path.display(),
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

/// Generates a 32-character lowercase hex token.
///
/// The token is a shared secret: it authenticates the peers to the relay, so
/// it must be unguessable.
fn generate_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut token = String::with_capacity(32);
    for _ in 0..4 {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        token.push_str(&format!("{value:016x}"));
    }
    token.truncate(32);
    token
}

/// Where the extension lives when the projects sit side by side.
fn default_extension_path() -> PathBuf {
    let sibling = Path::new("X4-Multiplayer-Source")
        .join("extension")
        .join(EXTENSION_NAME);
    if sibling.is_dir() {
        return sibling;
    }
    // Also look next to the current directory, so the tool works from
    // anywhere inside the workspace.
    let here = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for ancestor in here.ancestors() {
        let candidate = ancestor
            .join("X4-Multiplayer-Source")
            .join("extension")
            .join(EXTENSION_NAME);
        if candidate.is_dir() {
            return candidate;
        }
    }
    sibling
}

/// Command-line options.
struct Options {
    host: String,
    build: String,
    output: PathBuf,
    extension: Option<PathBuf>,
    port: u16,
    mode: String,
}

impl Options {
    const USAGE: &'static str = "\
Usage:
  x4lan-prepare --host <lan-ip> --build <label> --output <dir>

Options:
  --host <ip>        LAN IPv4 address of the relay computer
  --build <label>    Exact shared X4 build label
  --output <dir>     Directory to create; must not already exist
  --extension <dir>  The x4_lan_lab extension directory
  --port <n>         UDP port (default 45841)
  --mode <mode>      telemetry or proxy (default telemetry)";

    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut host = String::new();
        let mut build = String::new();
        let mut output = PathBuf::new();
        let mut extension = None;
        let mut port = DEFAULT_PORT;
        let mut mode = "telemetry".to_string();

        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--host" => host = arguments.next().ok_or("--host needs a value")?,
                "--build" => build = arguments.next().ok_or("--build needs a value")?,
                "--output" => {
                    output = PathBuf::from(arguments.next().ok_or("--output needs a value")?)
                }
                "--extension" => {
                    extension = Some(PathBuf::from(
                        arguments.next().ok_or("--extension needs a value")?,
                    ))
                }
                "--port" => {
                    port = arguments
                        .next()
                        .ok_or("--port needs a value")?
                        .parse()
                        .map_err(|_| "--port must be a number")?
                }
                "--mode" => {
                    mode = arguments.next().ok_or("--mode needs a value")?;
                    if mode != "telemetry" && mode != "proxy" {
                        return Err("--mode must be telemetry or proxy".to_string());
                    }
                }
                "-h" | "--help" => {
                    println!("{}", Options::USAGE);
                    std::process::exit(0);
                }
                other => return Err(format!("unknown option {other}")),
            }
        }

        if host.is_empty() {
            return Err("--host is required".to_string());
        }
        // The reference validates the address strictly, as a bare IPv4.
        if host.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(format!("--host must be an IPv4 address, not {host}"));
        }
        if build.is_empty() || build.len() > 96 {
            return Err("--build must be an identifier of 1 to 96 characters".to_string());
        }
        if !build
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
        {
            return Err("--build must be an identifier such as 9.00-611726".to_string());
        }
        if output.as_os_str().is_empty() {
            return Err("--output is required".to_string());
        }
        Ok(Options {
            host,
            build,
            output,
            extension,
            port,
            mode,
        })
    }
}
