//! Fetching inputs into source trees (`Input::getAccessor`,
//! `Input::checkLocks`, `EvalState::mountInput`).
//!
//! The [`Fetcher`] trait is what a scheme's `getAccessor` does: produce a
//! tree and the input attributes learned while fetching (`rev`,
//! `lastModified`, `narHash`, …). [`fetch_input`] wraps that with the
//! scheme-independent rules: lock-file entries are final and must agree
//! with what was fetched, a `narHash` in the reference must match the
//! tree, and the tree's store path follows from its NAR hash.

use std::sync::Arc;

use brix_store::{
    nar, CanonPath, FileIngestionMethod, FixedOutputInfo, Hash, HashAlgo, SourceAccessor, StoreDir,
    StorePath, StoreReferences,
};

use crate::attrs::{attrs_to_json_string, Attr};
use crate::hashes::NarHash;
use crate::input::{FetchSettings, Input, Scheme};
use crate::{err, Error, Result};

/// A fetched source tree with the fully resolved input describing it.
#[derive(Clone)]
pub struct FetchedTree {
    pub accessor: Arc<dyn SourceAccessor>,
    /// The locked, final input: `type`, `rev`, `narHash`, `lastModified`…
    pub input: Input,
    /// Whether `input`'s `narHash` is known to be the hash of `accessor`
    /// without hashing it again: a tree taken from a store or from the
    /// fetch cache lives at a path derived from that hash, exactly as Nix
    /// trusts the contents of a store path. [`fetch_input`] hashes every
    /// tree that is not verified.
    pub verified: bool,
}

impl std::fmt::Debug for FetchedTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FetchedTree")
            .field("input", &self.input)
            .finish_non_exhaustive()
    }
}

impl FetchedTree {
    /// The tree's NAR hash (always present after [`fetch_input`]).
    pub fn nar_hash(&self) -> Result<NarHash> {
        self.input
            .nar_hash()?
            .ok_or_else(|| err!("fetched input '{}' has no NAR hash", self.input))
    }

    /// `Input::computeStorePath`: `makeFixedOutputPath(name, NixArchive,
    /// narHash)`, the path the tree would have in a real store.
    pub fn store_path(&self, store_dir: &StoreDir) -> Result<StorePath> {
        compute_store_path(store_dir, &self.input)
    }
}

/// `Input::computeStorePath`.
pub fn compute_store_path(store_dir: &StoreDir, input: &Input) -> Result<StorePath> {
    let Some(nar_hash) = input.nar_hash()? else {
        return Err(err!(
            "cannot compute store path for unlocked input '{}'",
            input
        ));
    };
    store_dir
        .make_fixed_output_path(
            &input.name()?,
            &FixedOutputInfo {
                method: FileIngestionMethod::NixArchive,
                hash: Hash::from_bytes(HashAlgo::Sha256, nar_hash.as_bytes()),
                references: StoreReferences::default(),
            },
        )
        .map_err(|e| err!("{}", e))
}

/// What a scheme does to obtain a tree (`InputScheme::getAccessor`).
pub trait Fetcher {
    /// Fetch `input`, returning the tree and the input with whatever
    /// attributes fetching determined. The returned input need not be
    /// final and need not carry a `narHash`; [`fetch_input`] takes care of
    /// both.
    fn fetch(&self, settings: &FetchSettings, input: &Input) -> Result<FetchedTree>;

    /// What a persistent cache remembers about `input` without fetching:
    /// the same input with its mutable reference resolved to a `rev` and
    /// its locked attributes (`narHash`, `lastModified`, …) filled in,
    /// or `None` when the cache cannot complete it. A complete input may
    /// be served from a store or cached tree; see
    /// `native::reuse_from_store`.
    fn resolve_cached(&self, settings: &FetchSettings, input: &Input) -> Result<Option<Input>> {
        let _ = (settings, input);
        Ok(None)
    }

    /// Where `Input::getAccessorUnchecked` starts: the input with all it
    /// takes to find its tree without fetching. That is the input as given
    /// when it has a `narHash` (a tree is found by its content), else what
    /// [`Fetcher::resolve_cached`] completes it to; `None` when neither
    /// knows it.
    fn locked_candidate(&self, settings: &FetchSettings, input: &Input) -> Result<Option<Input>> {
        if input.nar_hash()?.is_some() {
            return Ok(Some(input.clone()));
        }
        self.resolve_cached(settings, input)
    }

    /// Whether [`Fetcher::fetch`] would answer `input` from this machine
    /// (a persistent cache, a store, a local tree read in place) rather
    /// than from its source. A caller deciding whether a fetch is worth
    /// starting early asks this; `false` when unsure.
    fn has_locally(&self, settings: &FetchSettings, input: &Input) -> Result<bool> {
        let _ = (settings, input);
        Ok(false)
    }

    /// Told the NAR hash [`fetch_input`] computed for a tree this fetcher
    /// returned without vouching for one (`result` is the final input),
    /// so that it may remember it as Nix's fetch-to-store cache keeps a
    /// tree's hash by fingerprint.
    fn nar_hash_computed(
        &self,
        settings: &FetchSettings,
        result: &Input,
        hash: &NarHash,
    ) -> Result<()> {
        let _ = (settings, result, hash);
        Ok(())
    }

    /// The NAR hash Nix's `fetchToStore` cache (`sourcePathToHash`)
    /// remembers for trees with this fingerprint (see
    /// [`SourceAccessor::fingerprint`]): the hash of the first such tree
    /// that was hashed. A fingerprint that does not tell all (a Git working
    /// tree's says nothing of its submodules) gets that first tree's hash,
    /// whatever the tree at hand holds.
    fn remembered_nar_hash(
        &self,
        settings: &FetchSettings,
        fingerprint: &str,
    ) -> Result<Option<NarHash>> {
        let _ = (settings, fingerprint);
        Ok(None)
    }

    /// Remembers `hash` for trees with this fingerprint (see
    /// [`Fetcher::remembered_nar_hash`]).
    fn remember_nar_hash(&self, fingerprint: &str, hash: &NarHash) -> Result<()> {
        let _ = (fingerprint, hash);
        Ok(())
    }
}

/// A fetcher that fails for everything: the default of a library user who
/// supplies pre-fetched trees.
pub struct NoFetcher;

impl Fetcher for NoFetcher {
    fn fetch(&self, _settings: &FetchSettings, input: &Input) -> Result<FetchedTree> {
        Err(err!(
            "fetching is not available in this configuration (while fetching the input '{}')",
            input
        ))
    }
}

/// The NAR hash of an accessor's root.
pub fn tree_nar_hash(accessor: &dyn SourceAccessor) -> Result<NarHash> {
    let (hash, _size) =
        nar::hash(accessor, &CanonPath::root(), HashAlgo::Sha256).map_err(nar_error)?;
    let bytes: [u8; 32] = hash
        .as_bytes()
        .try_into()
        .expect("sha256 digests are 32 bytes");
    Ok(NarHash::from_bytes(bytes))
}

/// A NAR error as an error of this crate, with the traces an accessor
/// added to it (a Git LFS file is fetched as it is read, and a failure
/// says "while smudging git-lfs file '…'").
fn nar_error(e: nar::NarError) -> Error {
    match e {
        nar::NarError::Accessor(e) => e
            .traces()
            .into_iter()
            .fold(err!("{}", e.untraced()), |error, trace| error.trace(trace)),
        other => err!("{}", other),
    }
}

/// `Input::getAccessor`, then the NAR-hash part of `EvalState::mountInput`:
/// fetch, mark the result final and check it against what was asked for,
/// all under the trace "while fetching the input '…'" as `getAccessor`
/// adds it; then make sure the result carries its NAR hash, which must be
/// the one the input asks for. That check is `mountInput`'s, outside the
/// trace, and a mismatch ends the process with 102.
pub fn fetch_input(
    fetcher: &dyn Fetcher,
    settings: &FetchSettings,
    input: &Input,
) -> Result<FetchedTree> {
    let (fetched, vouched) = get_accessor(fetcher, settings, input)?;
    with_nar_hash(fetcher, settings, input, fetched, vouched, true)
}

/// [`fetch_input`] for a tree that may be mounted lazily: when neither
/// the input nor the fetcher supplies a NAR hash, none is computed (it
/// costs a read of every file). The result then carries no `narHash` and
/// is not `verified`. A hash the input asks for is still checked.
pub fn fetch_input_unhashed(
    fetcher: &dyn Fetcher,
    settings: &FetchSettings,
    input: &Input,
) -> Result<FetchedTree> {
    let (fetched, vouched) = get_accessor(fetcher, settings, input)?;
    with_nar_hash(fetcher, settings, input, fetched, vouched, false)
}

/// `Input::getAccessor`: the fetch and `checkLocks`, an error in either
/// traced with the input. Also returns the NAR hash the fetcher vouches
/// for, taken before `check_locks` replaces the result's attributes with
/// a final input's.
fn get_accessor(
    fetcher: &dyn Fetcher,
    settings: &FetchSettings,
    input: &Input,
) -> Result<(FetchedTree, Option<NarHash>)> {
    get_accessor_untraced(fetcher, settings, input)
        .map_err(|e| e.trace(format!("while fetching the input '{input}'")))
}

fn get_accessor_untraced(
    fetcher: &dyn Fetcher,
    settings: &FetchSettings,
    input: &Input,
) -> Result<(FetchedTree, Option<NarHash>)> {
    if input.scheme.is_none() {
        return Err(err!(
            "cannot fetch unsupported input '{}'",
            attrs_to_json_string(&input.attrs)
        ));
    }
    let FetchedTree {
        accessor,
        input: mut result,
        verified,
    } = fetcher.fetch(settings, input)?;
    let vouched = if verified { result.nar_hash()? } else { None };
    // brix's fetchers give a result the NAR hash they know of its tree
    // (the fetch cache files trees by it), where of Nix's schemes only
    // some report one. A result is checked against the input as the
    // scheme's would be, without a hash the scheme does not report; that
    // hash is compared with the input's afterwards, as `mountInput` does.
    if !reports_nar_hash(input, settings) && result.nar_hash()? != input.nar_hash()? {
        result.attrs.remove("narHash");
    }
    result.set_final();
    check_locks(input, &mut result)?;
    Ok((
        FetchedTree {
            accessor,
            input: result,
            verified,
        },
        vouched,
    ))
}

/// Whether the attributes Nix's fetcher for the input's scheme returns
/// (`InputScheme::getAccessor`) include the tree's NAR hash: those of
/// `tarball` and `file` inputs do, and those of the forges' archives
/// without `trust-tarballs-from-git-forges`; `git`, `hg`, `path` and the
/// rest leave it to `fetchToStore` and `mountInput`.
fn reports_nar_hash(input: &Input, settings: &FetchSettings) -> bool {
    match input.scheme {
        Some(Scheme::Tarball | Scheme::File) => true,
        Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
            !settings.trust_tarballs_from_git_forges
        }
        _ => false,
    }
}

/// The NAR-hash part of `EvalState::mountInput`: the hash the fetcher
/// vouched for, or the tree's (unless it need not be computed), must be
/// the one the input asks for, and the result carries it.
fn with_nar_hash(
    fetcher: &dyn Fetcher,
    settings: &FetchSettings,
    input: &Input,
    fetched: FetchedTree,
    vouched: Option<NarHash>,
    always_hash: bool,
) -> Result<FetchedTree> {
    let FetchedTree {
        accessor,
        input: mut result,
        ..
    } = fetched;
    let wants_hash = always_hash || input.nar_hash()?.is_some();
    // A tree with a fingerprint has the hash Nix's `fetchToStore` cache
    // remembers for it, if any (`Fetcher::remembered_nar_hash`), and the
    // one it gets otherwise is remembered. Nix hashes such a tree only
    // when it must (`mountInput`): one that need not be lockable is
    // mounted lazily, whatever the fetcher knows of it, and hashed (through
    // the same cache, by its accessor) when evaluation needs its store path.
    let fingerprint = accessor.fingerprint().map(str::to_owned);
    if fingerprint.is_some() && !wants_hash {
        return Ok(FetchedTree {
            accessor,
            input: result,
            verified: false,
        });
    }
    let remembered = match &fingerprint {
        Some(fingerprint) => fetcher.remembered_nar_hash(settings, fingerprint)?,
        None => None,
    };
    let actual = match (remembered, vouched) {
        (Some(h), _) => h,
        (None, Some(h)) => {
            if let Some(fingerprint) = &fingerprint {
                fetcher.remember_nar_hash(fingerprint, &h)?;
            }
            h
        }
        (None, None) if wants_hash => {
            let hash = tree_nar_hash(accessor.as_ref())?;
            fetcher.nar_hash_computed(settings, &result, &hash)?;
            if let Some(fingerprint) = &fingerprint {
                fetcher.remember_nar_hash(fingerprint, &hash)?;
            }
            hash
        }
        (None, None) => {
            return Ok(FetchedTree {
                accessor,
                input: result,
                verified: false,
            })
        }
    };
    if let Some(expected) = input.nar_hash()? {
        if expected != actual {
            return Err(Error::HashMismatch(format!(
                "NAR hash mismatch in input '{}', expected '{}' but got '{}'",
                input
                    .to_url_text(false)
                    .unwrap_or_else(|_| input.to_string()),
                expected.to_sri(),
                actual.to_sri()
            )));
        }
    }
    result
        .attrs
        .entry("narHash".into())
        .or_insert_with(|| Attr::String(actual.to_sri()));

    Ok(FetchedTree {
        accessor,
        input: result,
        verified: true,
    })
}

/// `Input::checkLocks(specified, result)`: a final (lock file) input wins
/// over what was fetched, but every attribute both have must agree; an
/// unlocked input with a `narHash` or `rev` must get the same back.
pub fn check_locks(specified: &Input, result: &mut Input) -> Result<()> {
    let mut specified = specified.clone();
    if specified.is_final() {
        // Old lock files sometimes lack the base-64 padding of `narHash`;
        // normalise both sides before comparing.
        if let Some(h) = specified.nar_hash()? {
            specified
                .attrs
                .insert("narHash".into(), Attr::String(h.to_sri()));
        }
        if let Some(h) = result.nar_hash()? {
            result
                .attrs
                .insert("narHash".into(), Attr::String(h.to_sri()));
        }
        for (name, value) in &specified.attrs {
            if let Some(other) = result.attrs.get(name) {
                if other != value {
                    return Err(err!(
                        "mismatch in field '{}' of input '{}', got '{}'",
                        name,
                        attrs_to_json_string(&specified.attrs),
                        attrs_to_json_string(&result.attrs)
                    ));
                }
            }
        }
        result.attrs = specified.attrs;
        return Ok(());
    }

    if let Some(prev) = specified.nar_hash()? {
        match result.nar_hash()? {
            Some(got) if got != prev => {
                return Err(Error::HashMismatch(format!(
                    "NAR hash mismatch in input '{}', expected '{}' but got '{}'",
                    specified,
                    prev.to_sri(),
                    got.to_sri()
                )))
            }
            // The tree's hash is verified against `specified` later.
            _ => {}
        }
    }
    if let Some(prev) = specified.rev()? {
        if result.rev()? != Some(prev) {
            return Err(err!(
                "'rev' attribute mismatch in input '{}', expected {}",
                result,
                prev
            ));
        }
    }
    Ok(())
}

/// A store-layer error with its message, for `?` on brix-store results.
impl From<brix_store::AccessorError> for Error {
    fn from(e: brix_store::AccessorError) -> Self {
        Error::Message(e.to_string())
    }
}

impl From<brix_store::nar::NarError> for Error {
    fn from(e: brix_store::nar::NarError) -> Self {
        Error::Message(e.to_string())
    }
}

impl From<brix_store::HashError> for Error {
    fn from(e: brix_store::HashError) -> Self {
        Error::Message(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attrs::attrs;
    use brix_store::MemoryAccessor;

    struct Fixed(MemoryAccessor, Input);

    impl Fetcher for Fixed {
        fn fetch(&self, _settings: &FetchSettings, _input: &Input) -> Result<FetchedTree> {
            Ok(FetchedTree {
                accessor: Arc::new(self.0.clone()),
                input: self.1.clone(),
                verified: false,
            })
        }
    }

    fn tree() -> MemoryAccessor {
        let mut m = MemoryAccessor::new();
        m.add_file(&CanonPath::new("/hello").unwrap(), b"hi\n".to_vec(), false)
            .unwrap();
        m
    }

    fn github(extra: &[(&str, Attr)]) -> Input {
        let mut a = attrs([
            ("type", "github".into()),
            ("owner", "o".into()),
            ("repo", "r".into()),
            ("rev", "a".repeat(40).into()),
        ]);
        for (k, v) in extra {
            a.insert(k.to_string(), v.clone());
        }
        Input::from_attrs(&FetchSettings::default(), a).unwrap()
    }

    #[test]
    fn fetched_tree_gets_nar_hash_and_store_path() {
        let settings = FetchSettings::default();
        let fetcher = Fixed(tree(), github(&[("lastModified", Attr::Int(7))]));
        let fetched = fetch_input(&fetcher, &settings, &github(&[])).unwrap();
        let h = fetched.nar_hash().unwrap();
        assert!(fetched.input.is_final());
        assert_eq!(fetched.input.last_modified().unwrap(), Some(7));
        // The same hash `nix hash path` gives for a dir with one file "hi\n".
        let sp = fetched.store_path(&StoreDir::new("/nix/store")).unwrap();
        assert!(sp.name() == "source");
        // Mismatching expectations are errors with Nix's message.
        let wrong = github(&[(
            "narHash",
            Attr::String("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into()),
        )]);
        let err = fetch_input(&fetcher, &settings, &wrong).unwrap_err();
        assert!(
            err.message()
                .starts_with("NAR hash mismatch in input 'github:o/r/"),
            "{err}"
        );
        assert!(err.message().contains(&h.to_sri()));
        // A matching expectation passes and is kept.
        let right = github(&[("narHash", Attr::String(h.to_sri()))]);
        let again = fetch_input(&fetcher, &settings, &right).unwrap();
        assert_eq!(again.nar_hash().unwrap(), h);
    }

    /// A fetcher that reports the tree's NAR hash, as brix's do once their
    /// cache has filed the tree.
    struct Vouching(MemoryAccessor, Input);

    impl Fetcher for Vouching {
        fn fetch(&self, _settings: &FetchSettings, _input: &Input) -> Result<FetchedTree> {
            let mut input = self.1.clone();
            input.attrs.insert(
                "narHash".into(),
                Attr::String(tree_nar_hash(&self.0)?.to_sri()),
            );
            Ok(FetchedTree {
                accessor: Arc::new(self.0.clone()),
                input,
                verified: true,
            })
        }
    }

    struct Failing;

    impl Fetcher for Failing {
        fn fetch(&self, _settings: &FetchSettings, _input: &Input) -> Result<FetchedTree> {
            Err(err!("hg failed with exit code 255"))
        }
    }

    /// Where Nix reports a tree whose NAR hash is not the one asked for:
    /// `mountInput` for a scheme whose fetcher reports no hash (outside the
    /// input's trace), `checkLocks` for one that does (under it); and
    /// what the fetcher itself fails with, under it too.
    #[test]
    fn nar_hash_mismatches_are_reported_where_nix_reports_them() {
        let wrong = Attr::String("sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into());
        let mut settings = FetchSettings::default();
        let fetcher = Vouching(tree(), github(&[]));
        let mut asked = github(&[("narHash", wrong.clone())]);
        asked.set_final();
        let traced = |input: &Input| vec![format!("while fetching the input '{input}'")];

        // A trusted forge's archive: Nix's fetcher reports no hash.
        let err = fetch_input(&fetcher, &settings, &asked).unwrap_err();
        assert!(
            err.message()
                .starts_with("NAR hash mismatch in input 'github:o/r/"),
            "{err}"
        );
        assert!(err.traces().is_empty(), "{err:?}");
        assert_eq!(err.exit_status(), 102);

        // An untrusted one's: its fetcher reports it, and a final input's
        // fields must agree with what it reports.
        settings.trust_tarballs_from_git_forges = false;
        let err = fetch_input(&fetcher, &settings, &asked).unwrap_err();
        assert!(
            err.message().starts_with("mismatch in field 'narHash'"),
            "{err}"
        );
        assert_eq!(err.traces(), traced(&asked));
        assert_eq!(err.exit_status(), 1);
        // An input that is not final is checked by its hash.
        let not_final = github(&[("narHash", wrong)]);
        let err = fetch_input(&fetcher, &settings, &not_final).unwrap_err();
        assert!(
            err.message()
                .starts_with("NAR hash mismatch in input 'github:o/r/"),
            "{err}"
        );
        assert_eq!(err.traces(), traced(&not_final));
        assert_eq!(err.exit_status(), 102);

        let err = fetch_input(&Failing, &settings, &not_final).unwrap_err();
        assert_eq!(err.message(), "hg failed with exit code 255");
        assert_eq!(err.traces(), traced(&not_final));
    }

    #[test]
    fn final_inputs_win_but_must_agree() {
        let settings = FetchSettings::default();
        let mut locked = github(&[("lastModified", Attr::Int(7))]);
        locked.set_final();
        let fetcher = Fixed(tree(), github(&[("lastModified", Attr::Int(8))]));
        let err = fetch_input(&fetcher, &settings, &locked).unwrap_err();
        assert!(
            err.message()
                .starts_with("mismatch in field 'lastModified'"),
            "{err}"
        );

        let fetcher = Fixed(
            tree(),
            github(&[("lastModified", Attr::Int(7)), ("treeHash", "x".into())]),
        );
        let ok = fetch_input(&fetcher, &settings, &locked).unwrap();
        // The lock's attrs are used verbatim; fetched extras are dropped.
        assert!(!ok.input.attrs.contains_key("treeHash"));
        assert!(ok.input.attrs.contains_key("narHash"));
    }

    #[test]
    fn rev_mismatch_is_reported() {
        let settings = FetchSettings::default();
        let mut other = github(&[]);
        other
            .attrs
            .insert("rev".into(), Attr::String("b".repeat(40)));
        let fetcher = Fixed(tree(), other);
        let err = fetch_input(&fetcher, &settings, &github(&[])).unwrap_err();
        assert!(
            err.message()
                .starts_with("'rev' attribute mismatch in input"),
            "{err}"
        );
    }
}
