//! Run-time environments.
//!
//! An [`Env`] is Nix's `Env`: a frame of variable slots plus the frame it
//! is nested in. Slots are cells because a `rec` set's or a `let`'s slots
//! are filled after the frame exists (their thunks refer to it).
//!
//! A frame is two arena allocations: a [`Frame`] header (`up` and the
//! slot slice, three words) and the slots themselves (one word each).
//! Nix's frame is `up` plus the slots in one block and does not record
//! its length; here the slice carries it, so every slot access is
//! bounds-checked in every build. The length is what keeps a mismatched
//! `(environment, node)` pair (a corrupt compiled-input image, a compiler
//! bug, a thunk built by hand through the public [`crate::Value`]) a panic
//! rather than an out-of-bounds read.
//!
//! [`Env`] is a copyable handle: a reference to the header, one word wide.

use crate::runtime::cell::Cell;
use std::fmt;
use std::mem::size_of;

use crate::runtime::Bump;

use crate::value::ValueRef;

type Slot<'a> = Cell<ValueRef<'a>>;

/// A frame's header. Never written after construction; the slots are only
/// ever written through their `Cell`s.
struct Frame<'a> {
    up: Option<Env<'a>>,
    slots: &'a [Slot<'a>],
}

// A handle is one pointer, so `Value::Thunk { env, node }` stays 16 bytes.
const _: () = assert!(size_of::<Env<'static>>() == size_of::<usize>());
const _: () = assert!(size_of::<Option<Env<'static>>>() == size_of::<usize>());

/// A run-time environment frame. (`Env` in Nix; see the module docs.)
///
/// [`Env::new`] is the only constructor. Two handles name the same frame
/// when they refer to the same header ([`Env::ptr_eq`]).
#[derive(Clone, Copy)]
pub struct Env<'a>(&'a Frame<'a>);

impl<'a> Env<'a> {
    /// Allocates a frame of `len` slots in `bump`, nested in `up`, with
    /// slot `i` initialised to `init(i)`.
    #[inline]
    pub fn new(
        bump: &'a Bump,
        up: Option<Env<'a>>,
        len: usize,
        mut init: impl FnMut(usize) -> ValueRef<'a>,
    ) -> Env<'a> {
        // Slots are addressed by a `u32` displacement.
        u32::try_from(len).expect("environment frame too large");
        let slots: &'a [Slot<'a>] = bump.alloc_slice_fill_with(len, |i| Cell::new(init(i)));
        Env(bump.alloc(Frame { up, slots }))
    }

    /// The frame's slots.
    #[inline]
    pub fn slots(self) -> &'a [Slot<'a>] {
        self.0.slots
    }

    /// The enclosing frame.
    #[inline]
    pub fn up(self) -> Option<Env<'a>> {
        self.0.up
    }

    /// Number of slots.
    #[inline]
    pub fn len(self) -> usize {
        self.0.slots.len()
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Walks `level` frames up.
    #[inline]
    pub fn up_by(self, level: u32) -> Env<'a> {
        let mut env = self;
        for _ in 0..level {
            env = env
                .up()
                .expect("environment chain shorter than resolved level");
        }
        env
    }

    /// The cell in slot `displ`. Panics if the frame has no such slot.
    #[inline]
    pub fn slot(self, displ: u32) -> ValueRef<'a> {
        self.slots()[displ as usize].get()
    }

    /// Fills slot `displ`. Panics if the frame has no such slot.
    #[inline]
    pub fn set_slot(self, displ: usize, cell: ValueRef<'a>) {
        self.slots()[displ].set(cell)
    }

    /// Whether two handles name the same frame.
    pub fn ptr_eq(self, other: Env<'a>) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl fmt::Debug for Env<'_> {
    /// The frame's address and size only: slots hold thunks that refer
    /// back to their own frame, so printing them could recurse forever.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("at", &(self.0 as *const Frame<'_>))
            .field("len", &self.len())
            .field("has_up", &self.up().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{Value, ValueCell};

    fn cell<'a>(bump: &'a Bump, i: i64) -> ValueRef<'a> {
        bump.alloc(ValueCell::new(Value::Int(i)))
    }

    fn int(c: ValueRef<'_>) -> i64 {
        match c.get() {
            Value::Int(i) => i,
            v => panic!("expected an int, got {v:?}"),
        }
    }

    #[test]
    fn construction_initialises_every_slot() {
        let bump = Bump::new();
        let cells: Vec<ValueRef<'_>> = (0..5).map(|i| cell(&bump, i * 10)).collect();
        let env = Env::new(&bump, None, cells.len(), |i| cells[i]);
        assert_eq!(env.len(), 5);
        assert!(!env.is_empty());
        assert!(env.up().is_none());
        for (i, &c) in cells.iter().enumerate() {
            assert!(std::ptr::eq(env.slot(i as u32), c));
            assert_eq!(int(env.slot(i as u32)), i as i64 * 10);
        }
        assert_eq!(env.slots().len(), 5);
    }

    #[test]
    fn slots_are_filled_after_construction() {
        // As a `rec` set or `let` does: every slot starts as a placeholder
        // and is overwritten once the frame exists.
        let bump = Bump::new();
        let placeholder = cell(&bump, -1);
        let env = Env::new(&bump, None, 3, |_| placeholder);
        assert!(env
            .slots()
            .iter()
            .all(|s| std::ptr::eq(s.get(), placeholder)));
        let filled: Vec<ValueRef<'_>> = (0..3).map(|i| cell(&bump, i)).collect();
        for (i, &c) in filled.iter().enumerate().rev() {
            env.set_slot(i, c);
        }
        for (i, &c) in filled.iter().enumerate() {
            assert!(std::ptr::eq(env.slot(i as u32), c));
        }
        // A copy of the handle sees the same frame.
        let copy = env;
        copy.set_slot(1, placeholder);
        assert!(std::ptr::eq(env.slot(1), placeholder));
        assert!(env.ptr_eq(copy));
    }

    #[test]
    fn up_chain_walk() {
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let b = cell(&bump, 2);
        let c = cell(&bump, 3);
        let outer = Env::new(&bump, None, 1, |_| a);
        let middle = Env::new(&bump, Some(outer), 2, |_| b);
        let inner = Env::new(&bump, Some(middle), 1, |_| c);
        assert!(inner.up_by(0).ptr_eq(inner));
        assert!(inner.up_by(1).ptr_eq(middle));
        assert!(inner.up_by(2).ptr_eq(outer));
        assert!(inner.up().unwrap().up().unwrap().ptr_eq(outer));
        assert!(!middle.ptr_eq(outer));
        assert_eq!(int(inner.up_by(2).slot(0)), 1);
        assert_eq!(int(inner.up_by(1).slot(1)), 2);
        assert_eq!(int(inner.slot(0)), 3);
        assert!(outer.up().is_none());
    }

    #[test]
    #[should_panic(expected = "environment chain shorter than resolved level")]
    fn walking_past_the_outermost_frame_panics() {
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let outer = Env::new(&bump, None, 1, |_| a);
        let inner = Env::new(&bump, Some(outer), 0, |_| a);
        inner.up_by(2);
    }

    #[test]
    fn zero_slot_frames() {
        let bump = Bump::new();
        let a = cell(&bump, 7);
        let outer = Env::new(&bump, None, 1, |_| a);
        let empty = Env::new(&bump, Some(outer), 0, |_| unreachable!("no slots to fill"));
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
        assert!(empty.slots().is_empty());
        assert_eq!(int(empty.up_by(1).slot(0)), 7);
        // Frames allocated next to an empty one are unaffected by it.
        let after = Env::new(&bump, Some(empty), 2, |_| a);
        assert_eq!(after.len(), 2);
        assert!(after.up().unwrap().ptr_eq(empty));
        assert_eq!(empty.len(), 0);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn reading_a_slot_of_an_empty_frame_panics() {
        let bump = Bump::new();
        let empty = Env::new(&bump, None, 0, |_| unreachable!());
        empty.slot(0);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn reading_past_the_last_slot_panics() {
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let env = Env::new(&bump, None, 2, |_| a);
        env.slot(2);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn writing_past_the_last_slot_panics() {
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let env = Env::new(&bump, None, 2, |_| a);
        env.set_slot(2, a);
    }

    #[test]
    fn frames_have_distinct_identities() {
        // Also frames without slots: the header is its own allocation.
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let first = Env::new(&bump, None, 0, |_| a);
        let second = Env::new(&bump, None, 0, |_| a);
        let third = Env::new(&bump, Some(first), 3, |_| a);
        assert!(first.ptr_eq(first));
        assert!(!first.ptr_eq(second));
        assert!(!second.ptr_eq(third));
        assert!(third.up().unwrap().ptr_eq(first));
    }

    #[test]
    fn slots_are_initialised_in_index_order() {
        let bump = Bump::new();
        let a = cell(&bump, 1);
        let mut calls = Vec::new();
        let env = Env::new(&bump, None, 4, |i| {
            calls.push(i);
            a
        });
        assert_eq!(calls, vec![0, 1, 2, 3]);
        assert_eq!(env.len(), 4);
    }
}
