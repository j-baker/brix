//! Derivations: the `.drv` data model, its ATerm text form, and the
//! hashing that determines output paths.
//!
//! Mirrors `src/libstore/derivations.cc`. Two serialisations matter and
//! both must be byte-identical to Nix's:
//!
//! - [`Derivation::unparse`] produces the ATerm text that is written to
//!   the store as `<name>.drv`; the derivation's own store path is the
//!   text-hash of that string.
//! - [`hash_derivation_modulo`] hashes a *modified* unparse (output paths
//!   masked, input derivations replaced by their own hashes modulo) to get
//!   the value that output paths are derived from.
//!
//! Dynamic derivations (`DrvWithVersion`, nested input outputs) are an
//! experimental feature and not supported: parsing them is an error
//! (Nix's, for the missing feature, when it is not enabled).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::iter::Peekable;

use bstr::BString;

use crate::content_address::{
    ContentAddress, ContentAddressError, ContentAddressMethod, ContentAddressWithReferences,
};
use crate::experimental::{MissingExperimentalFeature, Xp, XpSettings};
use crate::hash::{base16, Hash, HashAlgo, HashError, HashFormat, Hasher};
use crate::store_path::{output_path_name, StoreDir, StorePath, StorePathError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DerivationError {
    #[error(transparent)]
    StorePath(#[from] StorePathError),
    #[error(transparent)]
    Hash(#[from] HashError),
    #[error(transparent)]
    ContentAddress(#[from] ContentAddressError),
    #[error(transparent)]
    MissingFeature(#[from] MissingExperimentalFeature),
    /// `FormatError` while parsing ATerm. Bytes: it can quote a string of
    /// the derivation (a path that is not absolute, an unknown version),
    /// which Nix shows as it is.
    #[error("{0}")]
    Format(BString),
    #[error("derivation does not start with 'Derive' or 'DrvWithVersion'")]
    BadHeader,
    #[error("dynamic derivations (ATerm format 'xp-dyn-drv') are not supported")]
    DynamicDerivations,
    #[error("can't mix derivation output types")]
    MixedOutputTypes,
    #[error("only one fixed output is allowed for now")]
    MultipleFixedOutputs,
    #[error("single fixed output must be named \"out\"")]
    FixedOutputNotOut,
    #[error("all floating outputs must use the same hash algorithm")]
    MixedFloatingAlgos,
    #[error("must have at least one output")]
    NoOutputs,
    #[error("no hash for output '{output}' of derivation '{drv}'")]
    MissingOutputHash { output: BString, drv: String },
    #[error("derivation has incorrect environment variable '{output}', should be '{expected}' but is actually '{actual}'")]
    WrongEnvVar {
        output: BString,
        expected: String,
        actual: BString,
    },
    #[error("derivation has incorrect output '{actual}', should be '{expected}'")]
    WrongOutputPath { actual: String, expected: String },
    #[error("derivation has incorrect output '{0}', should be deferred")]
    OutputShouldBeDeferred(String),
    /// A `builtin:substitute` derivation that is more than its outputs.
    #[error("'builtin:substitute' derivation must {0}")]
    BadSubstitute(&'static str),
    /// A derivation referenced as an input could not be found by the
    /// resolver passed to [`hash_derivation_modulo`].
    #[error("input derivation '{0}' is not known")]
    UnknownInputDerivation(String),
}

impl DerivationError {
    /// The message as Nix writes it: the `Display` text, but with the
    /// bytes of an output name, an environment variable, a hash, a store
    /// path or a string of the ATerm text that is not UTF-8 as they are
    /// (see [`StorePathError::message_bytes`], [`HashError::message_bytes`]).
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            DerivationError::StorePath(e) => e.message_bytes(),
            DerivationError::Format(message) => message.to_vec(),
            DerivationError::Hash(e) => e.message_bytes(),
            DerivationError::ContentAddress(e) => e.message_bytes(),
            DerivationError::MissingOutputHash { output, drv } => [
                b"no hash for output '".as_slice(),
                output.as_slice(),
                format!("' of derivation '{drv}'").as_bytes(),
            ]
            .concat(),
            DerivationError::WrongEnvVar {
                output,
                expected,
                actual,
            } => [
                b"derivation has incorrect environment variable '".as_slice(),
                output.as_slice(),
                format!("', should be '{expected}' but is actually '").as_bytes(),
                actual.as_slice(),
                b"'",
            ]
            .concat(),
            other => other.to_string().into_bytes(),
        }
    }
}

/// `DerivationOutput`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DerivationOutput {
    /// The usual case: the path was computed from the inputs.
    InputAddressed(StorePath),
    /// A fixed-output derivation: the hash is known up front.
    CAFixed(ContentAddress),
    /// `__contentAddressed`: the path is known only after the build.
    CAFloating {
        method: ContentAddressMethod,
        hash_algo: HashAlgo,
    },
    /// An input-addressed output whose path cannot be computed yet
    /// because an input is floating.
    Deferred,
    /// `__impure` (experimental).
    Impure {
        method: ContentAddressMethod,
        hash_algo: HashAlgo,
    },
}

impl DerivationOutput {
    /// `DerivationOutput::path`: the output path if it is known. The
    /// output name is bytes, as Nix's are.
    pub fn path(
        &self,
        store: &StoreDir,
        drv_name: &str,
        output_name: &[u8],
    ) -> Result<Option<StorePath>, DerivationError> {
        match self {
            DerivationOutput::InputAddressed(p) => Ok(Some(p.clone())),
            DerivationOutput::CAFixed(ca) => Ok(Some(store.make_fixed_output_path_from_ca(
                &output_path_name(drv_name, output_name),
                &ContentAddressWithReferences::without_refs(ca),
            )?)),
            _ => Ok(None),
        }
    }
}

/// `DerivationType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivationType {
    InputAddressed { deferred: bool },
    ContentAddressed { sandboxed: bool, fixed: bool },
    Impure,
    Substituted,
}

impl DerivationType {
    /// `isFixed`.
    pub fn is_fixed(self) -> bool {
        matches!(self, DerivationType::ContentAddressed { fixed: true, .. })
    }
}

/// `Derivation` (and `BasicDerivation`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Derivation {
    /// The derivation name, i.e. the store path name without `.drv`.
    pub name: String,
    /// By output name. Output names are bytes, as in Nix: one that is not
    /// a valid store path name (not ASCII, say) fails where an output path
    /// is made from it, but a floating content-addressed or impure output
    /// keeps it, and so do the string contexts and derived paths that
    /// name the output.
    pub outputs: BTreeMap<BString, DerivationOutput>,
    /// Input store paths that are not derivations.
    pub input_srcs: BTreeSet<StorePath>,
    /// Input derivations and which of their outputs (bytes) are used.
    pub input_drvs: BTreeMap<StorePath, BTreeSet<BString>>,
    /// The platform, builder, arguments and environment are bytes, as
    /// every Nix string is: an attribute of `derivation` need not be
    /// UTF-8, and its bytes are what the `.drv` file and its hash hold.
    pub platform: BString,
    pub builder: BString,
    pub args: Vec<BString>,
    /// Sorted by name in byte order, as Nix's `std::map`. With
    /// structured attrs the JSON lives here under `__json`, exactly as
    /// it is serialised.
    pub env: BTreeMap<BString, BString>,
}

/// The env var that carries structured attrs (`StructuredAttrs::envVarName`).
pub const STRUCTURED_ATTRS_ENV: &str = "__json";

/// The builder of a derivation whose outputs are substituted rather than
/// built (`baked-derivations`).
const SUBSTITUTE_BUILDER: &str = "builtin:substitute";

impl Derivation {
    /// `outputNames`.
    pub fn output_names(&self) -> BTreeSet<BString> {
        self.outputs.keys().cloned().collect()
    }

    /// `outputsAndOptPaths`, paths only.
    pub fn output_paths(
        &self,
        store: &StoreDir,
    ) -> Result<BTreeMap<BString, Option<StorePath>>, DerivationError> {
        self.outputs
            .iter()
            .map(|(n, o)| Ok((n.clone(), o.path(store, &self.name, n)?)))
            .collect()
    }

    /// `type()`: classify the derivation by its outputs, with Nix's
    /// consistency errors. A `builtin:substitute` derivation needs the
    /// `baked-derivations` feature and input-addressed outputs.
    pub fn type_(&self, xp: XpSettings) -> Result<DerivationType, DerivationError> {
        let mut ty: Option<DerivationType> = None;
        let mut floating_algo: Option<HashAlgo> = None;
        let mut decide = |new_ty: DerivationType| -> Result<(), DerivationError> {
            match ty {
                None => ty = Some(new_ty),
                Some(t) if t != new_ty => return Err(DerivationError::MixedOutputTypes),
                Some(DerivationType::ContentAddressed {
                    sandboxed: false,
                    fixed: true,
                }) => return Err(DerivationError::MultipleFixedOutputs),
                Some(_) => {}
            }
            Ok(())
        };
        for (name, output) in &self.outputs {
            match output {
                DerivationOutput::InputAddressed(_) => {
                    decide(DerivationType::InputAddressed { deferred: false })?
                }
                DerivationOutput::CAFixed(_) => {
                    decide(DerivationType::ContentAddressed {
                        sandboxed: false,
                        fixed: true,
                    })?;
                    if name != "out" {
                        return Err(DerivationError::FixedOutputNotOut);
                    }
                }
                DerivationOutput::CAFloating { hash_algo, .. } => {
                    decide(DerivationType::ContentAddressed {
                        sandboxed: true,
                        fixed: false,
                    })?;
                    match floating_algo {
                        None => floating_algo = Some(*hash_algo),
                        Some(a) if a != *hash_algo => {
                            return Err(DerivationError::MixedFloatingAlgos)
                        }
                        Some(_) => {}
                    }
                }
                DerivationOutput::Deferred => {
                    decide(DerivationType::InputAddressed { deferred: true })?
                }
                DerivationOutput::Impure { .. } => decide(DerivationType::Impure)?,
            }
        }
        let ty = ty.ok_or(DerivationError::NoOutputs)?;
        if self.builder == SUBSTITUTE_BUILDER {
            xp.require(
                Xp::BakedDerivations,
                Some("'builtin:substitute' derivation"),
            )?;
            if ty != (DerivationType::InputAddressed { deferred: false }) {
                return Err(DerivationError::BadSubstitute(
                    "have input-addressed outputs",
                ));
            }
            return Ok(DerivationType::Substituted);
        }
        Ok(ty)
    }

    /// The store paths this derivation refers to: its input sources and
    /// input derivations (not its outputs).
    pub fn references(&self) -> BTreeSet<StorePath> {
        let mut refs = self.input_srcs.clone();
        refs.extend(self.input_drvs.keys().cloned());
        refs
    }

    /// `unparse`: the ATerm text, bytes like the strings in it. With
    /// `mask_outputs`, output paths and the env vars named after outputs
    /// are blanked. (Nix's third argument, the input derivations' hashes
    /// modulo, is what [`hash_derivation_modulo`] writes itself.)
    pub fn unparse(
        &self,
        store: &StoreDir,
        mask_outputs: bool,
    ) -> Result<Vec<u8>, DerivationError> {
        let mut s = Vec::new();
        self.write_aterm_with(&mut s, store, mask_outputs, AtermInputs::Paths)?;
        Ok(s)
    }

    /// A generous estimate of the ATerm text length, so that one
    /// reservation covers the whole serialisation.
    fn aterm_len_hint(&self, store: &StoreDir) -> usize {
        let path_len = store.as_str().len() + 1 + 32 + 1 + self.name.len() + 8;
        let outputs: usize = self.outputs.keys().map(|k| k.len() + path_len + 96).sum();
        let inputs: usize = self
            .input_drvs
            .iter()
            .map(|(k, v)| k.as_str().len() + 16 + v.iter().map(|o| o.len() + 3).sum::<usize>())
            .sum::<usize>()
            + self
                .input_srcs
                .iter()
                .map(|p| p.as_str().len() + 16)
                .sum::<usize>();
        let env: usize = self.env.iter().map(|(k, v)| k.len() + v.len() + 8).sum();
        let args: usize = self.args.iter().map(|a| a.len() + 3).sum();
        64 + outputs + inputs + env + args + self.builder.len() + self.platform.len()
    }

    /// `unparse` appended to `s`, which callers reuse across
    /// serialisations, with the input derivations listed as `inputs` says.
    fn write_aterm_with(
        &self,
        s: &mut Vec<u8>,
        store: &StoreDir,
        mask_outputs: bool,
        inputs: AtermInputs<'_>,
    ) -> Result<(), DerivationError> {
        s.reserve(self.aterm_len_hint(store));
        self.write_outputs(s, store, mask_outputs)?;
        self.write_input_drvs(s, store, inputs);
        self.write_srcs_to_args(s, store);
        let mut outputs = self.outputs.keys().peekable();
        for (i, (k, v)) in self.env.iter().enumerate() {
            if i > 0 {
                s.push(b',');
            }
            s.push(b'(');
            print_string(s, k);
            s.push(b',');
            if mask_outputs && is_output(&mut outputs, k) {
                s.extend_from_slice(b"\"\"");
            } else {
                print_string(s, v);
            }
            s.push(b')');
        }
        s.extend_from_slice(b"])");
        Ok(())
    }

    /// The ATerm up to the input derivations: `Derive([<outputs>],[`.
    fn write_outputs(
        &self,
        s: &mut Vec<u8>,
        store: &StoreDir,
        mask_outputs: bool,
    ) -> Result<(), DerivationError> {
        s.extend_from_slice(b"Derive([");
        for (i, (name, output)) in self.outputs.iter().enumerate() {
            if i > 0 {
                s.push(b',');
            }
            // `(name,path,algo,hash)`, all printed unescaped.
            s.extend_from_slice(b"(\"");
            s.extend_from_slice(name);
            s.extend_from_slice(b"\",\"");
            match output {
                DerivationOutput::InputAddressed(p) if !mask_outputs => {
                    push_store_path(s, store, p)
                }
                DerivationOutput::CAFixed(_) if !mask_outputs => push_store_path(
                    s,
                    store,
                    &output
                        .path(store, &self.name, name)?
                        .expect("fixed outputs have a path"),
                ),
                _ => {}
            }
            s.extend_from_slice(b"\",\"");
            match output {
                DerivationOutput::InputAddressed(_) | DerivationOutput::Deferred => {
                    s.extend_from_slice(b"\",\"")
                }
                DerivationOutput::CAFixed(ca) => {
                    s.extend_from_slice(ca.method.prefix().as_bytes());
                    s.extend_from_slice(ca.hash.algo().name().as_bytes());
                    s.extend_from_slice(b"\",\"");
                    push_base16(s, ca.hash.as_bytes());
                }
                DerivationOutput::CAFloating { method, hash_algo } => {
                    s.extend_from_slice(method.prefix().as_bytes());
                    s.extend_from_slice(hash_algo.name().as_bytes());
                    s.extend_from_slice(b"\",\"");
                }
                DerivationOutput::Impure { method, hash_algo } => {
                    s.extend_from_slice(method.prefix().as_bytes());
                    s.extend_from_slice(hash_algo.name().as_bytes());
                    s.extend_from_slice(b"\",\"impure");
                }
            }
            s.extend_from_slice(b"\")");
        }
        s.extend_from_slice(b"],[");
        Ok(())
    }

    /// The input derivations' entries, without the surrounding brackets.
    fn write_input_drvs(&self, s: &mut Vec<u8>, store: &StoreDir, inputs: AtermInputs<'_>) {
        match inputs {
            AtermInputs::Paths => {
                for (i, (drv, outputs)) in self.input_drvs.iter().enumerate() {
                    if i > 0 {
                        s.push(b',');
                    }
                    s.push(b'(');
                    print_store_path(s, store, drv);
                    s.push(b',');
                    print_unquoted_list(s, outputs.iter().map(|o| o.as_slice()));
                    s.push(b')');
                }
            }
            AtermInputs::Hashes(mut rest) => {
                let mut first = true;
                while let Some(&(hash, _)) = rest.first() {
                    let n = rest.iter().take_while(|(h, _)| *h == hash).count();
                    let (group, tail) = rest.split_at(n);
                    rest = tail;
                    if !first {
                        s.push(b',');
                    }
                    first = false;
                    s.extend_from_slice(b"(\"");
                    push_base16(s, hash.as_bytes());
                    s.extend_from_slice(b"\",");
                    print_unquoted_list(s, group.iter().filter_map(|&(_, name)| name));
                    s.push(b')');
                }
            }
        }
    }

    /// The ATerm from the end of the input derivations to the start of
    /// the environment's entries:
    /// `],[<input srcs>],<platform>,<builder>,[<args>],[`.
    fn write_srcs_to_args(&self, s: &mut Vec<u8>, store: &StoreDir) {
        s.extend_from_slice(b"],[");
        for (i, p) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                s.push(b',');
            }
            print_store_path(s, store, p);
        }
        s.extend_from_slice(b"],");
        print_unquoted(s, &self.platform);
        s.push(b',');
        print_string(s, &self.builder);
        s.push(b',');
        print_string_list(s, self.args.iter().map(|a| a.as_slice()));
        s.extend_from_slice(b",[");
    }

    /// The ATerm text as written to the store (`unparse(store, false)`).
    pub fn to_aterm(&self, store: &StoreDir) -> Result<Vec<u8>, DerivationError> {
        self.unparse(store, false)
    }

    /// `computeStorePath` / `writeDerivation`: the `.drv` store path, a
    /// text object referring to the inputs.
    pub fn store_path(&self, store: &StoreDir) -> Result<StorePath, DerivationError> {
        let mut scratch = Vec::new();
        self.store_path_with(store, &mut scratch)
    }

    /// [`Derivation::store_path`] serialising into `scratch` (cleared
    /// first), which afterwards holds the ATerm text that was hashed.
    pub fn store_path_with(
        &self,
        store: &StoreDir,
        scratch: &mut Vec<u8>,
    ) -> Result<StorePath, DerivationError> {
        scratch.clear();
        self.write_aterm_with(scratch, store, false, AtermInputs::Paths)?;
        let hash = Hash::of(HashAlgo::Sha256, scratch);
        self.text_path(store, &hash)
    }

    /// The `.drv` path of this derivation given the hash of its ATerm
    /// text: a text object named `<name>.drv` referring to the input
    /// sources and input derivations.
    fn text_path(&self, store: &StoreDir, hash: &Hash) -> Result<StorePath, DerivationError> {
        // Both reference sets are sorted; `makeType` wants them merged in
        // order, without duplicates.
        let mut refs: Vec<&StorePath> =
            Vec::with_capacity(self.input_srcs.len() + self.input_drvs.len());
        let mut srcs = self.input_srcs.iter().peekable();
        let mut drvs = self.input_drvs.keys().peekable();
        loop {
            let next = match (srcs.peek(), drvs.peek()) {
                (Some(a), Some(b)) => match a.cmp(b) {
                    std::cmp::Ordering::Less => srcs.next(),
                    std::cmp::Ordering::Greater => drvs.next(),
                    std::cmp::Ordering::Equal => {
                        drvs.next();
                        srcs.next()
                    }
                },
                (Some(_), None) => srcs.next(),
                (None, _) => drvs.next(),
            };
            match next {
                Some(p) => refs.push(p),
                None => break,
            }
        }
        let mut name = String::with_capacity(self.name.len() + StorePath::DRV_EXTENSION.len());
        name.push_str(&self.name);
        name.push_str(StorePath::DRV_EXTENSION);
        Ok(store.make_text_path_from(&name, hash, refs.into_iter())?)
    }

    /// `parseDerivation`: parse the ATerm text of `<name>.drv`.
    ///
    /// The text is bytes, as in Nix, and so is every string in it: the
    /// output names, the store paths (checked for a leading `/`, then
    /// parsed with `parseStorePath`'s errors, whatever their bytes), the
    /// hashes and their algorithms, the platform, builder, arguments and
    /// environment. `xp` decides whether BLAKE3 output hashes and the
    /// experimental output types are accepted.
    pub fn parse(
        store: &StoreDir,
        text: impl AsRef<[u8]>,
        name: &str,
        xp: XpSettings,
    ) -> Result<Derivation, DerivationError> {
        Self::parse_bytes(store, text.as_ref(), name, xp)
    }

    fn parse_bytes(
        store: &StoreDir,
        text: &[u8],
        name: &str,
        xp: XpSettings,
    ) -> Result<Derivation, DerivationError> {
        let mut p = AtermParser { rest: text };
        let mut drv = Derivation {
            name: name.to_owned(),
            ..Default::default()
        };
        p.expect("D")?;
        match p.rest.first() {
            Some(b'e') => p.expect("erive(")?,
            Some(b'r') => {
                p.expect("rvWithVersion(")?;
                let version = p.string_bytes()?;
                if version != b"xp-dyn-drv" {
                    return Err(DerivationError::Format(
                        [
                            b"Unknown derivation ATerm format version '".as_slice(),
                            &version,
                            b"'",
                        ]
                        .concat()
                        .into(),
                    ));
                }
                xp.require(
                    Xp::DynamicDerivations,
                    Some(&format!(
                        "derivation '{name}', ATerm format version 'xp-dyn-drv'"
                    )),
                )?;
                return Err(DerivationError::DynamicDerivations);
            }
            _ => return Err(DerivationError::BadHeader),
        }
        p.expect("[")?;
        while !p.end_of_list() {
            p.expect("(")?;
            let id = p.string_bytes()?;
            p.expect(",")?;
            let path = p.string_bytes()?;
            p.expect(",")?;
            let algo = p.string_bytes()?;
            p.expect(",")?;
            let hash = p.string_bytes()?;
            p.expect(")")?;
            let output = parse_output(store, &path, &algo, &hash, xp)?;
            // `std::map::emplace`: a repeated output keeps its first
            // definition.
            drv.outputs.entry(id.into()).or_insert(output);
        }
        p.expect(",[")?;
        while !p.end_of_list() {
            p.expect("(")?;
            let drv_path = p.path()?;
            p.expect(",")?;
            // The traditional format has no nested (dynamic) entries.
            let outputs = p.output_names()?;
            p.expect(")")?;
            drv.input_drvs.insert(store.parse(&drv_path)?, outputs);
        }
        p.expect(",")?;
        for src in p.paths()? {
            drv.input_srcs.insert(store.parse(&src)?);
        }
        p.expect(",")?;
        drv.platform = p.string_bytes()?.into();
        p.expect(",")?;
        drv.builder = p.string_bytes()?.into();
        p.expect(",[")?;
        while !p.end_of_list() {
            drv.args.push(p.string_bytes()?.into());
        }
        p.expect(",[")?;
        while !p.end_of_list() {
            p.expect("(")?;
            let k = p.string_bytes()?;
            p.expect(",")?;
            let v = p.string_bytes()?;
            p.expect(")")?;
            drv.env.insert(k.into(), v.into());
        }
        p.expect(")")?;
        Ok(drv)
    }

    /// `fillInOutputPaths`: turn `Deferred` outputs into `InputAddressed`
    /// ones by hashing the derivation modulo its inputs, and set the
    /// matching env vars; validate outputs that already have paths.
    /// `resolve` supplies the hash modulo of each input derivation.
    pub fn fill_in_output_paths(
        &mut self,
        store: &StoreDir,
        xp: XpSettings,
        resolve: &mut dyn FnMut(&StorePath) -> Result<DrvHashModulo, DerivationError>,
    ) -> Result<(), DerivationError> {
        if self.builder == SUBSTITUTE_BUILDER {
            // `processDerivationOutputPaths`: a substitution is a builtin
            // with nothing but its outputs.
            let unmet = if self.platform != "builtin" {
                Some("be a builtin")
            } else if !self.args.is_empty() {
                Some("have no arguments")
            } else if !self.env.is_empty() {
                Some("have no environment variables")
            } else if !self.input_srcs.is_empty() {
                Some("have no inputs")
            } else {
                None
            };
            if let Some(unmet) = unmet {
                return Err(DerivationError::BadSubstitute(unmet));
            }
            self.type_(xp)?;
            return Ok(());
        }
        let mut hash_modulo: Option<DrvHashModulo> = None;
        let output_names: Vec<BString> = self.outputs.keys().cloned().collect();
        for output_name in output_names {
            let output = self.outputs[&output_name].clone();
            let needs_hash = matches!(
                output,
                DerivationOutput::InputAddressed(_) | DerivationOutput::Deferred
            );
            if needs_hash && hash_modulo.is_none() {
                hash_modulo = Some(hash_derivation_modulo(store, self, true, xp, resolve)?);
            }
            match output {
                DerivationOutput::InputAddressed(ref actual) => match hash_modulo.as_ref().unwrap()
                {
                    DrvHashModulo::DrvHash(h) => {
                        let expected = store.make_output_path(&output_name, h, &self.name)?;
                        if *actual != expected {
                            return Err(DerivationError::WrongOutputPath {
                                actual: store.display(actual),
                                expected: store.display(&expected),
                            });
                        }
                        self.env_has_right_path(store, &output_name, &expected)?;
                    }
                    DrvHashModulo::Deferred => {
                        return Err(DerivationError::OutputShouldBeDeferred(
                            store.display(actual),
                        ))
                    }
                    DrvHashModulo::CaOutputHashes(_) => {
                        unreachable!("input-addressed output with CA hashes")
                    }
                },
                DerivationOutput::Deferred => match hash_modulo.as_ref().unwrap() {
                    DrvHashModulo::DrvHash(h) => {
                        let path = store.make_output_path(&output_name, h, &self.name)?;
                        self.outputs.insert(
                            output_name.clone(),
                            DerivationOutput::InputAddressed(path.clone()),
                        );
                        self.env_has_right_path(store, &output_name, &path)?;
                    }
                    DrvHashModulo::Deferred => {}
                    DrvHashModulo::CaOutputHashes(_) => {
                        unreachable!("deferred output with CA hashes")
                    }
                },
                DerivationOutput::CAFixed(_) => {
                    let path = output
                        .path(store, &self.name, &output_name)?
                        .expect("fixed outputs have a path");
                    self.env_has_right_path(store, &output_name, &path)?;
                }
                _ => {}
            }
        }
        self.type_(xp)?;
        Ok(())
    }

    /// `envHasRightPath` in fill-in mode: a missing or empty env var is
    /// set; a wrong one is an error.
    fn env_has_right_path(
        &mut self,
        store: &StoreDir,
        output_name: &[u8],
        actual: &StorePath,
    ) -> Result<(), DerivationError> {
        let printed = store.display(actual);
        match self.env.get_mut(output_name) {
            None => {
                self.env.insert(output_name.into(), printed.into());
            }
            Some(v) if v.is_empty() => *v = printed.into(),
            Some(v) if *v != printed => {
                return Err(DerivationError::WrongEnvVar {
                    output: output_name.into(),
                    expected: printed,
                    actual: v.clone(),
                })
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// [`Derivation::fill_in_output_paths`] followed by
    /// [`Derivation::store_path_with`] for the usual product of
    /// `derivationStrict`: an input-addressed derivation whose outputs are
    /// all `Deferred`, each with an empty env var of its name.
    ///
    /// Those steps serialise the derivation twice (once with the outputs
    /// masked, to hash it modulo its inputs, and once in full, for its
    /// own path), and memoising its hash modulo afterwards serialises it
    /// a third time. The three texts differ only in the outputs, the
    /// input derivations (store paths or hashes modulo) and the output
    /// env vars, so the derivation is written out once, with those parts
    /// side by side and the output env vars left as gaps, and each text
    /// is hashed from its pieces. The returned [`FilledLayout`] hashes
    /// the third text on demand from `scratch`, which must not be
    /// changed in between.
    ///
    /// Returns `None`, with the derivation untouched, when the
    /// derivation is not of that shape or an input is deferred; the
    /// general functions handle those.
    pub(crate) fn fill_in_deferred(
        &mut self,
        store: &StoreDir,
        resolve: &mut dyn FnMut(&StorePath) -> Result<DrvHashModulo, DerivationError>,
        scratch: &mut Vec<u8>,
    ) -> Result<Option<(StorePath, FilledLayout)>, DerivationError> {
        if self.builder == SUBSTITUTE_BUILDER || self.outputs.is_empty() {
            return Ok(None);
        }
        for (name, output) in &self.outputs {
            let empty_env = self.env.get(name.as_slice()).is_some_and(|v| v.is_empty());
            if !matches!(output, DerivationOutput::Deferred) || !empty_env {
                return Ok(None);
            }
        }
        // All outputs deferred: `type_()` is input-addressed (deferred),
        // so `hashDerivationModulo` goes straight to the inputs.
        let Some(inputs) = modulo_inputs(self, resolve)? else {
            return Ok(None);
        };

        // The masked outputs (a deferred output prints the same masked or
        // not), both forms of the input derivations, then everything after
        // them with a gap where each output env var's value goes.
        scratch.clear();
        scratch.reserve(2 * self.aterm_len_hint(store));
        self.write_outputs(scratch, store, true)?;
        let masked_outputs = 0..scratch.len();
        self.write_input_drvs(scratch, store, AtermInputs::Hashes(&inputs));
        let hashed_inputs = masked_outputs.end..scratch.len();
        self.write_input_drvs(scratch, store, AtermInputs::Paths);
        let path_inputs = hashed_inputs.end..scratch.len();
        drop(inputs);
        self.write_srcs_to_args(scratch, store);
        let mut gaps = Vec::with_capacity(self.outputs.len());
        let mut outputs = self.outputs.keys().peekable();
        for (i, (k, v)) in self.env.iter().enumerate() {
            if i > 0 {
                scratch.push(b',');
            }
            scratch.push(b'(');
            print_string(scratch, k);
            scratch.push(b',');
            if is_output(&mut outputs, k) {
                gaps.push(scratch.len());
            } else {
                print_string(scratch, v);
            }
            scratch.push(b')');
        }
        scratch.extend_from_slice(b"])");
        let rest = path_inputs.end..scratch.len();
        // Every output has an env var, and both maps are sorted by name,
        // so gap `i` belongs to output `i`.
        debug_assert_eq!(gaps.len(), self.outputs.len());

        let masked_hash = hash_pieces(
            scratch,
            &[masked_outputs, hashed_inputs.clone()],
            &rest,
            &gaps,
            |_| b"\"\"",
        );

        // `fillInOutputPaths`, output by output.
        let mut paths = Vec::with_capacity(self.outputs.len());
        for name in self.outputs.keys() {
            paths.push(store.make_output_path(name, &masked_hash, &self.name)?);
        }
        let Derivation { outputs, env, .. } = self;
        for ((name, output), path) in outputs.iter_mut().zip(paths) {
            *env.get_mut(name.as_slice()).expect("checked above") = store.display(&path).into();
            *output = DerivationOutput::InputAddressed(path);
        }

        // The full outputs and the quoted output env vars, appended.
        let start = scratch.len();
        self.write_outputs(scratch, store, false)?;
        let full_outputs = start..scratch.len();
        let mut values = Vec::with_capacity(self.outputs.len());
        for name in self.outputs.keys() {
            let start = scratch.len();
            print_string(scratch, &self.env[name.as_slice()]);
            values.push(start..scratch.len());
        }
        let layout = FilledLayout {
            full_outputs,
            hashed_inputs,
            rest,
            gaps,
            values,
        };
        let text: &[u8] = scratch;
        let text_hash = hash_pieces(
            text,
            &[layout.full_outputs.clone(), path_inputs],
            &layout.rest,
            &layout.gaps,
            |i| &text[layout.values[i].clone()],
        );
        let drv_path = self.text_path(store, &text_hash)?;
        Ok(Some((drv_path, layout)))
    }
}

/// Where [`Derivation::fill_in_deferred`] left the pieces of a filled-in
/// derivation's ATerm in its scratch buffer.
pub(crate) struct FilledLayout {
    full_outputs: std::ops::Range<usize>,
    hashed_inputs: std::ops::Range<usize>,
    /// From the end of the input derivations to the end of the text,
    /// without the output env vars' values.
    rest: std::ops::Range<usize>,
    /// Offsets in `rest` where the output env vars' values belong.
    gaps: Vec<usize>,
    /// The values, quoted.
    values: Vec<std::ops::Range<usize>>,
}

impl FilledLayout {
    /// `hashDerivationModulo(drv, false)` of the filled-in derivation:
    /// its full text with the input derivations replaced by their hashes.
    pub(crate) fn hash_modulo(&self, scratch: &[u8]) -> Hash {
        hash_pieces(
            scratch,
            &[self.full_outputs.clone(), self.hashed_inputs.clone()],
            &self.rest,
            &self.gaps,
            |i| &scratch[self.values[i].clone()],
        )
    }
}

/// sha256 of the ranges `head` of `bytes`, then of `rest` with `fill(i)`
/// inserted at the `i`th of the (ascending) offsets `gaps`.
fn hash_pieces<'t>(
    bytes: &'t [u8],
    head: &[std::ops::Range<usize>],
    rest: &std::ops::Range<usize>,
    gaps: &[usize],
    fill: impl Fn(usize) -> &'t [u8],
) -> Hash {
    let mut hasher = Hasher::new(HashAlgo::Sha256);
    for range in head {
        hasher.update(&bytes[range.clone()]);
    }
    let mut pos = rest.start;
    for (i, &gap) in gaps.iter().enumerate() {
        hasher.update(&bytes[pos..gap]);
        hasher.update(fill(i));
        pos = gap;
    }
    hasher.update(&bytes[pos..rest.end]);
    hasher.finish().0
}

/// `parseDerivationOutput` from the three strings of an output tuple: a
/// store path, the algorithm and the hash, all bytes as Nix parses them.
fn parse_output(
    store: &StoreDir,
    path: &[u8],
    hash_algo: &[u8],
    hash: &[u8],
    xp: XpSettings,
) -> Result<DerivationOutput, DerivationError> {
    if hash_algo.is_empty() {
        if path.is_empty() {
            return Ok(DerivationOutput::Deferred);
        }
        validate_path(path)?;
        return Ok(DerivationOutput::InputAddressed(store.parse(path)?));
    }
    let (method, algo_name) = ContentAddressMethod::parse_prefix(hash_algo, xp)?;
    if method == ContentAddressMethod::Text {
        xp.require(
            Xp::DynamicDerivations,
            Some("text-hashed derivation output"),
        )?;
    }
    let algo = HashAlgo::parse(algo_name, xp)?;
    if hash == b"impure" {
        xp.require(Xp::ImpureDerivations, None)?;
        if !path.is_empty() {
            return Err(DerivationError::Format(
                "impure derivation output should not specify output path".into(),
            ));
        }
        Ok(DerivationOutput::Impure {
            method,
            hash_algo: algo,
        })
    } else if !hash.is_empty() {
        validate_path(path)?;
        Ok(DerivationOutput::CAFixed(ContentAddress {
            method,
            hash: Hash::parse_non_sri_unprefixed(hash, algo, xp)?,
        }))
    } else {
        xp.require(Xp::CaDerivations, None)?;
        if !path.is_empty() {
            return Err(DerivationError::Format(
                "content-addressing derivation output should not specify output path".into(),
            ));
        }
        Ok(DerivationOutput::CAFloating {
            method,
            hash_algo: algo,
        })
    }
}

/// `validatePath`: a path in a derivation must start with `/`; nothing
/// else is checked here. The message quotes it as it is.
fn validate_path(s: &[u8]) -> Result<(), DerivationError> {
    if s.first() != Some(&b'/') {
        return Err(DerivationError::Format(
            [b"bad path '".as_slice(), s, b"' in derivation"]
                .concat()
                .into(),
        ));
    }
    Ok(())
}

/// `printString`: quoted with `\"`, `\\`, `\n`, `\r`, `\t` escapes.
///
/// Derivation environments are mostly long strings with few escapes, and
/// every derivation is serialised several times, so the text is scanned a
/// word at a time for bytes that may need escaping (see
/// [`find_escape_candidate`]) and the runs between them are appended
/// whole. A byte-at-a-time loop was 4% of the instructions of a nixpkgs
/// evaluation.
fn print_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.reserve(bytes.len() + 2);
    out.push(b'"');
    // `start`: the first byte not yet appended; `from`: where to resume
    // scanning.
    let mut start = 0;
    let mut from = 0;
    while let Some(i) = find_escape_candidate(bytes, from) {
        from = i + 1;
        let escaped: &[u8] = match bytes[i] {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            // Another control byte: printed as it is.
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(escaped);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Whether `printString` may have to escape `b`: `"`, `\`, or a control
/// byte below 0x0e (which includes `\t`, `\n` and `\r`).
fn is_escape_candidate(b: u8) -> bool {
    b == b'"' || b == b'\\' || b < 0x0e
}

/// The index of the first escape candidate (see [`is_escape_candidate`])
/// in `bytes[from..]`, examining eight bytes at a time.
///
/// Per word, the classic "has a zero byte" test, `(x - 0x01…) & !x &
/// 0x80…`, finds bytes equal to `"` and `\` (after xor-ing them to zero),
/// and `(w - 0x0e…) & !w & 0x80…` finds bytes below 0x0e. Either test can
/// also flag bytes *above* a true match (through the borrow), never below
/// one, so the lowest flagged byte of the three is exactly the first
/// candidate.
fn find_escape_candidate(bytes: &[u8], from: usize) -> Option<usize> {
    const ONES: u64 = u64::from_le_bytes([0x01; 8]);
    const HIGH: u64 = u64::from_le_bytes([0x80; 8]);
    const QUOTE: u64 = ONES * b'"' as u64;
    const BACKSLASH: u64 = ONES * b'\\' as u64;
    const CONTROL: u64 = ONES * 0x0e;
    let rest = &bytes[from..];
    let mut words = rest.chunks_exact(8);
    let mut offset = from;
    for word in &mut words {
        let w = u64::from_le_bytes(word.try_into().expect("eight bytes"));
        let q = w ^ QUOTE;
        let b = w ^ BACKSLASH;
        let flagged = ((q.wrapping_sub(ONES) & !q)
            | (b.wrapping_sub(ONES) & !b)
            | (w.wrapping_sub(CONTROL) & !w))
            & HIGH;
        if flagged != 0 {
            return Some(offset + (flagged.trailing_zeros() / 8) as usize);
        }
        offset += 8;
    }
    words
        .remainder()
        .iter()
        .position(|&b| is_escape_candidate(b))
        .map(|p| offset + p)
}

/// The input derivations an ATerm serialisation lists.
#[derive(Clone, Copy)]
enum AtermInputs<'x> {
    /// The derivation's own input derivations, by store path.
    Paths,
    /// The input derivations as [`hash_derivation_modulo`] rewrites them
    /// (see [`modulo_inputs`]).
    Hashes(&'x [ModuloInput<'x>]),
}

/// One (hash modulo, output name) pair of an input derivation, as
/// [`hash_derivation_modulo`] rewrites the inputs. The name is `None` for
/// an input whose set of outputs is empty, which still gets an entry.
type ModuloInput<'x> = (Hash, Option<&'x [u8]>);

/// `printUnquotedString` of a store path shown in `store`, without
/// building the displayed path first.
fn print_store_path(out: &mut Vec<u8>, store: &StoreDir, path: &StorePath) {
    out.push(b'"');
    push_store_path(out, store, path);
    out.push(b'"');
}

/// A store path shown in `store`, appended as it is.
fn push_store_path(out: &mut Vec<u8>, store: &StoreDir, path: &StorePath) {
    out.extend_from_slice(store.as_str().as_bytes());
    out.push(b'/');
    out.extend_from_slice(path.as_str().as_bytes());
}

/// A digest in base16, appended.
fn push_base16(out: &mut Vec<u8>, digest: &[u8]) {
    let mut hex = [0u8; 2 * Hash::MAX_SIZE];
    out.extend_from_slice(base16::encode_digest(digest, &mut hex).as_bytes());
}

/// `printUnquotedString`: quoted, no escaping (used for paths and names
/// that cannot contain special characters).
fn print_unquoted(out: &mut Vec<u8>, s: &[u8]) {
    out.push(b'"');
    out.extend_from_slice(s);
    out.push(b'"');
}

fn print_string_list<'a>(out: &mut Vec<u8>, items: impl Iterator<Item = &'a [u8]>) {
    out.push(b'[');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        print_string(out, item);
    }
    out.push(b']');
}

fn print_unquoted_list<'a>(out: &mut Vec<u8>, items: impl Iterator<Item = &'a [u8]>) {
    out.push(b'[');
    for (i, item) in items.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        print_unquoted(out, item);
    }
    out.push(b']');
}

/// Whether the env var `key` is named after an output, for env vars
/// visited in order: `outputs`, the output names in order too, is
/// advanced past the names before `key`, so that a whole environment is
/// checked in one pass over both sorted maps (the byte order of the
/// names is the order of both).
fn is_output<'o>(outputs: &mut Peekable<impl Iterator<Item = &'o BString>>, key: &[u8]) -> bool {
    while let Some(name) = outputs.peek() {
        match name.as_slice().cmp(key) {
            Ordering::Less => {
                outputs.next();
            }
            Ordering::Equal => return true,
            Ordering::Greater => return false,
        }
    }
    false
}

/// A cursor over ATerm text, with Nix's `FormatError` messages.
struct AtermParser<'a> {
    rest: &'a [u8],
}

impl<'a> AtermParser<'a> {
    fn expect(&mut self, s: &str) -> Result<(), DerivationError> {
        match self.rest.strip_prefix(s.as_bytes()) {
            Some(r) => {
                self.rest = r;
                Ok(())
            }
            None => Err(DerivationError::Format(
                format!("expected string '{s}'").into(),
            )),
        }
    }

    /// `endOfList`: consume `,` (more items) or `]` (done).
    fn end_of_list(&mut self) -> bool {
        match self.rest.split_first() {
            Some((b',', r)) => {
                self.rest = r;
                false
            }
            Some((b']', r)) => {
                self.rest = r;
                true
            }
            _ => false,
        }
    }

    /// `parseString`: a quoted string in which a backslash takes the
    /// next byte as it is, except that `\n`, `\r` and `\t` stand for
    /// control characters (Nix's `escapes` table). The bytes of the
    /// string with its escapes resolved, copying the runs between escapes
    /// whole.
    fn string_bytes(&mut self) -> Result<Vec<u8>, DerivationError> {
        let unterminated = || DerivationError::Format("unterminated string in derivation".into());
        self.expect("\"")?;
        let rest = self.rest;
        let mut out = Vec::new();
        let mut i = 0;
        loop {
            let run = rest[i..]
                .iter()
                .position(|&b| b == b'"' || b == b'\\')
                .ok_or_else(unterminated)?;
            out.extend_from_slice(&rest[i..i + run]);
            i += run;
            if rest[i] == b'"' {
                break;
            }
            let escaped = *rest.get(i + 1).ok_or_else(unterminated)?;
            out.push(match escaped {
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                other => other,
            });
            i += 2;
        }
        self.rest = &rest[i + 1..];
        Ok(out)
    }

    /// `parsePath`: a string that starts with `/`.
    fn path(&mut self) -> Result<Vec<u8>, DerivationError> {
        let s = self.string_bytes()?;
        validate_path(&s)?;
        Ok(s)
    }

    /// `parseStrings(str, true)`: a list of paths into a set, in byte
    /// order as `StringSet` keeps them (the order `parseStorePathSet`
    /// then parses them in, which decides the error reported first).
    fn paths(&mut self) -> Result<BTreeSet<Vec<u8>>, DerivationError> {
        self.expect("[")?;
        let mut res = BTreeSet::new();
        while !self.end_of_list() {
            res.insert(self.path()?);
        }
        Ok(res)
    }

    /// `parseStrings(str, false)`: a list of output names, bytes, into a
    /// set.
    fn output_names(&mut self) -> Result<BTreeSet<BString>, DerivationError> {
        self.expect("[")?;
        let mut res = BTreeSet::new();
        while !self.end_of_list() {
            res.insert(BString::from(self.string_bytes()?));
        }
        Ok(res)
    }
}

/// `DrvHashModulo`: the result of [`hash_derivation_modulo`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrvHashModulo {
    /// An input-addressed derivation: one hash for the whole derivation.
    DrvHash(Hash),
    /// A fixed-output derivation: one hash per output, derived from the
    /// output's content hash so that provenance does not leak.
    CaOutputHashes(BTreeMap<BString, Hash>),
    /// Floating-CA (or depending on one): no hash yet.
    Deferred,
}

/// A memo of hashes modulo by derivation path (`DrvHashes`).
pub type DrvHashes = HashMap<StorePath, DrvHashModulo>;

/// `hashDerivationModulo`. For fixed-output derivations the per-output
/// hash is `sha256("fixed:out:<method><algo>:<hash base16>:<output path>")`.
/// Otherwise each input derivation is looked up through `resolve` and
/// replaced by its hash (base16) in the unparsed text, fixed-output inputs
/// contributing one `out` per used output; the result is the sha256 of
/// that text.
pub fn hash_derivation_modulo(
    store: &StoreDir,
    drv: &Derivation,
    mask_outputs: bool,
    xp: XpSettings,
    resolve: &mut dyn FnMut(&StorePath) -> Result<DrvHashModulo, DerivationError>,
) -> Result<DrvHashModulo, DerivationError> {
    let mut scratch = Vec::new();
    hash_derivation_modulo_with(store, drv, mask_outputs, xp, resolve, &mut scratch)
}

/// [`hash_derivation_modulo`] serialising into `scratch` (cleared first),
/// so a caller hashing many derivations reuses one buffer.
pub fn hash_derivation_modulo_with(
    store: &StoreDir,
    drv: &Derivation,
    mask_outputs: bool,
    xp: XpSettings,
    resolve: &mut dyn FnMut(&StorePath) -> Result<DrvHashModulo, DerivationError>,
    scratch: &mut Vec<u8>,
) -> Result<DrvHashModulo, DerivationError> {
    let ty = drv.type_(xp)?;
    if ty.is_fixed() {
        let mut output_hashes = BTreeMap::new();
        for (name, output) in &drv.outputs {
            let DerivationOutput::CAFixed(ca) = output else {
                unreachable!("fixed derivations have only fixed outputs")
            };
            let path = output
                .path(store, &drv.name, name)?
                .expect("fixed outputs have a path");
            // sha256("fixed:out:<method><algo>:<base16>:<output path>")
            let mut hex = [0u8; 2 * Hash::MAX_SIZE];
            let mut hasher = Hasher::new(HashAlgo::Sha256);
            for part in [
                "fixed:out:",
                ca.method.prefix(),
                ca.hash.algo().name(),
                ":",
                base16::encode_digest(ca.hash.as_bytes(), &mut hex),
                ":",
                store.as_str(),
                "/",
                path.as_str(),
            ] {
                hasher.update(part.as_bytes());
            }
            output_hashes.insert(name.clone(), hasher.finish().0);
        }
        return Ok(DrvHashModulo::CaOutputHashes(output_hashes));
    }
    if matches!(
        ty,
        DerivationType::ContentAddressed { .. } | DerivationType::Impure
    ) {
        return Ok(DrvHashModulo::Deferred);
    }
    let Some(inputs) = modulo_inputs(drv, resolve)? else {
        return Ok(DrvHashModulo::Deferred);
    };
    scratch.clear();
    drv.write_aterm_with(scratch, store, mask_outputs, AtermInputs::Hashes(&inputs))?;
    Ok(DrvHashModulo::DrvHash(Hash::of(HashAlgo::Sha256, scratch)))
}

/// The input derivations of `drv` as `hashDerivationModulo` rewrites them:
/// each input's hash modulo with the output names used (a fixed-output
/// input contributes its per-output hash with the name `out`), sorted by
/// the hash's base16 form and then by name, without duplicates. `None` if
/// an input is deferred, which makes the whole derivation deferred.
fn modulo_inputs<'d>(
    drv: &'d Derivation,
    resolve: &mut dyn FnMut(&StorePath) -> Result<DrvHashModulo, DerivationError>,
) -> Result<Option<Vec<ModuloInput<'d>>>, DerivationError> {
    let mut inputs: Vec<ModuloInput<'d>> = Vec::with_capacity(drv.input_drvs.len());
    for (drv_path, outputs) in &drv.input_drvs {
        match resolve(drv_path)? {
            DrvHashModulo::Deferred => return Ok(None),
            DrvHashModulo::DrvHash(h) => {
                if outputs.is_empty() {
                    inputs.push((h, None));
                }
                inputs.extend(outputs.iter().map(|o| (h, Some(o.as_slice()))));
            }
            DrvHashModulo::CaOutputHashes(hashes) => {
                for output_name in outputs {
                    let h = hashes.get(output_name).ok_or_else(|| {
                        DerivationError::MissingOutputHash {
                            output: output_name.clone(),
                            drv: drv.name.clone(),
                        }
                    })?;
                    inputs.push((*h, Some(b"out".as_slice())));
                }
            }
        }
    }
    // Byte order of the digests is the order of their base16 forms.
    inputs.sort_unstable_by(|a, b| {
        a.0.as_bytes()
            .cmp(b.0.as_bytes())
            .then_with(|| a.1.cmp(&b.1))
    });
    inputs.dedup_by(|a, b| a.0.as_bytes() == b.0.as_bytes() && a.1 == b.1);
    Ok(Some(inputs))
}

/// `DownstreamPlaceholder::unknownCaOutput`: the string that stands in for
/// output `output_name` of the content-addressed derivation `drv_path` in a
/// dependent derivation, `/<nix32 sha256("nix-upstream-output:<hash part>:<output path name>")>`.
/// The output name is bytes, hashed as they are.
pub fn downstream_placeholder(
    drv_path: &StorePath,
    output_name: &[u8],
) -> Result<String, DerivationError> {
    let drv_name = drv_path.derivation_name()?;
    let mut hasher = Hasher::new(HashAlgo::Sha256);
    hasher.update(b"nix-upstream-output:");
    hasher.update(drv_path.hash_part().as_bytes());
    hasher.update(b":");
    hasher.update(&output_path_name(drv_name, output_name));
    Ok(placeholder_path(&hasher.finish().0))
}

/// `hashPlaceholder`: the string a floating-CA output's path is stood in
/// for during evaluation, `/<nix32 sha256("nix-output:<name>")>`. The
/// output name is bytes, hashed as they are.
pub fn hash_placeholder(output_name: &[u8]) -> String {
    let mut hasher = Hasher::new(HashAlgo::Sha256);
    hasher.update(b"nix-output:");
    hasher.update(output_name);
    placeholder_path(&hasher.finish().0)
}

/// `/<nix32 hash>`, the form of both placeholders.
fn placeholder_path(h: &Hash) -> String {
    let mut s = String::with_capacity(1 + crate::hash::nix32::encoded_len(Hash::MAX_SIZE));
    s.push('/');
    h.write_to(&mut s, HashFormat::Nix32, false);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple() -> Derivation {
        let store = StoreDir::default();
        let mut drv = Derivation {
            name: "x".into(),
            platform: "x86_64-linux".into(),
            builder: "/bin/sh".into(),
            ..Default::default()
        };
        drv.env.insert("builder".into(), "/bin/sh".into());
        drv.env.insert("name".into(), "x".into());
        drv.env.insert("system".into(), "x86_64-linux".into());
        drv.env.insert("out".into(), "".into());
        drv.outputs.insert("out".into(), DerivationOutput::Deferred);
        drv.fill_in_output_paths(&store, XpSettings::NONE, &mut |p| {
            Err(DerivationError::UnknownInputDerivation(p.to_string()))
        })
        .unwrap();
        drv
    }

    #[test]
    fn computes_paths_of_the_simplest_derivation() {
        // Oracle: nix eval --store dummy:// of `derivation { name = "x";
        // builder = "/bin/sh"; system = "x86_64-linux"; }`.
        let store = StoreDir::default();
        let drv = simple();
        assert_eq!(
            drv.outputs[b"out".as_slice()],
            DerivationOutput::InputAddressed(
                StorePath::new("s668cfx1vrqfjryf96jj8wfhg7f67dhj-x").unwrap()
            )
        );
        assert_eq!(
            drv.env["out".as_bytes()],
            "/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x"
        );
        assert_eq!(
            drv.store_path(&store).unwrap().as_str(),
            "97qlv6h78lxlm9zc8849ahsbcklhsi2y-x.drv"
        );
        let text = drv.to_aterm(&store).unwrap();
        assert_eq!(
            Derivation::parse(&store, &text, "x", XpSettings::NONE).unwrap(),
            drv
        );
    }

    /// The platform, builder, arguments and environment are bytes, kept
    /// as they are through serialising, hashing and parsing.
    #[test]
    fn strings_that_are_not_utf8_are_kept_as_bytes() {
        // Oracle: nix eval --store dummy:// of `let l = builtins.substring
        // 0 1 "é"; in derivation { name = "x"; builder = "/bin/sh" + l;
        // system = "x86_64-linux" + l; args = [ l "-c" ]; ${l} = l +
        // "\n"; a = l; }`, where `l` is the byte 0xc3.
        let store = StoreDir::default();
        let mut drv = Derivation {
            name: "x".into(),
            platform: b"x86_64-linux\xc3".into(),
            builder: b"/bin/sh\xc3".into(),
            args: vec![b"\xc3".into(), "-c".into()],
            ..Default::default()
        };
        for (k, v) in [
            (&b"\xc3"[..], &b"\xc3\n"[..]),
            (b"a", b"\xc3"),
            (b"builder", b"/bin/sh\xc3"),
            (b"name", b"x"),
            (b"out", b""),
            (b"system", b"x86_64-linux\xc3"),
        ] {
            drv.env.insert(k.into(), v.into());
        }
        drv.outputs.insert("out".into(), DerivationOutput::Deferred);
        let no_inputs = &mut |p: &StorePath| -> Result<DrvHashModulo, DerivationError> {
            Err(DerivationError::UnknownInputDerivation(p.to_string()))
        };
        let mut fused = drv.clone();
        drv.fill_in_output_paths(&store, XpSettings::NONE, no_inputs)
            .unwrap();
        let out = "/nix/store/02np9djk0rgi70zs2fs04favcdfphkbz-x";
        assert_eq!(drv.env["out".as_bytes()], out);
        let drv_path = drv.store_path(&store).unwrap();
        assert_eq!(drv_path.as_str(), "zgiwsmi121wfcij3ynp24kdmygcwix63-x.drv");
        // The single-serialisation path agrees.
        let mut scratch = Vec::new();
        let (fused_path, layout) = fused
            .fill_in_deferred(&store, no_inputs, &mut scratch)
            .unwrap()
            .expect("all outputs deferred");
        assert_eq!((&fused_path, &fused), (&drv_path, &drv));
        assert_eq!(
            DrvHashModulo::DrvHash(layout.hash_modulo(&scratch)),
            hash_derivation_modulo(&store, &drv, false, XpSettings::NONE, no_inputs).unwrap()
        );
        let text = drv.to_aterm(&store).unwrap();
        let shown = bstr::BStr::new(&text);
        for part in [
            &b"\"x86_64-linux\xc3\",\"/bin/sh\xc3\",[\"\xc3\",\"-c\"],[(\"a\",\"\xc3\")"[..],
            b",(\"\xc3\",\"\xc3\\n\")])",
        ] {
            assert!(bstr::ByteSlice::contains_str(&text[..], part), "{shown}");
        }
        assert_eq!(
            Derivation::parse(&store, &text, "x", XpSettings::NONE).unwrap(),
            drv
        );
    }

    #[test]
    fn unparse_escapes_like_nix() {
        let mut s = Vec::new();
        print_string(&mut s, b"a\"b\\c\nd\re\tf\xff");
        assert_eq!(s, b"\"a\\\"b\\\\c\\nd\\re\\tf\xff\"");
        s.extend_from_slice(b"rest");
        let mut p = AtermParser { rest: &s };
        assert_eq!(p.string_bytes().unwrap(), b"a\"b\\c\nd\re\tf\xff");
        assert_eq!(p.rest, b"rest");
    }

    #[test]
    fn parse_string_takes_any_escaped_byte_verbatim() {
        let parse = |text: &[u8]| {
            let mut p = AtermParser { rest: text };
            p.string_bytes().map(|s| (s, p.rest.to_vec()))
        };
        // Nix's `escapes` table maps only n, r and t; any other byte
        // after a backslash, a multi-byte character's first byte
        // included, is kept as it is.
        assert_eq!(
            parse("\"\\é\\x\\\\\\\"\"!".as_bytes()).unwrap(),
            ("éx\\\"".as_bytes().to_vec(), b"!".to_vec())
        );
        assert_eq!(
            parse(b"\"a\\").unwrap_err().to_string(),
            "unterminated string in derivation"
        );
        assert_eq!(
            parse(b"\"a\\\"").unwrap_err().to_string(),
            "unterminated string in derivation"
        );
        assert_eq!(
            parse(b"a\"").unwrap_err().to_string(),
            "expected string '\"'"
        );
        assert_eq!(
            parse(b"\"a\\\xff\"").unwrap(),
            (b"a\xff".to_vec(), b"".to_vec())
        );
    }

    /// Store paths in a `.drv` file are bytes, as every string in it is:
    /// one that is not a valid store path gets `parseStorePath`'s error
    /// (or `validatePath`'s), quoting it as it is. Oracle: `nix-store
    /// --register-validity` of such a file in a chroot store
    /// (`tests/oracle.rs` runs it), which adds "error parsing derivation
    /// '…': " before a `FormatError` only.
    #[test]
    fn store_paths_in_derivations_are_bytes() {
        let store = StoreDir::default();
        let h = "ffffffffffffffffffffffffffffffff";
        let message = |text: &[u8]| {
            Derivation::parse(&store, text, "x", XpSettings::NONE)
                .unwrap_err()
                .message_bytes()
        };
        let drv = |outputs: &str, input_drvs: &[u8], input_srcs: &[u8]| {
            [
                b"Derive([".as_slice(),
                outputs.as_bytes(),
                b"],[",
                input_drvs,
                b"],[",
                input_srcs,
                b"],\"x86_64-linux\",\"/bin/sh\",[],[])",
            ]
            .concat()
        };
        let out = r#"("out","/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x","","")"#;
        let bad_name = [
            b"path '".as_slice(),
            h.as_bytes(),
            b"-caf\xe9' is not a valid store path: name 'caf\xe9' contains illegal character '\xe9'",
        ]
        .concat();
        let src = [b"\"/nix/store/".as_slice(), h.as_bytes(), b"-caf\xe9\""].concat();
        assert_eq!(message(&drv(out, b"", &src)), bad_name);
        let output = [
            b"Derive([(\"out\",\"/nix/store/".as_slice(),
            h.as_bytes(),
            b"-caf\xe9\",\"\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])",
        ]
        .concat();
        assert_eq!(message(&output), bad_name);
        assert_eq!(
            message(b"Derive([(\"out\",\"caf\xe9\",\"\",\"\")],[],[],\"x86_64-linux\",\"/bin/sh\",[],[])"),
            b"bad path 'caf\xe9' in derivation"
        );
        assert_eq!(
            message(&drv(
                r#"("out","","","")"#,
                b"(\"/tmp/caf\xe9.drv\",[\"out\"])",
                b""
            )),
            b"path \"/tmp/caf\xe9.drv\" is not in the Nix store"
        );
        assert_eq!(
            message(b"DrvWithVersion(\"caf\xe9\",[])"),
            b"Unknown derivation ATerm format version 'caf\xe9'"
        );
        assert!(matches!(
            Derivation::parse(
                &store,
                b"DrvWithVersion(\"caf\xe9\",[])",
                "x",
                XpSettings::NONE
            ),
            Err(DerivationError::Format(_))
        ));
        // An escaped byte in a path is the byte, as in any string.
        let escaped = [b"\"/nix/store/".as_slice(), h.as_bytes(), b"-caf\\\xe9\""].concat();
        assert_eq!(message(&drv(out, b"", &escaped)), bad_name);
    }

    #[test]
    fn parse_keeps_the_first_of_repeated_outputs() {
        // `parseDerivation` inserts outputs with `emplace`.
        let store = StoreDir::default();
        let text = r#"Derive([("out","/nix/store/s668cfx1vrqfjryf96jj8wfhg7f67dhj-x","",""),("out","","",""),("out","/nix/store/00000000000000000000000000000000-x","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let drv = Derivation::parse(&store, text, "x", XpSettings::NONE).unwrap();
        assert_eq!(
            drv.outputs[b"out".as_slice()],
            DerivationOutput::InputAddressed(
                StorePath::new("s668cfx1vrqfjryf96jj8wfhg7f67dhj-x").unwrap()
            )
        );
        assert_eq!(drv.outputs.len(), 1);
    }

    /// Output types and formats behind experimental features, with the
    /// messages `nix store add --mode flat --name <name>.drv` gives for
    /// such a file without the feature.
    #[test]
    fn experimental_output_types_need_their_features() {
        let store = StoreDir::default();
        let drv = |outputs: &str| {
            format!(r#"Derive([{outputs}],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#)
        };
        let disabled = |feature: &str, reason: &str| {
            format!(
                "experimental Nix feature '{feature}' is disabled{reason}; add '--extra-experimental-features {feature}' to enable it"
            )
        };
        let git_out = format!(
            r#"("out","/nix/store/00000000000000000000000000000000-x","git:sha1","{}")"#,
            "0".repeat(40)
        );
        for (text, feature, reason, xp) in [
            (
                drv(r#"("out","","r:sha256","")"#),
                "ca-derivations",
                "",
                Xp::CaDerivations,
            ),
            (
                drv(r#"("out","","r:sha256","impure")"#),
                "impure-derivations",
                "",
                Xp::ImpureDerivations,
            ),
            (
                drv(r#"("out","","text:sha256","")"#),
                "dynamic-derivations",
                " (text-hashed derivation output)",
                Xp::DynamicDerivations,
            ),
            (drv(&git_out), "git-hashing", "", Xp::GitHashing),
        ] {
            assert_eq!(
                Derivation::parse(&store, &text, "x", XpSettings::NONE)
                    .unwrap_err()
                    .to_string(),
                disabled(feature, reason),
                "{text}"
            );
            // Text-hashed outputs are floating too.
            let on = XpSettings::NONE.with(xp).with(Xp::CaDerivations);
            assert!(
                Derivation::parse(&store, &text, "x", on).is_ok(),
                "{text} with {feature}"
            );
        }
        let dyn_drv = r#"DrvWithVersion("xp-dyn-drv",[("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#;
        assert_eq!(
            Derivation::parse(&store, dyn_drv, "dyn", XpSettings::NONE)
                .unwrap_err()
                .to_string(),
            disabled(
                "dynamic-derivations",
                " (derivation 'dyn', ATerm format version 'xp-dyn-drv')"
            )
        );
        assert_eq!(
            Derivation::parse(
                &store,
                dyn_drv,
                "dyn",
                XpSettings::NONE.with(Xp::DynamicDerivations)
            )
            .unwrap_err(),
            DerivationError::DynamicDerivations
        );
        assert_eq!(
            Derivation::parse(
                &store,
                r#"DrvWithVersion("other",[])"#,
                "x",
                XpSettings::NONE
            )
            .unwrap_err()
            .to_string(),
            "Unknown derivation ATerm format version 'other'"
        );
    }

    #[test]
    fn parse_header_errors_match_nix() {
        let store = StoreDir::default();
        let err = |text: &str| {
            Derivation::parse(&store, text, "x", XpSettings::NONE)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(err("Nope"), "expected string 'D'");
        assert_eq!(
            err("Dx"),
            "derivation does not start with 'Derive' or 'DrvWithVersion'"
        );
        assert_eq!(err("Dexive("), "expected string 'erive('");
        assert_eq!(
            err(r#"Derive([],[("/nix/store/00000000000000000000000000000000-x.drv",("#),
            "expected string '['"
        );
    }

    #[test]
    fn word_at_a_time_escaping_matches_byte_at_a_time() {
        // The obvious per-byte printString, as a reference.
        fn reference(s: &[u8]) -> Vec<u8> {
            let mut out = vec![b'"'];
            for &b in s {
                match b {
                    b'"' => out.extend_from_slice(b"\\\""),
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    b'\t' => out.extend_from_slice(b"\\t"),
                    b => out.push(b),
                }
            }
            out.push(b'"');
            out
        }
        // Every escape, the control bytes next to them, bytes whose
        // neighbours the word tests could confuse, multi-byte UTF-8 and
        // bytes that are not UTF-8, at every position and length around
        // the word size.
        let alphabet: [&[u8]; 21] = [
            b"a",
            b"\"",
            b"\\",
            b"\n",
            b"\r",
            b"\t",
            b"\x00",
            b"\x08",
            b"\x0b",
            b"\x0c",
            b"\x0e",
            b"!",
            b"#",
            b"[",
            b"]",
            b"\x7f",
            "é".as_bytes(),
            "€".as_bytes(),
            "𝄞".as_bytes(),
            b"\xff",
            b"\xc3",
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for len in 0..40 {
            for _ in 0..200 {
                let mut s = Vec::new();
                for _ in 0..len {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    s.extend_from_slice(alphabet[(seed % alphabet.len() as u64) as usize]);
                }
                let mut ours = Vec::new();
                print_string(&mut ours, &s);
                assert_eq!(ours, reference(&s), "{:?}", bstr::BStr::new(&s));
            }
        }
    }

    #[test]
    fn type_errors() {
        let store = StoreDir::default();
        let mut drv = simple();
        assert_eq!(
            drv.type_(XpSettings::NONE).unwrap(),
            DerivationType::InputAddressed { deferred: false }
        );
        drv.outputs.insert(
            "dev".into(),
            DerivationOutput::CAFixed(ContentAddress {
                method: ContentAddressMethod::Flat,
                hash: Hash::of(HashAlgo::Sha256, b""),
            }),
        );
        // BTreeMap order visits "dev" before "out", so the first complaint is
        // about the fixed output's name, as in Nix.
        assert_eq!(
            drv.type_(XpSettings::NONE).unwrap_err(),
            DerivationError::FixedOutputNotOut
        );
        let fixed = drv.outputs.remove(b"dev".as_slice()).unwrap();
        let out = drv.outputs.remove(b"out".as_slice()).unwrap();
        drv.outputs.insert("dev".into(), out);
        drv.outputs.insert("out".into(), fixed);
        assert_eq!(
            drv.type_(XpSettings::NONE).unwrap_err(),
            DerivationError::MixedOutputTypes
        );
        drv.outputs.clear();
        assert_eq!(
            drv.type_(XpSettings::NONE).unwrap_err(),
            DerivationError::NoOutputs
        );
        drv.outputs.insert(
            "dev".into(),
            DerivationOutput::CAFixed(ContentAddress {
                method: ContentAddressMethod::Flat,
                hash: Hash::of(HashAlgo::Sha256, b""),
            }),
        );
        assert_eq!(
            drv.type_(XpSettings::NONE).unwrap_err(),
            DerivationError::FixedOutputNotOut
        );
        assert!(Derivation::parse(&store, "Nope", "x", XpSettings::NONE).is_err());
        assert_eq!(
            Derivation::parse(
                &store,
                "DrvWithVersion(\"xp-dyn-drv\",[])",
                "x",
                XpSettings::NONE.with(Xp::DynamicDerivations)
            )
            .unwrap_err(),
            DerivationError::DynamicDerivations
        );
    }

    #[test]
    fn substitute_derivations_are_checked_like_nix() {
        // `processDerivationOutputPaths` and `type()` for
        // `builtin:substitute`.
        let store = StoreDir::default();
        let baked = XpSettings::NONE.with(Xp::BakedDerivations);
        let out = StorePath::new("s668cfx1vrqfjryf96jj8wfhg7f67dhj-x").unwrap();
        let bare = || {
            let mut drv = Derivation {
                name: "x".into(),
                platform: "builtin".into(),
                builder: "builtin:substitute".into(),
                ..Default::default()
            };
            drv.outputs
                .insert("out".into(), DerivationOutput::InputAddressed(out.clone()));
            drv
        };
        let fill = |mut drv: Derivation, xp: XpSettings| {
            drv.fill_in_output_paths(&store, xp, &mut |p| {
                Err(DerivationError::UnknownInputDerivation(p.to_string()))
            })
            .map(|()| drv)
            .map_err(|e| e.to_string())
        };
        let with = |change: fn(&mut Derivation)| {
            let mut drv = bare();
            change(&mut drv);
            fill(drv, baked).unwrap_err()
        };
        assert_eq!(
            with(|d| d.platform = "x86_64-linux".into()),
            "'builtin:substitute' derivation must be a builtin"
        );
        assert_eq!(
            with(|d| d.args.push("-c".into())),
            "'builtin:substitute' derivation must have no arguments"
        );
        assert_eq!(
            with(|d| {
                d.env.insert("out".into(), "".into());
            }),
            "'builtin:substitute' derivation must have no environment variables"
        );
        assert_eq!(
            with(|d| {
                d.input_srcs
                    .insert(StorePath::new("00000000000000000000000000000000-src").unwrap());
            }),
            "'builtin:substitute' derivation must have no inputs"
        );
        assert_eq!(
            fill(bare(), XpSettings::NONE).unwrap_err(),
            "experimental Nix feature 'baked-derivations' is disabled ('builtin:substitute' derivation); add '--extra-experimental-features baked-derivations' to enable it"
        );
        let filled = fill(bare(), baked).unwrap();
        assert_eq!(filled, bare());
        assert_eq!(filled.type_(baked), Ok(DerivationType::Substituted));
        // Only non-deferred input-addressed outputs can be substituted.
        let mut deferred = bare();
        deferred
            .outputs
            .insert("out".into(), DerivationOutput::Deferred);
        let mut fixed = bare();
        fixed.outputs.insert(
            "out".into(),
            DerivationOutput::CAFixed(ContentAddress {
                method: ContentAddressMethod::Flat,
                hash: Hash::of(HashAlgo::Sha256, b""),
            }),
        );
        for drv in [deferred, fixed] {
            assert_eq!(
                drv.type_(baked).unwrap_err().to_string(),
                "'builtin:substitute' derivation must have input-addressed outputs"
            );
            assert_eq!(
                drv.type_(XpSettings::NONE).unwrap_err(),
                DerivationError::MissingFeature(MissingExperimentalFeature {
                    feature: Xp::BakedDerivations,
                    reason: Some("'builtin:substitute' derivation".to_owned()),
                })
            );
        }
    }

    #[test]
    fn placeholder_matches_nix() {
        // `nix eval --expr 'builtins.placeholder "out"'`
        assert_eq!(
            hash_placeholder(b"out"),
            "/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"
        );
    }

    /// An input-addressed output whose name is not a valid store path
    /// name: its path is computed from the name's bytes and refused, with
    /// Nix's message. Oracle: `nix eval --raw -f ia.nix drvPath` of the
    /// Latin-1 file `derivation { name = "x"; system = "x"; builder =
    /// "x"; outputs = [ "\xff" ]; }`.
    #[test]
    fn input_addressed_output_names_are_bytes_and_refused_like_nix() {
        let store = StoreDir::default();
        let mut drv = Derivation {
            name: "x".into(),
            platform: "x".into(),
            builder: "x".into(),
            ..Default::default()
        };
        for (k, v) in [
            (&b"builder"[..], &b"x"[..]),
            (b"name", b"x"),
            (b"outputs", b"\xff"),
            (b"system", b"x"),
            (b"\xff", b""),
        ] {
            drv.env.insert(k.into(), v.into());
        }
        drv.outputs
            .insert(b"\xff".into(), DerivationOutput::Deferred);
        let expected: &[u8] = b"path 'vnld66mkjf0b5h5vqnd4b4a508qc52ik-x-\xff' is not a valid store path: name 'x-\xff' contains illegal character '\xff'";
        let no_inputs = &mut |p: &StorePath| -> Result<DrvHashModulo, DerivationError> {
            Err(DerivationError::UnknownInputDerivation(p.to_string()))
        };
        let err = drv
            .clone()
            .fill_in_output_paths(&store, XpSettings::NONE, no_inputs)
            .unwrap_err();
        assert_eq!(
            bstr::BStr::new(&err.message_bytes()),
            bstr::BStr::new(expected)
        );
        // The single-serialisation path fails alike.
        let Err(err) = drv
            .clone()
            .fill_in_deferred(&store, no_inputs, &mut Vec::new())
        else {
            panic!("an output name that is not ASCII has no output path");
        };
        assert_eq!(
            bstr::BStr::new(&err.message_bytes()),
            bstr::BStr::new(expected)
        );
    }

    /// A `.drv` file's hashes and algorithm names are bytes, parsed as
    /// `parseDerivationOutput` parses them: one that is not UTF-8 fails
    /// with Nix's message for those bytes.
    #[test]
    fn parse_reads_hashes_as_bytes() {
        let store = StoreDir::default();
        let message = |text: &[u8]| {
            Derivation::parse(&store, text, "x", XpSettings::NONE)
                .unwrap_err()
                .message_bytes()
        };
        assert_eq!(
            bstr::BStr::new(&message(
                b"Derive([(\"out\",\"/nix/store/00000000000000000000000000000000-x\",\"sha256\",\"\xff\")],[],[],\"x\",\"x\",[],[])"
            )),
            bstr::BStr::new(b"hash '\xff' has wrong length for hash algorithm 'sha256'")
        );
        assert_eq!(
            bstr::BStr::new(&message(
                b"Derive([(\"out\",\"\",\"r:sha\xff\",\"\")],[],[],\"x\",\"x\",[],[])"
            )),
            bstr::BStr::new(
                b"unknown hash algorithm 'sha\xff', expect 'blake3', 'md5', 'sha1', 'sha256', or 'sha512'"
            )
        );
    }

    /// A floating content-addressed output keeps a name that is not
    /// UTF-8: in the `.drv` text, its placeholders and the inputs of a
    /// derivation using it. Oracle: the `.drv` files `nix eval --store
    /// <dir> --raw -f ca.nix drvPath` writes for the Latin-1 file
    /// `derivation { name = "x"; system = "x"; builder = "x"; outputs = [
    /// "\xff" ]; __contentAddressed = true; outputHashMode = "recursive";
    /// outputHashAlgo = "sha256"; }`, and for `let d = import ./ca.nix; in
    /// derivation { name = "dep"; system = "x"; builder = "x"; inherit (d)
    /// outPath; p = d."\xff".outPath; }`.
    #[test]
    fn floating_output_names_are_bytes_like_nix() {
        let store = StoreDir::default();
        let ca = XpSettings::NONE.with(Xp::CaDerivations);
        let text: &[u8] = b"Derive([(\"\xff\",\"\",\"r:sha256\",\"\")],[],[],\"x\",\"x\",[],[(\"builder\",\"x\"),(\"name\",\"x\"),(\"outputHashAlgo\",\"sha256\"),(\"outputHashMode\",\"recursive\"),(\"outputs\",\"\xff\"),(\"system\",\"x\"),(\"\xff\",\"/17nqja818zpxs8fp3xdliy52hfqvyf7p7cxarl8qzj2j8n5ayrjg\")])";
        let drv = Derivation::parse(&store, text, "x", ca).unwrap();
        assert_eq!(
            drv.outputs.keys().collect::<Vec<_>>(),
            [&BString::from(&b"\xff"[..])]
        );
        assert_eq!(
            hash_placeholder(b"\xff"),
            "/17nqja818zpxs8fp3xdliy52hfqvyf7p7cxarl8qzj2j8n5ayrjg"
        );
        assert_eq!(drv.to_aterm(&store).unwrap(), text);
        let drv_path = drv.store_path(&store).unwrap();
        assert_eq!(drv_path.as_str(), "41jl7avqf4k12kbp4av14k5v7h3kzfja-x.drv");
        // `nix eval --raw -f ca.nix outPath`.
        let placeholder = downstream_placeholder(&drv_path, b"\xff").unwrap();
        assert_eq!(
            placeholder,
            "/14f1idrlds3hlhr49m17zkazlh64gh3nprk2c3nivzxb5xibjwd1"
        );
        assert_eq!(
            hash_derivation_modulo(&store, &drv, false, ca, &mut |p| {
                Err(DerivationError::UnknownInputDerivation(p.to_string()))
            })
            .unwrap(),
            DrvHashModulo::Deferred
        );

        let dep: &[u8] = b"Derive([(\"out\",\"\",\"\",\"\")],[(\"/nix/store/41jl7avqf4k12kbp4av14k5v7h3kzfja-x.drv\",[\"\xff\"])],[],\"x\",\"x\",[],[(\"builder\",\"x\"),(\"name\",\"dep\"),(\"out\",\"\"),(\"outPath\",\"/14f1idrlds3hlhr49m17zkazlh64gh3nprk2c3nivzxb5xibjwd1\"),(\"p\",\"/14f1idrlds3hlhr49m17zkazlh64gh3nprk2c3nivzxb5xibjwd1\"),(\"system\",\"x\")])";
        let mut dep_drv = Derivation::parse(&store, dep, "dep", ca).unwrap();
        assert_eq!(
            dep_drv.input_drvs[&drv_path],
            BTreeSet::from([BString::from(&b"\xff"[..])])
        );
        assert_eq!(dep_drv.to_aterm(&store).unwrap(), dep);
        assert_eq!(
            dep_drv.store_path(&store).unwrap().as_str(),
            "58jj2wds2yvy3pdsd4vqn2gmm6bbrzzi-dep.drv"
        );
        // An input that is floating leaves the output deferred.
        let resolve = &mut |p: &StorePath| -> Result<DrvHashModulo, DerivationError> {
            assert_eq!(p, &drv_path);
            Ok(DrvHashModulo::Deferred)
        };
        dep_drv.fill_in_output_paths(&store, ca, resolve).unwrap();
        assert_eq!(
            dep_drv.outputs[b"out".as_slice()],
            DerivationOutput::Deferred
        );
    }
}
