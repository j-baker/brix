//! The experimental features that change what the store code accepts.
//!
//! Nix consults one global `ExperimentalFeatureSettings` wherever a
//! feature matters (`parseHashAlgoOpt` for BLAKE3, `BasicDerivation::type`
//! for `builtin:substitute`, …); some of those functions also take it as
//! an `xpSettings` parameter. brix has no global: the functions that check
//! a feature take an [`XpSettings`] saying which ones are enabled.

use std::fmt;

/// An experimental feature that the store code checks (`Xp` in Nix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Xp {
    /// `blake3-hashes`: the BLAKE3 hash algorithm.
    Blake3Hashes,
    /// `baked-derivations`: `builtin:substitute` derivations.
    BakedDerivations,
    /// `ca-derivations`: floating content-addressed derivation outputs.
    CaDerivations,
    /// `impure-derivations`: impure derivation outputs.
    ImpureDerivations,
    /// `dynamic-derivations`: text-hashed outputs and the `xp-dyn-drv`
    /// derivation format.
    DynamicDerivations,
    /// `git-hashing`: the Git content-addressing method.
    GitHashing,
}

impl Xp {
    const ALL: [Xp; 6] = [
        Xp::Blake3Hashes,
        Xp::BakedDerivations,
        Xp::CaDerivations,
        Xp::ImpureDerivations,
        Xp::DynamicDerivations,
        Xp::GitHashing,
    ];

    /// The name in the `experimental-features` setting
    /// (`showExperimentalFeature`).
    pub const fn name(self) -> &'static str {
        match self {
            Xp::Blake3Hashes => "blake3-hashes",
            Xp::BakedDerivations => "baked-derivations",
            Xp::CaDerivations => "ca-derivations",
            Xp::ImpureDerivations => "impure-derivations",
            Xp::DynamicDerivations => "dynamic-derivations",
            Xp::GitHashing => "git-hashing",
        }
    }

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// Which [`Xp`] features are enabled: the part of Nix's
/// `ExperimentalFeatureSettings` that the store code consults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct XpSettings {
    enabled: u8,
}

impl XpSettings {
    /// No feature enabled, which is Nix's default.
    pub const NONE: XpSettings = XpSettings { enabled: 0 };

    /// The store's features among the enabled `names` (the
    /// `experimental-features` setting). Features the store code does
    /// not check are not its concern and are left out.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> XpSettings {
        names
            .into_iter()
            .filter_map(|name| Xp::ALL.into_iter().find(|xp| xp.name() == name))
            .fold(XpSettings::NONE, XpSettings::with)
    }

    /// These settings with `xp` enabled as well.
    pub const fn with(self, xp: Xp) -> XpSettings {
        XpSettings {
            enabled: self.enabled | xp.bit(),
        }
    }

    /// `isEnabled`.
    pub const fn is_enabled(self, xp: Xp) -> bool {
        self.enabled & xp.bit() != 0
    }

    /// `require`: Nix's error unless `xp` is enabled. `reason` says what
    /// needed it; Nix shows it in parentheses after "disabled".
    pub fn require(self, xp: Xp, reason: Option<&str>) -> Result<(), MissingExperimentalFeature> {
        if self.is_enabled(xp) {
            Ok(())
        } else {
            Err(MissingExperimentalFeature {
                feature: xp,
                reason: reason.map(str::to_owned),
            })
        }
    }
}

/// `MissingExperimentalFeature`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingExperimentalFeature {
    pub feature: Xp,
    pub reason: Option<String>,
}

impl fmt::Display for MissingExperimentalFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.feature.name();
        write!(f, "experimental Nix feature '{name}' is disabled")?;
        if let Some(reason) = &self.reason {
            write!(f, " ({reason})")?;
        }
        write!(
            f,
            "; add '--extra-experimental-features {name}' to enable it"
        )
    }
}

impl std::error::Error for MissingExperimentalFeature {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_names() {
        let xp = XpSettings::from_names(["nix-command", "blake3-hashes", "flakes"]);
        assert!(xp.is_enabled(Xp::Blake3Hashes));
        assert!(!xp.is_enabled(Xp::BakedDerivations));
        assert_eq!(xp, XpSettings::NONE.with(Xp::Blake3Hashes));
        assert_eq!(XpSettings::from_names([]), XpSettings::NONE);
    }

    #[test]
    fn missing_feature_messages_match_nix() {
        assert_eq!(
            XpSettings::NONE
                .require(Xp::Blake3Hashes, None)
                .unwrap_err()
                .to_string(),
            "experimental Nix feature 'blake3-hashes' is disabled; add '--extra-experimental-features blake3-hashes' to enable it"
        );
        assert_eq!(
            XpSettings::NONE
                .require(Xp::BakedDerivations, Some("'builtin:substitute' derivation"))
                .unwrap_err()
                .to_string(),
            "experimental Nix feature 'baked-derivations' is disabled ('builtin:substitute' derivation); add '--extra-experimental-features baked-derivations' to enable it"
        );
        assert!(XpSettings::NONE
            .with(Xp::BakedDerivations)
            .require(Xp::BakedDerivations, None)
            .is_ok());
    }
}
