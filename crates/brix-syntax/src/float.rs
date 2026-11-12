//! Formatting of floats the way C++'s `std::ostream << double` does it.
//!
//! Nix prints floats with the stream's default settings: `%g` with a
//! precision of 6 significant digits, trailing zeros removed. Rust has no
//! `%g`, so this reproduces it. The parser's `--parse` output and the
//! evaluator's value printers both use it.

/// The precision `std::ostream` uses by default.
const PRECISION: usize = 6;

/// Formats `v` as `printf("%g")` (precision 6) does, which is what
/// `std::ostream << double` prints. A NaN keeps its sign, as glibc prints
/// it (`-nan`): a float literal is never one, but a computed value can be
/// (`0.0 / 0.0` is `-nan` on x86-64).
pub fn format_g(v: f64) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() {
            "-nan".into()
        } else {
            "nan".into()
        };
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if v == 0.0 {
        return if v.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    // `{:e}` with precision-1 decimals gives the correctly rounded mantissa
    // and the decimal exponent *after* rounding, which is what `%g` uses to
    // pick the notation.
    let sci = format!("{:.*e}", PRECISION - 1, v);
    let (mantissa, exponent) = sci
        .split_once('e')
        .expect("Rust `{:e}` always has an exponent");
    let exponent: i32 = exponent
        .parse()
        .expect("Rust `{:e}` exponent is an integer");
    if exponent < -4 || exponent >= PRECISION as i32 {
        let mantissa = strip_trailing_zeros(mantissa);
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    } else {
        let decimals = (PRECISION as i32 - 1 - exponent).max(0) as usize;
        strip_trailing_zeros(&format!("{:.*}", decimals, v)).to_string()
    }
}

fn strip_trailing_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cpp_ostream() {
        // Expected values are what `nix-instantiate --parse` prints.
        for (v, s) in [
            (1.0, "1"),
            (1.5, "1.5"),
            (2.0, "2"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (0.1 + 0.2, "0.3"),
            (100.0, "100"),
            (100000.0, "100000"),
            (999999.0, "999999"),
            (9999999.0, "1e+07"),
            (1234567.0, "1.23457e+06"),
            (1234567.5, "1.23457e+06"),
            (123456789.0, "1.23457e+08"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (0.000123456789, "0.000123457"),
            (0.000099999999, "0.0001"),
            (1.25e-5, "1.25e-05"),
            (1e-7, "1e-07"),
            (2.5e8, "2.5e+08"),
            (1e10, "1e+10"),
            (1e30, "1e+30"),
            (1.5e300, "1.5e+300"),
            (5e-5, "5e-05"),
            (0.0, "0"),
            (-0.0, "-0"),
            (-2.5, "-2.5"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
            (-f64::NAN, "-nan"),
        ] {
            assert_eq!(format_g(v), s, "formatting {v:?}");
        }
    }
}
