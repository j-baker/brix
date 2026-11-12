//! TOML timestamp formatting shared by both evaluation paths.

/// toml11's date-time formatting: fractional seconds are printed with 3,
/// 6 or 9 digits depending on which of milli-, micro- or nanoseconds are
/// non-zero (`normalizeSubsecondPrecision` in Nix's `fromTOML.cc`).
pub(crate) fn normalize_toml_datetime(s: &str) -> String {
    let Some(dot) = s.find('.') else {
        return s.to_owned();
    };
    let rest = &s[dot + 1..];
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (digits, suffix) = rest.split_at(digits_end);
    let mut nanos = format!("{digits:0<9}");
    nanos.truncate(9);
    let millis = &nanos[0..3];
    let micros = &nanos[3..6];
    let nano = &nanos[6..9];
    let precision = if nano != "000" {
        9
    } else if micros != "000" {
        6
    } else if millis != "000" {
        3
    } else {
        0
    };
    if precision == 0 {
        return format!("{}{}", &s[..dot], suffix);
    }
    format!("{}.{}{}", &s[..dot], &nanos[..precision], suffix)
}

#[cfg(test)]
mod toml_tests {
    use super::normalize_toml_datetime;

    #[test]
    fn subsecond_precision() {
        assert_eq!(normalize_toml_datetime("07:32:00.1"), "07:32:00.100");
        assert_eq!(normalize_toml_datetime("00:32:00.1234"), "00:32:00.123400");
        assert_eq!(
            normalize_toml_datetime("00:32:00.1234567"),
            "00:32:00.123456700"
        );
        assert_eq!(
            normalize_toml_datetime("1979-05-27T07:32:00.12Z"),
            "1979-05-27T07:32:00.120Z"
        );
        assert_eq!(
            normalize_toml_datetime("1979-05-27T00:32:00-07:00"),
            "1979-05-27T00:32:00-07:00"
        );
    }
}
