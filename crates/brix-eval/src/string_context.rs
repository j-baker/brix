//! The string context accumulated while coercing values to strings.
//!
//! Nix's `coerceToString` takes a `NixStringContext` (a `std::set`) and
//! inserts into it. Almost every coercion in practice contributes no
//! context at all, and when one does it is usually a single string whose
//! context is simply passed through (`"${drv}/bin/foo"`). A set that
//! allocates on first insert is wasteful for both cases, so this
//! accumulator has three shapes:
//!
//! * empty;
//! * a borrowed slice: exactly one string's context has been merged, so
//!   the result can share that string's (immutable, arena-allocated)
//!   context slice without copying;
//! * an owned, small, sorted and deduplicated vector, once two different
//!   contexts have been merged or an element has been inserted.
//!
//! Observable behaviour is the set's: iteration is in sorted order and
//! without duplicates, whatever the shape.

use smallvec::SmallVec;

use crate::value::ContextElem;

/// A string context under construction. See the module documentation.
#[derive(Clone, Debug)]
pub struct StringContext<'a> {
    repr: Repr<'a>,
}

#[derive(Clone, Debug)]
enum Repr<'a> {
    /// Shares a string's context slice. The empty context is
    /// `Borrowed(&[])`.
    Borrowed(&'a [ContextElem<'a>]),
    /// Sorted and deduplicated.
    Owned(SmallVec<[ContextElem<'a>; 4]>),
}

impl<'a> Default for StringContext<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> StringContext<'a> {
    /// The empty context.
    pub fn new() -> Self {
        StringContext {
            repr: Repr::Borrowed(&[]),
        }
    }

    /// A context that shares an existing string's context slice.
    pub fn borrowed(elems: &'a [ContextElem<'a>]) -> Self {
        StringContext {
            repr: Repr::Borrowed(elems),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    /// The elements, sorted and deduplicated.
    pub fn as_slice(&self) -> &[ContextElem<'a>] {
        match &self.repr {
            Repr::Borrowed(s) => s,
            Repr::Owned(v) => v,
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ContextElem<'a>> {
        self.as_slice().iter()
    }

    pub fn contains(&self, elem: &ContextElem<'a>) -> bool {
        self.as_slice().binary_search(elem).is_ok()
    }

    /// Whether any element counts as real context (`builtins.hasContext`).
    pub fn has_real(&self) -> bool {
        self.iter().any(|c| c.is_real())
    }

    /// Inserts one element. Returns whether it was new.
    pub fn insert(&mut self, elem: ContextElem<'a>) -> bool {
        match self.as_slice().binary_search(&elem) {
            Ok(_) => false,
            Err(i) => {
                self.make_owned().insert(i, elem);
                true
            }
        }
    }

    /// Merges a string's context (which is sorted and deduplicated, as
    /// every `NixStr::context` is). The common cases cost nothing: an
    /// empty slice, the first non-empty slice, or the same slice again.
    pub fn extend_from_slice(&mut self, elems: &'a [ContextElem<'a>]) {
        // A lazy hash's offset belongs to its own string (they sort first).
        assert!(
            !matches!(elems.first(), Some(ContextElem::LazyHash { .. })),
            "a string's lazy hashes were merged as context"
        );
        if elems.is_empty() {
            return;
        }
        if let Repr::Borrowed(cur) = self.repr {
            if cur.is_empty() {
                self.repr = Repr::Borrowed(elems);
                return;
            }
            if std::ptr::eq(cur, elems) {
                return;
            }
        }
        // Both sides are sorted: merge them.
        let cur = std::mem::replace(&mut self.repr, Repr::Borrowed(&[]));
        let cur: SmallVec<[ContextElem<'a>; 4]> = match cur {
            Repr::Borrowed(s) => SmallVec::from_slice(s),
            Repr::Owned(v) => v,
        };
        let mut merged = SmallVec::with_capacity(cur.len() + elems.len());
        let (mut i, mut j) = (0, 0);
        while i < cur.len() && j < elems.len() {
            match cur[i].cmp(&elems[j]) {
                std::cmp::Ordering::Less => {
                    merged.push(cur[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    merged.push(elems[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    merged.push(cur[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        merged.extend_from_slice(&cur[i..]);
        merged.extend_from_slice(&elems[j..]);
        self.repr = Repr::Owned(merged);
    }

    /// Merges another accumulator into this one.
    pub fn extend_from(&mut self, other: &StringContext<'a>) {
        match &other.repr {
            Repr::Borrowed(s) => self.extend_from_slice(s),
            Repr::Owned(v) => {
                for &e in v.iter() {
                    self.insert(e);
                }
            }
        }
    }

    /// Inserts every element of an iterator.
    pub fn extend<I: IntoIterator<Item = ContextElem<'a>>>(&mut self, iter: I) {
        for e in iter {
            self.insert(e);
        }
    }

    /// The context as an arena slice: the borrowed slice itself when one
    /// string's context was merged, otherwise a copy of the owned vector.
    pub fn finish(&self, bump: &'a crate::runtime::Bump) -> &'a [ContextElem<'a>] {
        match &self.repr {
            Repr::Borrowed(s) => s,
            Repr::Owned(v) if v.is_empty() => &[],
            Repr::Owned(v) => bump.alloc_slice_copy(v),
        }
    }

    fn make_owned(&mut self) -> &mut SmallVec<[ContextElem<'a>; 4]> {
        if let Repr::Borrowed(s) = self.repr {
            self.repr = Repr::Owned(SmallVec::from_slice(s));
        }
        match &mut self.repr {
            Repr::Owned(v) => v,
            Repr::Borrowed(_) => unreachable!("made owned above"),
        }
    }
}

impl<'a> FromIterator<ContextElem<'a>> for StringContext<'a> {
    fn from_iter<I: IntoIterator<Item = ContextElem<'a>>>(iter: I) -> Self {
        let mut ctx = StringContext::new();
        ctx.extend(iter);
        ctx
    }
}

impl<'a, 'b> IntoIterator for &'b StringContext<'a> {
    type Item = &'b ContextElem<'a>;
    type IntoIter = std::slice::Iter<'b, ContextElem<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for StringContext<'a> {
    type Item = ContextElem<'a>;
    type IntoIter = smallvec::IntoIter<[ContextElem<'a>; 4]>;

    fn into_iter(self) -> Self::IntoIter {
        match self.repr {
            Repr::Borrowed(s) => SmallVec::from_slice(s).into_iter(),
            Repr::Owned(v) => v.into_iter(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque(s: &str) -> ContextElem<'_> {
        ContextElem::Opaque(s)
    }

    #[test]
    fn empty_and_single_slice_share() {
        let bump = crate::runtime::Bump::new();
        let slice: &[ContextElem<'_>] = bump.alloc_slice_copy(&[opaque("a"), opaque("b")]);
        let mut ctx = StringContext::new();
        assert!(ctx.is_empty());
        ctx.extend_from_slice(&[]);
        assert!(ctx.is_empty());
        ctx.extend_from_slice(slice);
        assert!(std::ptr::eq(ctx.finish(&bump), slice));
        ctx.extend_from_slice(slice);
        assert!(std::ptr::eq(ctx.finish(&bump), slice));
        assert_eq!(ctx.len(), 2);
    }

    #[test]
    fn merging_sorts_and_dedups() {
        let bump = crate::runtime::Bump::new();
        let a: &[ContextElem<'_>] = bump.alloc_slice_copy(&[opaque("a"), opaque("c")]);
        let b: &[ContextElem<'_>] = bump.alloc_slice_copy(&[opaque("b"), opaque("c")]);
        let mut ctx = StringContext::new();
        ctx.extend_from_slice(a);
        ctx.extend_from_slice(b);
        assert_eq!(ctx.as_slice(), &[opaque("a"), opaque("b"), opaque("c")]);
        assert!(ctx.insert(opaque("0")));
        assert!(!ctx.insert(opaque("b")));
        assert_eq!(
            ctx.as_slice(),
            &[opaque("0"), opaque("a"), opaque("b"), opaque("c")]
        );
        assert!(ctx.contains(&opaque("c")));
        assert!(!ctx.contains(&opaque("d")));
        let finished = ctx.finish(&bump);
        assert_eq!(finished, ctx.as_slice());
        let collected: StringContext<'_> = ctx.clone().into_iter().collect();
        assert_eq!(collected.as_slice(), ctx.as_slice());
    }

    #[test]
    fn matches_btreeset_semantics() {
        // Random insert/extend sequences agree with a BTreeSet model.
        use std::collections::BTreeSet;
        let bump = crate::runtime::Bump::new();
        let names = ["a", "b", "c", "d", "e", "f"];
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..200 {
            let mut ctx = StringContext::new();
            let mut model = BTreeSet::new();
            for _ in 0..8 {
                if next() % 2 == 0 {
                    let e = opaque(names[next() % names.len()]);
                    ctx.insert(e);
                    model.insert(e);
                } else {
                    let mut sl: Vec<ContextElem<'_>> = (0..next() % 4)
                        .map(|_| opaque(names[next() % names.len()]))
                        .collect();
                    sl.sort();
                    sl.dedup();
                    let sl: &[ContextElem<'_>] = bump.alloc_slice_copy(&sl);
                    ctx.extend_from_slice(sl);
                    model.extend(sl.iter().copied());
                }
                let expected: Vec<ContextElem<'_>> = model.iter().copied().collect();
                assert_eq!(ctx.as_slice(), &expected[..]);
            }
        }
    }
}
