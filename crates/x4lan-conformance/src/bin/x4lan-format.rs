//! Formats numbers the way the relay does, for comparison with another
//! implementation.
//!
//! Reads one number per line from standard input and writes the relay's
//! rendering of each on its own line. The point is to compare the port's
//! `py_g` against another language's own general-format rendering, because
//! the wire protocol is byte-exact and a difference in how a float is
//! rendered is a difference on the wire.
//!
//! ```text
//! echo 0.30000000000000004 | x4lan-format
//! 0.3
//! ```

use std::io::{BufRead, Write};

use x4lan_core::format::py_g;

fn main() -> std::process::ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let precision: usize = arguments
        .first()
        .and_then(|value| value.parse().ok())
        .unwrap_or(9);

    let stdin = std::io::stdin();
    let mut output = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = trimmed.parse::<f64>() else {
            eprintln!("not a number: {trimmed}");
            continue;
        };
        if writeln!(output, "{}", py_g(value, precision)).is_err() {
            return std::process::ExitCode::FAILURE;
        }
    }
    std::process::ExitCode::SUCCESS
}
