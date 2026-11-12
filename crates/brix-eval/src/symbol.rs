//! Interned attribute names and identifiers.
//!
//! Nix's `SymbolTable` hands out small integer ids in creation order. A
//! [`Symbol`] is such an id, and `Ord` on it is creation order, which is
//! what Nix sorts its `Bindings` by, and so does brix (see
//! [`crate::bindings`]). The table pre-creates the symbols Nix creates in
//! its `EvalState` constructor, in the same order, and the compiler and
//! the primops intern names when Nix does, so the places where creation
//! order is observable (which attribute `==`, a deep force or a primop's
//! argument loop visits first) match.
//!
//! Name order is needed too, where Nix sorts by name (`attrNames`, the
//! printers, `derivationStrict`): [`SymbolKeys`] stores, for every
//! symbol, the first eight bytes of its name as a big-endian integer, so
//! comparing two names is one integer comparison, falling back to the full
//! names only when the prefixes tie.

use crate::runtime::cell::Cell;

use crate::runtime::Bump;
use hashbrown::HashMap;

/// An interned name. `Ord` is creation order, as in Nix; name order goes
/// through `SymbolKeys` (see [`crate::bindings::Bindings::lexicographic`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(pub u32);

impl std::fmt::Debug for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Symbol({})", self.0)
    }
}

/// One entry of [`SymbolKeys`]: the name and its comparison prefix.
#[derive(Clone, Copy)]
struct Entry<'a> {
    /// The first eight bytes of the name, big-endian, zero padded. Equal
    /// prefixes order like equal names would, so comparing prefixes is a
    /// correct first step of a lexicographic comparison.
    prefix: u64,
    name: &'a [u8],
}

/// One chunk of the key table.
type Chunk<'a> = &'a [Cell<Entry<'a>>];

/// Symbols per chunk of the key table. Chunks are allocated on demand in
/// the arena, so the table grows without moving entries and can be read
/// through a shared reference while new symbols are being created.
const CHUNK: usize = 4096;
/// Chunk slots: `CHUNK * CHUNKS` symbols in total (16 million, far above
/// the ~100k names a nixpkgs evaluation creates).
const CHUNKS: usize = 4096;

/// The names of all symbols, readable through `&self` from anywhere a
/// [`Symbol`] is compared or printed, and appendable through `&self` by
/// [`SymbolTable::intern`]. One per evaluator: the operations that order
/// names ([`crate::bindings::Bindings::lexicographic`] and friends) take
/// it as an argument rather than every set pointing to it.
pub struct SymbolKeys<'a> {
    arena: &'a Bump,
    /// In the arena rather than a `Box`: the arena never runs destructors,
    /// so a boxed table would leak 64 KiB per evaluator.
    chunks: &'a [Cell<Option<Chunk<'a>>>],
    len: Cell<u32>,
}

impl<'a> SymbolKeys<'a> {
    pub fn new(arena: &'a Bump) -> &'a SymbolKeys<'a> {
        arena.alloc(SymbolKeys {
            arena,
            chunks: arena.alloc_slice_fill_with(CHUNKS, |_| Cell::new(None)),
            len: Cell::new(0),
        })
    }

    /// The comparison prefix of a name: its first eight bytes as a
    /// big-endian integer, zero padded. `a` and `ab` give `0x61000…` and
    /// `0x6162000…`, so shorter names sort first, as in a byte comparison.
    pub fn prefix_of(name: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        let n = name.len().min(8);
        buf[..n].copy_from_slice(&name[..n]);
        u64::from_be_bytes(buf)
    }

    fn push(&self, name: &'a [u8]) -> Symbol {
        let id = self.len.get();
        let (chunk_ix, ix) = (id as usize / CHUNK, id as usize % CHUNK);
        assert!(chunk_ix < CHUNKS, "too many symbols");
        let chunk = match self.chunks[chunk_ix].get() {
            Some(c) => c,
            None => {
                let empty = Entry {
                    prefix: 0,
                    name: &[],
                };
                let c: Chunk<'a> = self
                    .arena
                    .alloc_slice_fill_with(CHUNK, |_| Cell::new(empty));
                self.chunks[chunk_ix].set(Some(c));
                c
            }
        };
        chunk[ix].set(Entry {
            prefix: Self::prefix_of(name),
            name,
        });
        self.len.set(id + 1);
        Symbol(id)
    }

    #[inline]
    fn entry(&self, sym: Symbol) -> Entry<'a> {
        let (chunk_ix, ix) = (sym.0 as usize / CHUNK, sym.0 as usize % CHUNK);
        self.chunks[chunk_ix]
            .get()
            .expect("symbol from another table")[ix]
            .get()
    }

    /// The name of a symbol.
    #[inline]
    pub fn name(&self, sym: Symbol) -> &'a [u8] {
        self.entry(sym).name
    }

    /// The comparison prefix of a symbol's name.
    #[inline]
    pub fn prefix(&self, sym: Symbol) -> u64 {
        self.entry(sym).prefix
    }

    pub fn len(&self) -> usize {
        self.len.get() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }
}

/// The symbol table: interning on top of [`SymbolKeys`]. Names are bytes:
/// attribute names can come from any string value, and Nix strings need
/// not be UTF-8.
pub struct SymbolTable<'a> {
    keys: &'a SymbolKeys<'a>,
    map: HashMap<&'a [u8], Symbol>,
}

impl<'a> SymbolTable<'a> {
    pub fn new(keys: &'a SymbolKeys<'a>) -> SymbolTable<'a> {
        assert!(keys.is_empty(), "a SymbolTable owns its SymbolKeys");
        let mut table = SymbolTable {
            keys,
            map: HashMap::new(),
        };
        for name in STATIC_SYMBOLS {
            table.intern(name.as_bytes());
        }
        table
    }

    /// Returns the symbol for `name`, creating it if needed.
    pub fn intern(&mut self, name: &[u8]) -> Symbol {
        if let Some(&sym) = self.map.get(name) {
            return sym;
        }
        let stored: &'a [u8] = self.keys.arena.alloc_slice_copy(name);
        let sym = self.keys.push(stored);
        self.map.insert(stored, sym);
        sym
    }

    /// Looks `name` up without creating it.
    #[cfg(any(test, brix_parallel))]
    pub fn get(&self, name: &[u8]) -> Option<Symbol> {
        self.map.get(name).copied()
    }

    /// The name of a symbol.
    pub fn name(&self, sym: Symbol) -> &'a [u8] {
        self.keys.name(sym)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }
}

/// The symbols Nix creates before parsing anything (`StaticEvalSymbols`
/// in `eval.hh`, followed by `Expr::AstSymbols`). Their ids are fixed by
/// their index here, so they can be referred to as constants.
pub const STATIC_SYMBOLS: &[&str] = &[
    "<with>",
    "outPath",
    "drvPath",
    "type",
    "meta",
    "name",
    "value",
    "system",
    "__overrides",
    "outputs",
    "outputName",
    "__ignoreNulls",
    "file",
    "line",
    "column",
    "__functor",
    "__toString",
    "right",
    "wrong",
    "__structuredAttrs",
    "__json",
    "allowedReferences",
    "allowedRequisites",
    "disallowedReferences",
    "disallowedRequisites",
    "maxSize",
    "maxClosureSize",
    "builder",
    "args",
    "__contentAddressed",
    "__impure",
    "outputHash",
    "outputHashAlgo",
    "outputHashMode",
    "recurseForDerivations",
    "description",
    "self",
    "",
    "startSet",
    "operator",
    "key",
    "path",
    "prefix",
    "outputSpecified",
    "__meta",
    "__sub",
    "__lessThan",
    "__mul",
    "__div",
    "or",
    "__findFile",
    "__nixPath",
    "body",
];

/// Well-known symbols, by their fixed index in [`STATIC_SYMBOLS`].
pub mod sym {
    use super::Symbol;
    pub const OUT_PATH: Symbol = Symbol(1);
    pub const DRV_PATH: Symbol = Symbol(2);
    pub const TYPE: Symbol = Symbol(3);
    pub const NAME: Symbol = Symbol(5);
    pub const VALUE: Symbol = Symbol(6);
    pub const SYSTEM: Symbol = Symbol(7);
    pub const OVERRIDES: Symbol = Symbol(8);
    pub const OUTPUTS: Symbol = Symbol(9);
    pub const IGNORE_NULLS: Symbol = Symbol(11);
    pub const FILE: Symbol = Symbol(12);
    pub const LINE: Symbol = Symbol(13);
    pub const COLUMN: Symbol = Symbol(14);
    pub const FUNCTOR: Symbol = Symbol(15);
    pub const TO_STRING: Symbol = Symbol(16);
    pub const RIGHT: Symbol = Symbol(17);
    pub const WRONG: Symbol = Symbol(18);
    pub const STRUCTURED_ATTRS: Symbol = Symbol(19);
    pub const JSON: Symbol = Symbol(20);
    pub const ALLOWED_REFERENCES: Symbol = Symbol(21);
    pub const ALLOWED_REQUISITES: Symbol = Symbol(22);
    pub const DISALLOWED_REFERENCES: Symbol = Symbol(23);
    pub const DISALLOWED_REQUISITES: Symbol = Symbol(24);
    pub const MAX_SIZE: Symbol = Symbol(25);
    pub const MAX_CLOSURE_SIZE: Symbol = Symbol(26);
    pub const BUILDER: Symbol = Symbol(27);
    pub const ARGS: Symbol = Symbol(28);
    pub const CONTENT_ADDRESSED: Symbol = Symbol(29);
    pub const IMPURE: Symbol = Symbol(30);
    pub const OUTPUT_HASH: Symbol = Symbol(31);
    pub const OUTPUT_HASH_ALGO: Symbol = Symbol(32);
    pub const OUTPUT_HASH_MODE: Symbol = Symbol(33);
    pub const START_SET: Symbol = Symbol(38);
    pub const OPERATOR: Symbol = Symbol(39);
    pub const KEY: Symbol = Symbol(40);
    pub const PATH: Symbol = Symbol(41);
    pub const PREFIX: Symbol = Symbol(42);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_symbols_have_fixed_ids() {
        let arena = Bump::new();
        let table = SymbolTable::new(SymbolKeys::new(&arena));
        assert_eq!(table.get(b"outPath"), Some(sym::OUT_PATH));
        assert_eq!(table.get(b"prefix"), Some(sym::PREFIX));
        assert_eq!(
            table.get(b"body"),
            Some(Symbol(STATIC_SYMBOLS.len() as u32 - 1))
        );
        assert_eq!(table.name(sym::FUNCTOR), b"__functor");
        assert_eq!(table.len(), STATIC_SYMBOLS.len());
    }

    #[test]
    fn interning_is_idempotent_and_ordered() {
        let arena = Bump::new();
        let mut table = SymbolTable::new(SymbolKeys::new(&arena));
        let a = table.intern(b"zzz");
        let b = table.intern(b"aaa");
        assert!(a < b, "later creation gives a larger id");
        assert_eq!(table.intern(b"zzz"), a);
        assert_eq!(table.name(b), b"aaa");
    }

    #[test]
    fn keys_order_names_lexicographically() {
        let arena = Bump::new();
        let keys = SymbolKeys::new(&arena);
        let mut table = SymbolTable::new(keys);
        let names: [&[u8]; 8] = [
            b"a",
            b"ab",
            b"b",
            b"propagatedBuildInputs",
            b"propagatedNativeBuildInputs",
            b"propagatedB",
            b"",
            b"\xff",
        ];
        let syms: Vec<Symbol> = names.iter().map(|n| table.intern(n)).collect();
        for (i, &a) in syms.iter().enumerate() {
            for (j, &b) in syms.iter().enumerate() {
                // The comparison `Bindings::lexicographic` makes.
                let by_key = (keys.prefix(a), keys.name(a)).cmp(&(keys.prefix(b), keys.name(b)));
                assert_eq!(by_key, names[i].cmp(names[j]), "{i} vs {j}");
            }
        }
        assert_eq!(SymbolKeys::prefix_of(b"a"), 0x6100_0000_0000_0000);
        assert_eq!(
            SymbolKeys::prefix_of(b"abcdefghij"),
            SymbolKeys::prefix_of(b"abcdefgh")
        );
    }

    #[test]
    fn keys_grow_past_one_chunk() {
        let arena = Bump::new();
        let keys = SymbolKeys::new(&arena);
        let mut table = SymbolTable::new(keys);
        let syms: Vec<Symbol> = (0..CHUNK * 2 + 7)
            .map(|i| table.intern(format!("name{i}").as_bytes()))
            .collect();
        for (i, &s) in syms.iter().enumerate() {
            assert_eq!(keys.name(s), format!("name{i}").as_bytes());
        }
    }
}
