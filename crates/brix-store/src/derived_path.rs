//! What can be asked of a store (`derived-path.hh`, `outputs-spec.hh`): a
//! store path as it is, or some outputs of a derivation. Two renderings
//! exist: `^` between the derivation and its outputs on the command line
//! (`DerivedPath::to_string`), `!` in the worker protocol
//! (`DerivedPath::to_string_legacy`). Only static derivations are
//! supported: the derivation is a store path, not itself a built output
//! (dynamic derivations).
//!
//! Output names are bytes, as in Nix. Those parsed from text
//! (`OutputsSpec::parse`) are valid store path names, and so ASCII, but a
//! derivation's own (a floating content-addressed output's, say) need not
//! be, and they reach derived paths through string contexts; both
//! renderings are therefore bytes.

use std::collections::BTreeSet;

use bstr::BString;

use crate::store_path::{validate_name, StoreDir, StorePath, StorePathError};

/// Which outputs of a derivation are meant (`OutputsSpec`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputsSpec {
    /// `*`
    All,
    /// `out,dev`
    Names(BTreeSet<BString>),
}

impl OutputsSpec {
    /// `OutputsSpec::parse`: `*`, or a comma-separated list of output
    /// names, each a valid store path name (`checkName`).
    pub fn parse<S: AsRef<[u8]> + ?Sized>(s: &S) -> Result<OutputsSpec, StorePathError> {
        let s = s.as_ref();
        if s == b"*" {
            return Ok(OutputsSpec::All);
        }
        let mut names = BTreeSet::new();
        for name in s.split(|&b| b == b',') {
            validate_name(name)?;
            names.insert(BString::from(name));
        }
        Ok(OutputsSpec::Names(names))
    }

    pub fn contains<S: AsRef<[u8]> + ?Sized>(&self, output: &S) -> bool {
        match self {
            OutputsSpec::All => true,
            OutputsSpec::Names(n) => n.contains(output.as_ref()),
        }
    }

    /// `OutputsSpec::union_`.
    pub fn union(&self, other: &OutputsSpec) -> OutputsSpec {
        match (self, other) {
            (OutputsSpec::All, _) | (_, OutputsSpec::All) => OutputsSpec::All,
            (OutputsSpec::Names(a), OutputsSpec::Names(b)) => {
                OutputsSpec::Names(a.union(b).cloned().collect())
            }
        }
    }

    /// `OutputsSpec::to_string`: `*` or the names joined by `,`, in order,
    /// as bytes.
    pub fn to_bytes(&self) -> BString {
        let mut out = Vec::new();
        self.write_to(&mut out);
        out.into()
    }

    /// [`OutputsSpec::to_bytes`] appended to `out`.
    fn write_to(&self, out: &mut Vec<u8>) {
        match self {
            OutputsSpec::All => out.push(b'*'),
            OutputsSpec::Names(names) => {
                for (i, name) in names.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(name);
                }
            }
        }
    }
}

/// The `^…` suffix of an installable, or its absence
/// (`ExtendedOutputsSpec`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ExtendedOutputsSpec {
    /// No `^`: the outputs the installable itself names.
    Default,
    /// `^out,dev` or `^*`.
    Explicit(OutputsSpec),
}

impl ExtendedOutputsSpec {
    /// `ExtendedOutputsSpec::parse`: the text before the last `^` and
    /// the outputs after it.
    pub fn parse(s: &str) -> Result<(&str, ExtendedOutputsSpec), InvalidExtendedOutputsSpec> {
        match s.rsplit_once('^') {
            None => Ok((s, ExtendedOutputsSpec::Default)),
            Some((prefix, outputs)) => match OutputsSpec::parse(outputs) {
                Ok(spec) => Ok((prefix, ExtendedOutputsSpec::Explicit(spec))),
                Err(_) => Err(InvalidExtendedOutputsSpec(s.to_owned())),
            },
        }
    }
}

/// Why [`ExtendedOutputsSpec::parse`] rejected its text: what follows the
/// last `^` is not an [`OutputsSpec`].
#[derive(Debug, thiserror::Error)]
#[error("invalid extended outputs specifier '{0}'")]
pub struct InvalidExtendedOutputsSpec(pub String);

/// Something to build or fetch (`DerivedPath`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DerivedPath {
    /// A store path as it is (`DerivedPath::Opaque`).
    Opaque(StorePath),
    /// Outputs of a derivation (`DerivedPath::Built`).
    Built {
        drv_path: StorePath,
        outputs: OutputsSpec,
    },
}

/// Why [`DerivedPath::parse_legacy`] rejected its text.
#[derive(Debug, thiserror::Error)]
pub enum DerivedPathError {
    #[error(transparent)]
    StorePath(#[from] StorePathError),
    /// `drv!out!out`: an output of a built derivation.
    #[error("dynamic derivation path '{0}' is not supported")]
    Dynamic(String),
}

impl DerivedPath {
    /// Every output of `drv_path`.
    pub fn all_outputs(drv_path: StorePath) -> DerivedPath {
        DerivedPath::Built {
            drv_path,
            outputs: OutputsSpec::All,
        }
    }

    /// The outputs `names` of `drv_path`.
    pub fn outputs<I: IntoIterator<Item = S>, S: Into<BString>>(
        drv_path: StorePath,
        names: I,
    ) -> DerivedPath {
        DerivedPath::Built {
            drv_path,
            outputs: OutputsSpec::Names(names.into_iter().map(Into::into).collect()),
        }
    }

    /// `DerivedPath::to_string`: `<store path>` or `<drv path>^<outputs>`,
    /// as the command line writes it. Bytes, as the output names are.
    pub fn display(&self, store_dir: &StoreDir) -> BString {
        self.render(store_dir, b'^')
    }

    /// `DerivedPath::to_string_legacy`: `<drv path>!<outputs>`, as the
    /// worker protocol sends it.
    pub fn display_legacy(&self, store_dir: &StoreDir) -> BString {
        self.render(store_dir, b'!')
    }

    fn render(&self, store_dir: &StoreDir, separator: u8) -> BString {
        match self {
            DerivedPath::Opaque(p) => store_dir.display(p).into(),
            DerivedPath::Built { drv_path, outputs } => {
                let mut out = store_dir.display(drv_path).into_bytes();
                out.push(separator);
                outputs.write_to(&mut out);
                out.into()
            }
        }
    }

    /// `DerivedPath::parseLegacy`: the worker protocol's `!` form. The
    /// output names are checked as `OutputsSpec::parse` checks them.
    pub fn parse_legacy(store_dir: &StoreDir, s: &str) -> Result<DerivedPath, DerivedPathError> {
        match s.rsplit_once('!') {
            None => Ok(DerivedPath::Opaque(store_dir.parse(s)?)),
            Some((drv, _)) if drv.contains('!') => Err(DerivedPathError::Dynamic(s.to_owned())),
            Some((drv, outputs)) => Ok(DerivedPath::Built {
                drv_path: store_dir.parse(drv)?,
                outputs: OutputsSpec::parse(outputs)?,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO: &str = "9xy8d7g1qf0n7hjbw4gj5zr3sx8yx3ra-hello-2.12.2";

    #[test]
    fn both_renderings_and_the_legacy_parse() {
        let dir = StoreDir::default();
        let drv = StorePath::new(format!("{HELLO}.drv")).unwrap();
        let all = DerivedPath::all_outputs(drv.clone());
        assert_eq!(all.display(&dir), format!("/nix/store/{HELLO}.drv^*"));
        assert_eq!(
            all.display_legacy(&dir),
            format!("/nix/store/{HELLO}.drv!*")
        );
        let some = DerivedPath::outputs(drv.clone(), ["out", "dev"]);
        assert_eq!(
            some.display(&dir),
            format!("/nix/store/{HELLO}.drv^dev,out")
        );
        assert_eq!(
            some.display_legacy(&dir),
            format!("/nix/store/{HELLO}.drv!dev,out")
        );
        let opaque = DerivedPath::Opaque(StorePath::new(HELLO).unwrap());
        assert_eq!(opaque.display(&dir), format!("/nix/store/{HELLO}"));
        for p in [&all, &some, &opaque] {
            let legacy = p.display_legacy(&dir);
            let legacy = std::str::from_utf8(&legacy).unwrap();
            assert_eq!(&DerivedPath::parse_legacy(&dir, legacy).unwrap(), p);
        }
        assert!(DerivedPath::parse_legacy(&dir, "/elsewhere/x").is_err());
        assert!(matches!(
            DerivedPath::parse_legacy(&dir, &format!("/nix/store/{HELLO}.drv!out!bin")),
            Err(DerivedPathError::Dynamic(_))
        ));
    }

    /// An output name from a derivation is bytes, and so is the derived
    /// path that names it (`nix eval` of `builtins.getContext` shows such
    /// a name for a floating output of a derivation whose outputs are not
    /// UTF-8).
    #[test]
    fn output_names_are_bytes() {
        let dir = StoreDir::default();
        let drv = StorePath::new(format!("{HELLO}.drv")).unwrap();
        let p = DerivedPath::outputs(drv, [b"a\xff".as_slice(), b"out".as_slice()]);
        assert_eq!(
            p.display(&dir),
            [format!("/nix/store/{HELLO}.drv^").as_bytes(), b"a\xff,out"].concat()
        );
        assert_eq!(
            p.display_legacy(&dir),
            [format!("/nix/store/{HELLO}.drv!").as_bytes(), b"a\xff,out"].concat()
        );
        let DerivedPath::Built { outputs, .. } = &p else {
            panic!("outputs of a derivation")
        };
        assert!(outputs.contains(b"a\xff".as_slice()) && outputs.contains("out"));
        // Parsed names are store path names, which such a name is not.
        assert_eq!(
            OutputsSpec::parse(b"out,a\xff".as_slice())
                .unwrap_err()
                .message_bytes(),
            b"name 'a\xff' contains illegal character '\xff'"
        );
    }

    /// `OutputsSpec::parse`: `*` or names that pass `checkName`; an empty
    /// list is one empty name.
    #[test]
    fn outputs_specs_parse_like_nix() {
        assert_eq!(OutputsSpec::parse("*").unwrap(), OutputsSpec::All);
        assert_eq!(
            OutputsSpec::parse("out,dev,out").unwrap().to_bytes(),
            "dev,out"
        );
        assert!(matches!(
            OutputsSpec::parse(""),
            Err(StorePathError::NameEmpty)
        ));
        assert!(OutputsSpec::parse("out,a b").is_err());
        let names = OutputsSpec::parse("out").unwrap();
        assert!(names.contains("out") && !names.contains("dev"));
        assert_eq!(names.union(&OutputsSpec::All), OutputsSpec::All);
        assert_eq!(
            names.union(&OutputsSpec::parse("dev").unwrap()).to_bytes(),
            "dev,out"
        );
        assert_eq!(OutputsSpec::All.to_bytes(), "*");
    }

    /// `ExtendedOutputsSpec::parse`: split at the last `^`, the outputs
    /// parsed as `OutputsSpec::parse` does.
    #[test]
    fn extended_outputs_specs_parse_like_nix() {
        assert_eq!(
            ExtendedOutputsSpec::parse("nixpkgs#hello").unwrap(),
            ("nixpkgs#hello", ExtendedOutputsSpec::Default)
        );
        assert_eq!(
            ExtendedOutputsSpec::parse("nixpkgs#hello^out,dev").unwrap(),
            (
                "nixpkgs#hello",
                ExtendedOutputsSpec::Explicit(OutputsSpec::parse("dev,out").unwrap())
            )
        );
        assert_eq!(
            ExtendedOutputsSpec::parse("a^b^*").unwrap(),
            ("a^b", ExtendedOutputsSpec::Explicit(OutputsSpec::All))
        );
        assert_eq!(
            ExtendedOutputsSpec::parse("nixpkgs#hello^")
                .unwrap_err()
                .to_string(),
            "invalid extended outputs specifier 'nixpkgs#hello^'"
        );
    }
}
