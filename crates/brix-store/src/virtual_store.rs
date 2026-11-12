//! The virtual store: what evaluation *would* have written to the store.
//!
//! brix never writes to a Nix store. Instead every operation that would
//! add a store object (a source tree referenced by a path literal, a
//! string written with `toFile`, a derivation instantiated by
//! `derivation`) computes the object's store path exactly as Nix would and
//! records enough to reproduce the object later. The result of an
//! evaluation is a set of derivations plus this map.

use std::collections::{BTreeMap, BTreeSet};

use crate::accessor::{AccessorRegistry, SourcePath};
use crate::canon_path::CanonPath;
use crate::content_address::{
    ContentAddressError, ContentAddressMethod, ContentAddressWithReferences, StoreReferences,
};
use crate::derivation::{
    hash_derivation_modulo, hash_derivation_modulo_with, Derivation, DerivationError,
    DrvHashModulo, DrvHashes,
};
use crate::experimental::XpSettings;
use crate::hash::{Hash, HashAlgo};
use crate::nar::{self, NarError, PathFilter};
use crate::store_path::{StoreDir, StorePath, StorePathError};

#[derive(Debug, thiserror::Error)]
pub enum VirtualStoreError {
    #[error(transparent)]
    StorePath(#[from] StorePathError),
    #[error(transparent)]
    ContentAddress(#[from] ContentAddressError),
    #[error(transparent)]
    Nar(#[from] NarError),
    #[error(transparent)]
    Derivation(#[from] DerivationError),
}

impl VirtualStoreError {
    /// The message as Nix writes it ([`StorePathError::message_bytes`],
    /// [`DerivationError::message_bytes`], [`NarError::message_bytes`]:
    /// the bytes of a name or a path as they are).
    pub fn message_bytes(&self) -> Vec<u8> {
        match self {
            VirtualStoreError::StorePath(e) => e.message_bytes(),
            VirtualStoreError::Derivation(e) => e.message_bytes(),
            VirtualStoreError::ContentAddress(e) => e.message_bytes(),
            VirtualStoreError::Nar(e) => e.message_bytes(),
        }
    }
}

/// How a path is ingested by [`VirtualStore::add_path`]: the content
/// address method, hash algorithm and references. The default is what
/// a path literal gets: a sha256 NAR with no references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathIngestion {
    pub method: ContentAddressMethod,
    pub algo: HashAlgo,
    pub references: BTreeSet<StorePath>,
}

impl Default for PathIngestion {
    fn default() -> Self {
        PathIngestion {
            method: ContentAddressMethod::NixArchive,
            algo: HashAlgo::Sha256,
            references: BTreeSet::new(),
        }
    }
}

/// What a virtual store path stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreObject {
    /// A file system object reachable through an accessor, ingested with
    /// `method`. `filtered` is set when a path filter was applied, in
    /// which case the tree must be re-filtered to reproduce the object.
    SourceTree {
        source: SourcePath,
        method: ContentAddressMethod,
        hash: Hash,
        filtered: bool,
    },
    /// A flat text file (`builtins.toFile`), with references.
    Text {
        contents: Vec<u8>,
        references: BTreeSet<StorePath>,
    },
    /// A derivation (`builtins.derivation`).
    Derivation(Derivation),
}

/// The virtual store, keyed by store path.
#[derive(Default)]
pub struct VirtualStore {
    store_dir: StoreDir,
    /// The experimental features derivations are checked against.
    xp: XpSettings,
    objects: BTreeMap<StorePath, StoreObject>,
    drv_hashes: DrvHashes,
}

impl VirtualStore {
    pub fn new(store_dir: StoreDir, xp: XpSettings) -> VirtualStore {
        VirtualStore {
            store_dir,
            xp,
            objects: BTreeMap::new(),
            drv_hashes: DrvHashes::new(),
        }
    }

    pub fn store_dir(&self) -> &StoreDir {
        &self.store_dir
    }

    pub fn xp(&self) -> XpSettings {
        self.xp
    }

    pub fn get(&self, path: &StorePath) -> Option<&StoreObject> {
        self.objects.get(path)
    }

    pub fn contains(&self, path: &StorePath) -> bool {
        self.objects.contains_key(path)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&StorePath, &StoreObject)> {
        self.objects.iter()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The derivation at `path`, if the store holds one.
    pub fn derivation(&self, path: &StorePath) -> Option<&Derivation> {
        match self.objects.get(path) {
            Some(StoreObject::Derivation(d)) => Some(d),
            _ => None,
        }
    }

    /// `Store::addToStore` without the writing: hash `source` as
    /// `ingestion` says, compute the store path, and remember the object.
    /// The filter receives each entry's absolute path on the accessor, as
    /// `builtins.filterSource` expects. The name is bytes, as Nix's
    /// (`builtins.path`'s `name`): one that is not a valid store path name
    /// is refused once the path is computed, with Nix's message.
    pub fn add_path<N: AsRef<[u8]> + ?Sized>(
        &mut self,
        registry: &AccessorRegistry,
        source: &SourcePath,
        name: &N,
        ingestion: &PathIngestion,
        filter: Option<&mut PathFilter<'_>>,
    ) -> Result<StorePath, VirtualStoreError> {
        let (method, algo) = (ingestion.method, ingestion.algo);
        let (accessor, path) = source.resolve(registry);
        let filtered = filter.is_some();
        // An accessor that already knows the tree's NAR hash (a compiled
        // input image) answers without a traversal. Only an unfiltered
        // recursive SHA-256 ingestion is that hash; anything else is
        // computed as usual.
        // A tree that remembers its NAR hash by fingerprint (Nix's
        // `fetchToStore` cache) has that one.
        let precomputed = if !filtered
            && method == ContentAddressMethod::NixArchive
            && algo == HashAlgo::Sha256
        {
            match accessor.precomputed_nar_hash(path) {
                Some((hash, _size)) => Some(hash),
                None => accessor.remembered_nar_hash(path).map_err(NarError::from)?,
            }
        } else {
            None
        };
        let hash = match precomputed {
            Some(hash) => hash,
            None => {
                let mut accept_all = |_: &CanonPath| Ok(true);
                let filter: &mut PathFilter<'_> = match filter {
                    Some(f) => f,
                    None => &mut accept_all,
                };
                nar::hash_path(accessor, path, method.ingestion_method(), algo, filter)?.0
            }
        };
        self.add_hashed_path(source, name, ingestion, hash, filtered)
    }

    /// [`VirtualStore::add_path`] for a hash computed elsewhere, as
    /// `ingestion` says: for a filtered copy, whose filter is Nix code that
    /// may add paths itself while the tree is walked, and for a tree whose
    /// NAR hash is known.
    pub fn add_hashed_path<N: AsRef<[u8]> + ?Sized>(
        &mut self,
        source: &SourcePath,
        name: &N,
        ingestion: &PathIngestion,
        hash: Hash,
        filtered: bool,
    ) -> Result<StorePath, VirtualStoreError> {
        let store_path = self.store_dir.make_fixed_output_path_from_ca(
            name,
            &ContentAddressWithReferences::from_parts(
                ingestion.method,
                hash,
                StoreReferences {
                    others: ingestion.references.clone(),
                    self_reference: false,
                },
            )?,
        )?;
        self.objects
            .entry(store_path.clone())
            .or_insert(StoreObject::SourceTree {
                source: source.clone(),
                method: ingestion.method,
                hash,
                filtered,
            });
        Ok(store_path)
    }

    /// The store path a tree with a known NAR hash would get, without
    /// reading anything: `makeFixedOutputPath(NixArchive, hash, name)`.
    /// This is how a flake input's `outPath` follows from its `narHash`.
    pub fn add_known_tree(
        &mut self,
        source: &SourcePath,
        name: &str,
        nar_hash: Hash,
    ) -> Result<StorePath, VirtualStoreError> {
        self.add_hashed_path(source, name, &PathIngestion::default(), nar_hash, false)
    }

    /// `addTextToStore` / `builtins.toFile`: a text object with references.
    /// The name is bytes (see [`VirtualStore::add_path`]).
    pub fn add_text<N: AsRef<[u8]> + ?Sized>(
        &mut self,
        name: &N,
        contents: &[u8],
        references: BTreeSet<StorePath>,
    ) -> Result<StorePath, VirtualStoreError> {
        let hash = Hash::of(HashAlgo::Sha256, contents);
        let store_path = self.store_dir.make_text_path(name, &hash, &references)?;
        self.objects
            .entry(store_path.clone())
            .or_insert(StoreObject::Text {
                contents: contents.to_vec(),
                references,
            });
        Ok(store_path)
    }

    /// `writeDerivation`: compute the `.drv` path, memoise the hash modulo
    /// (as `derivationStrict` does), and remember the derivation. The
    /// derivation's outputs must already be filled in (see
    /// [`VirtualStore::fill_in_output_paths`]).
    pub fn add_derivation(&mut self, drv: Derivation) -> Result<StorePath, VirtualStoreError> {
        self.hash_and_add(drv, false)
    }

    /// What `derivationStrict` does with an input-addressed derivation:
    /// [`VirtualStore::fill_in_output_paths`], then
    /// [`VirtualStore::add_derivation`]. When its outputs are all
    /// deferred with empty env vars, as `derivationStrict` builds them,
    /// the derivation is serialised once instead of three times (see
    /// `Derivation::fill_in_deferred`); the results are the same.
    pub fn fill_in_and_add_derivation(
        &mut self,
        drv: Derivation,
    ) -> Result<StorePath, VirtualStoreError> {
        self.hash_and_add(drv, true)
    }

    fn hash_and_add(
        &mut self,
        drv: Derivation,
        fill_in: bool,
    ) -> Result<StorePath, VirtualStoreError> {
        let inputs = self.memoise_input_hashes(&drv);
        let hashed =
            HashedDerivation::new(&self.store_dir, self.xp, drv, fill_in, &inputs, |path| {
                self.is_hashed_derivation(path)
            })?;
        Ok(self.add_hashed_derivation(hashed))
    }

    /// The hashes modulo of `drv`'s input derivations, if reading the
    /// store is enough to tell them: `None` when one has yet to be
    /// computed, which [`VirtualStore::memoise_input_hashes`] does.
    ///
    /// With [`HashedDerivation::new`] and
    /// [`VirtualStore::add_hashed_derivation`] this is
    /// [`VirtualStore::fill_in_and_add_derivation`] in three steps, of
    /// which the expensive middle one needs no access to the store: an
    /// evaluator with several threads serialises and hashes derivations
    /// outside its store lock.
    pub fn input_hashes(&self, drv: &Derivation) -> Option<InputHashes> {
        let mut inputs = Vec::with_capacity(drv.input_drvs.len());
        for path in drv.input_drvs.keys() {
            let hash = match self.drv_hashes.get(path) {
                Some(hash) => Ok(hash.clone()),
                None if matches!(self.objects.get(path), Some(StoreObject::Derivation(_))) => {
                    return None
                }
                None => Err(DerivationError::UnknownInputDerivation(
                    self.store_dir.display(path),
                )),
            };
            inputs.push((path.clone(), hash));
        }
        Some(InputHashes(inputs))
    }

    /// [`VirtualStore::input_hashes`], computing and memoising the ones
    /// not known yet.
    pub fn memoise_input_hashes(&mut self, drv: &Derivation) -> InputHashes {
        let objects = &self.objects;
        let (store_dir, xp) = (&self.store_dir, self.xp);
        let memo = &mut self.drv_hashes;
        InputHashes(
            drv.input_drvs
                .keys()
                .map(|path| {
                    let hash = path_derivation_modulo(store_dir, xp, objects, memo, path);
                    (path.clone(), hash)
                })
                .collect(),
        )
    }

    /// Whether the derivation at `path` is in the store with its hash
    /// modulo memoised, so that adding it again changes nothing.
    pub fn is_hashed_derivation(&self, path: &StorePath) -> bool {
        self.drv_hashes.contains_key(path) && self.objects.contains_key(path)
    }

    /// Remember a derivation and its hash modulo; the first one added at
    /// a path stays.
    pub fn add_hashed_derivation(&mut self, hashed: HashedDerivation) -> StorePath {
        let HashedDerivation { drv, path, hash } = hashed;
        if let Some(hash) = hash {
            self.drv_hashes.entry(path.clone()).or_insert(hash);
        }
        self.objects
            .entry(path.clone())
            .or_insert(StoreObject::Derivation(drv));
        path
    }

    /// `hashDerivationModulo` with input derivations looked up in this
    /// store and memoised.
    pub fn hash_derivation_modulo(
        &mut self,
        drv: &Derivation,
        mask_outputs: bool,
    ) -> Result<DrvHashModulo, VirtualStoreError> {
        let objects = &self.objects;
        let (store_dir, xp) = (&self.store_dir, self.xp);
        let memo = &mut self.drv_hashes;
        Ok(hash_derivation_modulo_with(
            store_dir,
            drv,
            mask_outputs,
            xp,
            &mut |path| path_derivation_modulo(store_dir, xp, objects, memo, path),
            &mut Vec::new(),
        )?)
    }

    /// `Derivation::fillInOutputPaths` with inputs from this store.
    pub fn fill_in_output_paths(&mut self, drv: &mut Derivation) -> Result<(), VirtualStoreError> {
        let objects = &self.objects;
        let (store_dir, xp) = (&self.store_dir, self.xp);
        let memo = &mut self.drv_hashes;
        drv.fill_in_output_paths(store_dir, xp, &mut |path| {
            path_derivation_modulo(store_dir, xp, objects, memo, path)
        })?;
        Ok(())
    }

    /// The hash modulo of a derivation already in the store, if known.
    pub fn drv_hash(&self, path: &StorePath) -> Option<&DrvHashModulo> {
        self.drv_hashes.get(path)
    }
}

/// The hashes modulo of a derivation's input derivations (or why one has
/// none), by path: what hashing the derivation needs from the store.
pub struct InputHashes(Vec<(StorePath, Result<DrvHashModulo, DerivationError>)>);

impl InputHashes {
    fn get(
        &self,
        store_dir: &StoreDir,
        path: &StorePath,
    ) -> Result<DrvHashModulo, DerivationError> {
        // In path order, as `Derivation::input_drvs` is.
        match self.0.binary_search_by(|(input, _)| input.cmp(path)) {
            Ok(index) => self.0[index].1.clone(),
            Err(_) => Err(DerivationError::UnknownInputDerivation(
                store_dir.display(path),
            )),
        }
    }
}

thread_local! {
    /// Reused for every ATerm serialisation on this thread, so hashing a
    /// derivation allocates no text buffer after the first.
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A derivation with its store path and hash modulo worked out, ready for
/// [`VirtualStore::add_hashed_derivation`].
pub struct HashedDerivation {
    drv: Derivation,
    path: StorePath,
    /// `None` when the store already had it.
    hash: Option<DrvHashModulo>,
}

impl HashedDerivation {
    /// Serialise and hash `drv`, first filling in its output paths if
    /// `fill_in`. `known` says whether the store already has the
    /// derivation at a path with its hash modulo
    /// ([`VirtualStore::is_hashed_derivation`]), which saves hashing it
    /// again.
    pub fn new(
        store_dir: &StoreDir,
        xp: XpSettings,
        mut drv: Derivation,
        fill_in: bool,
        inputs: &InputHashes,
        known: impl FnOnce(&StorePath) -> bool,
    ) -> Result<HashedDerivation, VirtualStoreError> {
        let resolve = &mut |path: &StorePath| inputs.get(store_dir, path);
        SCRATCH.with_borrow_mut(|scratch| {
            if fill_in {
                if let Some((path, layout)) = drv.fill_in_deferred(store_dir, resolve, scratch)? {
                    let hash = (!known(&path))
                        .then(|| DrvHashModulo::DrvHash(layout.hash_modulo(scratch)));
                    return Ok(HashedDerivation { drv, path, hash });
                }
                drv.fill_in_output_paths(store_dir, xp, resolve)?;
            }
            let path = drv.store_path_with(store_dir, scratch)?;
            let hash = if known(&path) {
                None
            } else {
                Some(hash_derivation_modulo_with(
                    store_dir, &drv, false, xp, resolve, scratch,
                )?)
            };
            Ok(HashedDerivation { drv, path, hash })
        })
    }

    pub fn path(&self) -> &StorePath {
        &self.path
    }

    /// Whether the store already had this derivation (`known` said so),
    /// so that adding it would change nothing.
    pub fn is_stored(&self) -> bool {
        self.hash.is_none()
    }
}

/// `pathDerivationModulo`: look a derivation up by path, memoising its
/// hash modulo. Mutual recursion with `hash_derivation_modulo` is through
/// this closure.
fn path_derivation_modulo(
    store_dir: &StoreDir,
    xp: XpSettings,
    objects: &BTreeMap<StorePath, StoreObject>,
    memo: &mut DrvHashes,
    path: &StorePath,
) -> Result<DrvHashModulo, DerivationError> {
    if let Some(h) = memo.get(path) {
        return Ok(h.clone());
    }
    let Some(StoreObject::Derivation(drv)) = objects.get(path) else {
        return Err(DerivationError::UnknownInputDerivation(
            store_dir.display(path),
        ));
    };
    let h = hash_derivation_modulo(store_dir, drv, false, xp, &mut |p| {
        path_derivation_modulo(store_dir, xp, objects, memo, p)
    })?;
    memo.insert(path.clone(), h.clone());
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accessor::MemoryAccessor;
    use crate::derivation::DerivationOutput;
    use bstr::BString;
    use std::sync::Arc;

    #[test]
    fn text_and_tree_paths() {
        // `nix eval --store dummy:// --expr 'builtins.toFile "foo" "bar"'`
        // gives /nix/store/vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo.
        let mut store = VirtualStore::default();
        let p1 = store.add_text("foo", b"bar", BTreeSet::new()).unwrap();
        assert_eq!(p1.as_str(), "vxjiwkjkn7x4079qvh1jkl5pn05j2aw0-foo");
        let p2 = store.add_text("foo", b"bar", BTreeSet::new()).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(store.len(), 1);
        assert_eq!(p1.name(), "foo");
        let mut reg = AccessorRegistry::new();
        let mut m = MemoryAccessor::new();
        m.add_file(&CanonPath::new("/a").unwrap(), "x", false)
            .unwrap();
        let id = reg.register(Arc::new(m));
        let src = SourcePath::new(id, CanonPath::root());
        let tree = store
            .add_path(&reg, &src, "src", &PathIngestion::default(), None)
            .unwrap();
        let Some(StoreObject::SourceTree { hash, .. }) = store.get(&tree) else {
            panic!("expected a source tree")
        };
        assert_eq!(store.add_known_tree(&src, "src", *hash).unwrap(), tree);
    }

    /// A tree's names are bytes; so is what an error shows its paths as.
    #[test]
    fn trees_with_byte_names() {
        let mut store = VirtualStore::default();
        let mut reg = AccessorRegistry::new();
        let mut m = MemoryAccessor::new();
        m.set_display_prefix(b"/d\xe9");
        m.add_file(&CanonPath::new(b"/caf\xe9").unwrap(), "x", false)
            .unwrap();
        m.add_symlink(&CanonPath::new(b"/l\xff").unwrap(), b"t\xe9")
            .unwrap();
        let (nar_hash, _) = crate::nar::hash(&m, &CanonPath::root(), HashAlgo::Sha256).unwrap();
        let id = reg.register(Arc::new(m));
        let tree = store
            .add_path(
                &reg,
                &SourcePath::new(id, CanonPath::root()),
                "src",
                &PathIngestion::default(),
                None,
            )
            .unwrap();
        let Some(StoreObject::SourceTree { hash, .. }) = store.get(&tree) else {
            panic!("expected a source tree")
        };
        assert_eq!(*hash, nar_hash);
        let missing = SourcePath::new(id, CanonPath::new(b"/nope\xe9").unwrap());
        let err = store
            .add_path(&reg, &missing, "src", &PathIngestion::default(), None)
            .unwrap_err();
        assert_eq!(
            err.message_bytes(),
            b"path '/d\xe9/nope\xe9' does not exist"
        );
    }

    #[test]
    fn derivations_chain_through_hash_modulo() {
        let mut store = VirtualStore::default();
        let mk = |name: &str| {
            let mut d = Derivation {
                name: name.into(),
                platform: "x86_64-linux".into(),
                builder: "/bin/sh".into(),
                ..Default::default()
            };
            d.env.insert("name".into(), name.into());
            d.env.insert("out".into(), "".into());
            d.outputs.insert("out".into(), DerivationOutput::Deferred);
            d
        };
        let mut a = mk("a");
        store.fill_in_output_paths(&mut a).unwrap();
        let a_path = store.add_derivation(a.clone()).unwrap();
        assert!(matches!(
            store.drv_hash(&a_path),
            Some(DrvHashModulo::DrvHash(_))
        ));
        let mut b = mk("b");
        b.input_drvs
            .insert(a_path.clone(), BTreeSet::from([BString::from("out")]));
        store.fill_in_output_paths(&mut b).unwrap();
        let DerivationOutput::InputAddressed(b_out) = &b.outputs[b"out".as_slice()] else {
            panic!("b should be input-addressed")
        };
        assert_eq!(b.env["out".as_bytes()], store.store_dir().display(b_out));
        let b_path = store.add_derivation(b).unwrap();
        assert!(b_path.is_derivation());
        // The single-serialisation path gives the same derivation, path
        // and hash modulo, here for derivations with several outputs,
        // escapes, structured attrs, input sources that are also input
        // derivations, an input with no outputs used, and two fixed-output
        // inputs with the same output (so the same hash modulo).
        let fixed = |url: &str| {
            let mut d = mk("src");
            d.env.insert("url".into(), url.into());
            d.outputs.insert(
                "out".into(),
                DerivationOutput::CAFixed(crate::ContentAddress {
                    method: ContentAddressMethod::Flat,
                    hash: Hash::of(HashAlgo::Sha256, b"tarball"),
                }),
            );
            d
        };
        let mut fused = VirtualStore::default();
        let mut b_again = mk("b");
        b_again
            .input_drvs
            .insert(a_path.clone(), BTreeSet::from([BString::from("out")]));
        for d in [a.clone(), b_again, fixed("https://a"), fixed("https://b")] {
            let mut general = d.clone();
            store.fill_in_output_paths(&mut general).unwrap();
            store.add_derivation(general).unwrap();
            let mut general = d;
            fused.fill_in_output_paths(&mut general).unwrap();
            fused.add_derivation(general).unwrap();
        }
        let fixed_paths: Vec<StorePath> = store
            .iter()
            .filter(|(_, o)| matches!(o, StoreObject::Derivation(d) if d.name == "src"))
            .map(|(p, _)| p.clone())
            .collect();
        assert_eq!(fixed_paths.len(), 2);
        let mut d = mk("multi \"quoted\"");
        d.name = "multi".into();
        for o in ["dev", "doc", "out"] {
            d.env.insert(o.into(), "".into());
            d.outputs.insert(o.into(), DerivationOutput::Deferred);
        }
        d.env.insert(
            "script".into(),
            "echo \"a\\tb\"\n\tcd $out\r\n\u{1}\u{b}é".into(),
        );
        d.env.insert("__json".into(), "{\"k\":[1,\"\\n\"]}".into());
        d.args = vec!["-c".into(), "x\ny".into()];
        d.input_drvs
            .insert(a_path.clone(), BTreeSet::from([BString::from("out")]));
        d.input_drvs.insert(b_path.clone(), BTreeSet::new());
        for p in &fixed_paths {
            d.input_drvs
                .insert(p.clone(), BTreeSet::from([BString::from("out")]));
        }
        d.input_srcs.insert(a_path.clone());
        d.input_srcs
            .insert(StorePath::new("00000000000000000000000000000000-src").unwrap());
        let mut general = d.clone();
        store.fill_in_output_paths(&mut general).unwrap();
        let general_path = store.add_derivation(general.clone()).unwrap();
        let fused_path = fused.fill_in_and_add_derivation(d).unwrap();
        assert_eq!(fused_path, general_path);
        assert_eq!(fused.derivation(&fused_path), Some(&general));
        assert_eq!(fused.drv_hash(&fused_path), store.drv_hash(&general_path));
        assert!(matches!(
            fused.drv_hash(&fused_path),
            Some(DrvHashModulo::DrvHash(_))
        ));

        // An unknown input is an error, not a silent guess.
        let mut c = mk("c");
        c.input_drvs.insert(
            StorePath::new("ffffffffffffffffffffffffffffffff-nope.drv").unwrap(),
            BTreeSet::from([BString::from("out")]),
        );
        assert!(store.fill_in_output_paths(&mut c).is_err());
    }

    /// What an evaluator with several threads does instead of
    /// `fill_in_and_add_derivation`: look the inputs up, hash without the
    /// store, record the result.
    fn add_in_steps(
        store: &mut VirtualStore,
        drv: Derivation,
        fill_in: bool,
    ) -> Result<(StorePath, bool), VirtualStoreError> {
        let inputs = match store.input_hashes(&drv) {
            Some(inputs) => inputs,
            None => store.memoise_input_hashes(&drv),
        };
        let hashed = HashedDerivation::new(
            &store.store_dir().clone(),
            store.xp(),
            drv,
            fill_in,
            &inputs,
            |p| store.is_hashed_derivation(p),
        )?;
        let stored = hashed.is_stored();
        Ok((store.add_hashed_derivation(hashed), stored))
    }

    #[test]
    fn hashing_outside_the_store_adds_the_same_derivations() {
        let mk = |name: &str, inputs: &[&StorePath]| {
            let mut d = Derivation {
                name: name.into(),
                platform: "x86_64-linux".into(),
                builder: "/bin/sh".into(),
                ..Default::default()
            };
            d.env.insert("name".into(), name.into());
            d.env.insert("out".into(), "".into());
            d.outputs.insert("out".into(), DerivationOutput::Deferred);
            for input in inputs {
                d.input_drvs
                    .insert((*input).clone(), BTreeSet::from([BString::from("out")]));
            }
            d
        };
        let fixed = |inputs: &[&StorePath]| {
            let mut d = mk("src", inputs);
            d.outputs.insert(
                "out".into(),
                DerivationOutput::CAFixed(crate::ContentAddress {
                    method: ContentAddressMethod::Flat,
                    hash: Hash::of(HashAlgo::Sha256, b"tarball"),
                }),
            );
            d
        };
        let unknown = StorePath::new("ffffffffffffffffffffffffffffffff-nope.drv").unwrap();

        let mut whole = VirtualStore::default();
        let mut steps = VirtualStore::default();
        let a = whole.fill_in_and_add_derivation(mk("a", &[])).unwrap();
        assert_eq!(
            add_in_steps(&mut steps, mk("a", &[]), true).unwrap(),
            (a.clone(), false)
        );
        // A fixed-output derivation never asks for its inputs' hashes, so
        // an input the store has not got is no error; its outputs are not
        // filled in.
        let src = whole.add_derivation(fixed(&[&unknown])).unwrap();
        assert_eq!(
            add_in_steps(&mut steps, fixed(&[&unknown]), false).unwrap(),
            (src.clone(), false)
        );
        let b = whole
            .fill_in_and_add_derivation(mk("b", &[&a, &src]))
            .unwrap();

        // An input whose hash modulo is not memoised yet (as after
        // reading it from elsewhere) takes the store's own computation.
        steps.drv_hashes.remove(&a);
        assert!(steps.input_hashes(&mk("b", &[&a, &src])).is_none());
        assert_eq!(
            add_in_steps(&mut steps, mk("b", &[&a, &src]), true).unwrap(),
            (b.clone(), false)
        );
        assert!(steps.input_hashes(&mk("b", &[&a, &src])).is_some());

        for path in [&a, &src, &b] {
            assert_eq!(steps.derivation(path), whole.derivation(path));
            assert_eq!(steps.drv_hash(path), whole.drv_hash(path));
        }
        assert_eq!(steps.len(), whole.len());

        // Again: found in the store, with nothing to add.
        assert_eq!(
            add_in_steps(&mut steps, mk("b", &[&a, &src]), true).unwrap(),
            (b.clone(), true)
        );
        assert_eq!(
            add_in_steps(&mut steps, fixed(&[&unknown]), false).unwrap(),
            (src, true)
        );
        assert_eq!(steps.len(), whole.len());

        // An unknown input that is asked for is the same error either way.
        let error =
            |result: Result<_, VirtualStoreError>| result.map(|_| ()).unwrap_err().message_bytes();
        assert_eq!(
            error(add_in_steps(&mut steps, mk("c", &[&unknown]), true)),
            error(
                whole
                    .fill_in_and_add_derivation(mk("c", &[&unknown]))
                    .map(|p| (p, false))
            ),
        );
    }
}
