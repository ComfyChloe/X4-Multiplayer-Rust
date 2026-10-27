//! Python-compatible number formatting and parsing.
//!
//! The wire protocol is byte-exact: a guest and a host must agree on the text
//! of a pose, or the two sides disagree about where a ship is. The Python
//! reference therefore formats floats with `format(x, '.9g')` (and `'%.17g'`
//! for projectile velocities), and those are *not* the same as Rust's default
//! `{}` formatting, which prints the shortest round-trip representation.
//!
//! `1.0` formats as `1` under `.9g` but as `1` under Rust's `{}` too, yet
//! `0.1 + 0.2` differs: Python's `.9g` gives `0.3` while Rust's `{}` gives
//! `0.30000000000000004`. Every helper here exists to remove that class of
//! difference.

/// Formats a float the way Python's `format(value, f'.{precision}g')` does.
///
/// This implements the C `printf` `%g` rules that Python follows:
///
/// * `precision` significant digits are used (minimum 1).
/// * The exponent `X` is taken from the `%e` rendering with `precision - 1`
///   fractional digits.
/// * If `precision > X >= -4` the value is rendered in fixed-point notation
///   with `precision - 1 - X` fractional digits, otherwise in scientific
///   notation with `precision - 1` fractional digits.
/// * Trailing zeros are stripped, and a trailing decimal point is removed.
///
/// ```
/// # use x4lan_core::format::py_g;
/// assert_eq!(py_g(0.30000000000000004, 9), "0.3");
/// assert_eq!(py_g(1.0, 9), "1");
/// assert_eq!(py_g(1e20, 9), "1e+20");
/// ```
pub fn py_g(value: f64, precision: usize) -> String {
    if value.is_nan() {
        return "nan".to_string();
    }
    if value.is_infinite() {
        return if value < 0.0 {
            "-inf".into()
        } else {
            "inf".into()
        };
    }

    let precision = precision.max(1);
    let negative = value.is_sign_negative();

    // Rust's `{:e}` gives a correctly rounded mantissa with the requested
    // number of fractional digits, which is exactly the `%e` rendering Python
    // uses to determine the exponent. Parse the pieces back out.
    let scientific = format!("{:.*e}", precision - 1, value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("Rust always renders an exponent for the LowerExp formatter");
    let exponent: i32 = exponent
        .parse()
        .expect("Rust always renders a decimal exponent");
    let mantissa_digits: Vec<char> = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits: String = mantissa_digits.iter().collect();

    let mut out = String::new();
    if negative {
        out.push('-');
    }

    if precision as i32 > exponent && exponent >= -4 {
        // Fixed-point notation.
        let fractional = precision as i32 - 1 - exponent;
        if exponent >= 0 {
            let integer_len = (exponent + 1) as usize;
            for index in 0..integer_len {
                out.push(mantissa_digits.get(index).copied().unwrap_or('0'));
            }
            if fractional > 0 {
                out.push('.');
                for index in 0..fractional as usize {
                    out.push(
                        mantissa_digits
                            .get(integer_len + index)
                            .copied()
                            .unwrap_or('0'),
                    );
                }
            }
        } else {
            // 0.000ddd
            out.push('0');
            out.push('.');
            for _ in 0..(-exponent - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        }
    } else {
        // Scientific notation: d.dddde[+-]XX
        out.push(mantissa_digits.first().copied().unwrap_or('0'));
        let fractional = precision - 1;
        if fractional > 0 {
            out.push('.');
            for index in 1..=fractional {
                out.push(mantissa_digits.get(index).copied().unwrap_or('0'));
            }
        }
        out.push('e');
        if exponent < 0 {
            out.push('-');
        } else {
            out.push('+');
        }
        let magnitude = exponent.unsigned_abs();
        if magnitude < 10 {
            out.push('0');
        }
        out.push_str(&magnitude.to_string());
    }

    strip_trailing_zeros(&out)
}

/// Removes insignificant trailing zeros from the fractional part.
fn strip_trailing_zeros(text: &str) -> String {
    if !text.contains('.') {
        return text.to_string();
    }
    // Keep the exponent suffix intact; it never contains a decimal point.
    let (mantissa, exponent) = match text.split_once('e') {
        Some((m, e)) => (m, Some(e)),
        None => (text, None),
    };
    let trimmed = mantissa.trim_end_matches('0');
    let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
    match exponent {
        Some(e) => format!("{trimmed}e{e}"),
        None => trimmed.to_string(),
    }
}

/// Parses a float the way Python's `float(text)` does.
///
/// The reference implementation feeds wire fields straight into `float()`.
/// Python is more permissive than Rust here in three ways that matter:
///
/// * surrounding whitespace is ignored,
/// * underscores may separate digits (`1_000`),
/// * `inf`, `infinity` and `nan` are accepted in any case.
///
/// Accepting the same strings keeps a peer and a host in agreement about which
/// messages are well formed, which is what the protocol's rejection rules
/// depend on.
pub fn py_float(text: &str) -> Option<f64> {
    let trimmed = text.trim_matches(|c: char| c.is_ascii_whitespace());
    if trimmed.is_empty() {
        return None;
    }

    // Python allows single underscores between digits only.
    let mut cleaned = String::with_capacity(trimmed.len());
    let bytes: Vec<char> = trimmed.chars().collect();
    for (index, &character) in bytes.iter().enumerate() {
        if character == '_' {
            let before = index.checked_sub(1).and_then(|i| bytes.get(i));
            let after = bytes.get(index + 1);
            match (before, after) {
                (Some(b), Some(a)) if b.is_ascii_digit() && a.is_ascii_digit() => continue,
                _ => return None,
            }
        }
        cleaned.push(character);
    }

    // `str::parse` accepts "inf"/"infinity"/"nan" case-insensitively, matching
    // Python, but it rejects a bare sign or a missing mantissa the same way
    // Python's `float()` does.
    cleaned.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_python_g() {
        // Values chosen to cover fixed/scientific switching and rounding.
        assert_eq!(py_g(1.0, 9), "1");
        assert_eq!(py_g(0.1, 9), "0.1");
        assert_eq!(py_g(0.30000000000000004, 9), "0.3");
        assert_eq!(py_g(-2.5, 9), "-2.5");
        assert_eq!(py_g(0.0, 9), "0");
        assert_eq!(py_g(1e20, 9), "1e+20");
        assert_eq!(py_g(1e-5, 9), "1e-05");
        assert_eq!(py_g(123456789.0, 9), "123456789");
        assert_eq!(py_g(0.0001, 9), "0.0001");
        assert_eq!(py_g(1e300, 9), "1e+300");
    }

    #[test]
    fn formats_high_precision() {
        // '%.17g' must round-trip a double exactly.
        let value = 0.1f64 + 0.2f64;
        let rendered = py_g(value, 17);
        assert_eq!(rendered.parse::<f64>().unwrap(), value);
    }

    #[test]
    fn parses_like_python_float() {
        assert_eq!(py_float("1"), Some(1.0));
        assert_eq!(py_float("  2.5  "), Some(2.5));
        assert_eq!(py_float("1_000"), Some(1000.0));
        assert_eq!(py_float("1e3"), Some(1000.0));
        assert_eq!(py_float("inf"), Some(f64::INFINITY));
        // NaN is never equal to itself, so it needs its own check.
        assert!(py_float("NaN").is_some_and(|value| value.is_nan()));
        assert_eq!(py_float(""), None);
        assert_eq!(py_float("abc"), None);
        assert_eq!(py_float("_1"), None);
        assert_eq!(py_float("1_"), None);
        assert_eq!(py_float("1__0"), None);
    }
}
