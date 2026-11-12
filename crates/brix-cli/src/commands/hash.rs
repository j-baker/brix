//! `brix hash {file,path,convert,to-*}` (`src/nix/hash.cc`).
//!
//! Nix parses each flag's value as the flag is read, so the flags are
//! applied here in command-line order too: that decides which of two bad
//! flags is reported and the order of the deprecation warnings.

use brix_store::{FileIngestionMethod, Hash, HashAlgo, HashError, HashFormat, XpSettings};

use crate::commands::realise::fs_path;
use crate::commands::{print_stdout, Ctx};
use crate::{CliError, Result};

/// `nix hash file` (`CmdHashFile`): the hash of each file's contents.
pub fn file(ctx: &Ctx<'_>) -> Result<()> {
    hash_paths(ctx, FileIngestionMethod::Flat)
}

/// `nix hash path` (`CmdHashPath`): by default, the hash of each path's
/// NAR serialisation.
pub fn path(ctx: &Ctx<'_>) -> Result<()> {
    hash_paths(ctx, FileIngestionMethod::NixArchive)
}

/// The experimental features the hash parsers check.
fn xp_settings(ctx: &Ctx<'_>) -> XpSettings {
    XpSettings::from_names(
        ctx.settings
            .experimental_features()
            .iter()
            .map(String::as_str),
    )
}

/// A hash parsing error as Nix reports it: an unknown algorithm or
/// format name is a `UsageError` (followed by the "Try --help" hint),
/// anything else a plain error.
fn hash_error(e: HashError) -> CliError {
    match e {
        HashError::UnknownAlgo(_) | HashError::UnknownFormat(_) => {
            CliError::UsageWithHint(e.to_string().into())
        }
        _ => CliError::Brix(e.into()),
    }
}

/// `parseHashFormat` for a flag's value, printing Nix's warning for the
/// deprecated spelling `base32`.
fn hash_format(s: &str) -> Result<HashFormat> {
    let (format, deprecation) = HashFormat::parse(s).map_err(hash_error)?;
    if let Some(warning) = deprecation {
        brix::log::stderr(&format!("warning: {warning}"));
    }
    Ok(format)
}

/// The value of a flag that takes one.
fn value(values: &[String]) -> &str {
    values
        .first()
        .expect("the parser gives a flag with an argument its value")
}

/// `CmdHashBase::run`.
fn hash_paths(ctx: &Ctx<'_>, default_mode: FileIngestionMethod) -> Result<()> {
    let xp = xp_settings(ctx);
    let mut algo = HashAlgo::Sha256;
    let mut format = HashFormat::Sri;
    let mut mode = default_mode;
    for f in &ctx.inv.flags {
        match f.name.as_str() {
            "sri" => format = HashFormat::Sri,
            "base64" => format = HashFormat::Base64,
            "base32" => format = HashFormat::Nix32,
            "base16" => format = HashFormat::Base16,
            "type" | "algo" => algo = HashAlgo::parse(value(&f.values), xp).map_err(hash_error)?,
            "format" => format = hash_format(value(&f.values))?,
            // `parseFileIngestionMethod`: a `UsageError`.
            "mode" => {
                mode = FileIngestionMethod::parse(value(&f.values))
                    .map_err(|e| CliError::UsageWithHint(e.to_string().into()))?
            }
            _ => {}
        }
    }
    let mut out = String::new();
    for path in ctx.inv.arg("paths") {
        let abs = ctx.abs_path(path);
        let hash = match mode {
            FileIngestionMethod::Flat => {
                let data = std::fs::read(fs_path(&abs))
                    .map_err(|e| brix::Error::io(format!("reading '{path}'"), e))?;
                Hash::of(algo, &data)
            }
            FileIngestionMethod::NixArchive => {
                let accessor = brix_store::FsAccessor::new("/");
                let canon = brix_store::CanonPath::new(&abs).map_err(brix::Error::from)?;
                brix_store::nar::hash(&accessor, &canon, algo)
                    .map_err(brix::Error::from)?
                    .0
            }
            FileIngestionMethod::Git => {
                // `git::dump` needs the feature, as each path is hashed.
                xp.require(brix_store::Xp::GitHashing, None)
                    .map_err(|e| hash_error(HashError::MissingFeature(e)))?;
                return Err(CliError::Brix(brix::Error::Usage(
                    "the 'git' hashing mode is not supported by brix".into(),
                )));
            }
        };
        out.push_str(&hash.to_string(format, format == HashFormat::Sri));
        out.push('\n');
    }
    print_stdout(&out)
}

/// `nix hash convert` (`CmdHashConvert`).
pub fn convert(ctx: &Ctx<'_>) -> Result<()> {
    let xp = xp_settings(ctx);
    let mut from = None;
    let mut to = HashFormat::Sri;
    let mut algo = None;
    for f in &ctx.inv.flags {
        match f.name.as_str() {
            "from" => from = Some(hash_format(value(&f.values))?),
            "to" => to = hash_format(value(&f.values))?,
            "hash-algo" => algo = Some(HashAlgo::parse(value(&f.values), xp).map_err(hash_error)?),
            _ => {}
        }
    }
    let mut out = String::new();
    for s in ctx.inv.arg("hashes") {
        let (h, parsed_format) =
            Hash::parse_any_returning_format(s, algo, xp).map_err(hash_error)?;
        if let Some(from) = from {
            if from != parsed_format {
                return Err(CliError::Brix(brix::Error::Store(
                    format!(
                        "input hash '{s}' has format '{}', but '--from {}' was specified",
                        parsed_format.name(),
                        from.name()
                    )
                    .into(),
                )));
            }
        }
        out.push_str(&h.to_string(to, to == HashFormat::Sri));
        out.push('\n');
    }
    print_stdout(&out)
}

/// The deprecated `nix hash to-*` commands (`CmdToBase`).
pub fn to_base(ctx: &Ctx<'_>, format: HashFormat) -> Result<()> {
    let xp = xp_settings(ctx);
    let mut algo = None;
    for f in &ctx.inv.flags {
        if f.name == "type" {
            algo = Some(HashAlgo::parse(value(&f.values), xp).map_err(hash_error)?);
        }
    }
    brix::log::stderr("warning: The old format conversion subcommands of `nix hash` were deprecated in favor of `nix hash convert`.");
    let mut out = String::new();
    for s in ctx.inv.arg("strings") {
        let h = Hash::parse_any(s, algo, xp).map_err(hash_error)?;
        out.push_str(&h.to_string(format, format == HashFormat::Sri));
        out.push('\n');
    }
    print_stdout(&out)
}
