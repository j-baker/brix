//! Setting values parsed as Nix parses them (`BaseSetting<T>::parse` in
//! `configuration.cc` and `config-impl.hh`), with Nix's errors: the one
//! parser for every place brix reads a setting, `nix.conf` and the
//! command line (`brix::nixconf`) as well as store URI parameters
//! (`brix-daemon`).

/// A setting value Nix rejects, with its `UsageError` message.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SettingError {
    /// `BaseSetting<bool>::parse`.
    #[error("Boolean setting '{name}' has invalid value '{value}'")]
    Bool { name: String, value: String },
    /// `BaseSetting<N>::parse` for an integer type `N`.
    #[error("setting '{name}' has invalid value '{value}'")]
    Int { name: String, value: String },
}

/// Why `string2IntWithUnitPrefix` rejected its text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IntError {
    /// A letter other than `K`, `M`, `G` or `T` at the end (upper-cased).
    #[error("invalid unit specifier '{0}'")]
    UnitSpecifier(char),
    /// What is left without the unit is not a number of the type.
    #[error("'{0}' is not an integer")]
    NotAnInteger(String),
}

/// The integer types settings have (`unsigned int`, `size_t`, `time_t`,
/// …), as `string2Int` reads them.
pub trait SettingInt: Copy + std::str::FromStr {
    /// `n * multiplier` computed in `uint64_t` and converted back to the
    /// type, wrapping as the C++ conversion does.
    fn scale(self, multiplier: u64) -> Self;
}

macro_rules! setting_ints {
    ($($t:ty),*) => {$(
        impl SettingInt for $t {
            fn scale(self, multiplier: u64) -> Self {
                // Sign-extends a negative value first, as the C++
                // conversion to `uint64_t` does.
                (self as u64).wrapping_mul(multiplier) as $t
            }
        }
    )*};
}

setting_ints!(u32, u64, usize, i32, i64);

/// `string2IntWithUnitPrefix<N>`: a decimal number of type `N` (an
/// optional sign, `-` only for a signed type, then digits: what
/// `boost::lexical_cast` takes, no whitespace) with an optional `K`, `M`,
/// `G` or `T` suffix, either case, multiplying it by that power of 1024.
/// The number must fit in `N`; the product wraps.
pub fn int_with_unit_prefix<N: SettingInt>(s: &str) -> Result<N, IntError> {
    let mut digits = s;
    let mut multiplier = 1u64;
    if let Some(&last) = s.as_bytes().last() {
        let unit = last.to_ascii_uppercase();
        if unit.is_ascii_alphabetic() {
            multiplier = match unit {
                b'K' => 1 << 10,
                b'M' => 1 << 20,
                b'G' => 1 << 30,
                b'T' => 1 << 40,
                _ => return Err(IntError::UnitSpecifier(unit as char)),
            };
            digits = &s[..s.len() - 1];
        }
    }
    // Rust's integer parsing takes exactly what `lexical_cast` does: a
    // leading `+`, a leading `-` only for signed types.
    digits
        .parse::<N>()
        .map(|n| n.scale(multiplier))
        .map_err(|_| IntError::NotAnInteger(digits.to_owned()))
}

/// `BaseSetting<N>::parse` for an integer setting `name` of type `N`.
pub fn parse_int<N: SettingInt>(name: &str, value: &str) -> Result<N, SettingError> {
    int_with_unit_prefix(value).map_err(|_| SettingError::Int {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// `BaseSetting<bool>::parse`.
pub fn parse_bool(name: &str, value: &str) -> Result<bool, SettingError> {
    match value {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(SettingError::Bool {
            name: name.to_owned(),
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `string2IntWithUnitPrefix` with its own errors, as `nix path-info
    /// --json-format` reports them.
    #[test]
    fn unit_prefixes_like_nix() {
        assert_eq!(int_with_unit_prefix::<u64>("2K"), Ok(2048));
        assert_eq!(int_with_unit_prefix::<u64>("1g"), Ok(1 << 30));
        assert_eq!(int_with_unit_prefix::<u64>("+3"), Ok(3));
        assert_eq!(int_with_unit_prefix::<i64>("-2k"), Ok(-2048));
        assert_eq!(
            int_with_unit_prefix::<u64>("5x"),
            Err(IntError::UnitSpecifier('X'))
        );
        for bad in ["-1", "", " 5", "5 ", "K", "0x10", "+"] {
            assert!(
                matches!(
                    int_with_unit_prefix::<u32>(bad),
                    Err(IntError::NotAnInteger(_))
                ),
                "{bad:?}"
            );
        }
        assert_eq!(
            int_with_unit_prefix::<u32>("-1").unwrap_err().to_string(),
            "'-1' is not an integer"
        );
        // The number must fit the type; the product wraps at its width.
        assert!(int_with_unit_prefix::<u32>("4294967296").is_err());
        assert_eq!(int_with_unit_prefix::<u32>("4G"), Ok(0));
        assert_eq!(int_with_unit_prefix::<u64>("4G"), Ok(4 << 30));
        assert_eq!(
            int_with_unit_prefix::<u64>("18446744073709551615K"),
            Ok(!0 << 10)
        );
    }

    #[test]
    fn settings_errors_like_nix() {
        assert_eq!(parse_int::<u32>("cores", "2K"), Ok(2048));
        assert_eq!(
            parse_int::<u32>("max-call-depth", "abc")
                .unwrap_err()
                .to_string(),
            "setting 'max-call-depth' has invalid value 'abc'"
        );
        for (text, value) in [("yes", true), ("1", true), ("no", false), ("false", false)] {
            assert_eq!(parse_bool("fallback", text), Ok(value));
        }
        assert_eq!(
            parse_bool("fallback", "maybe").unwrap_err().to_string(),
            "Boolean setting 'fallback' has invalid value 'maybe'"
        );
    }
}
