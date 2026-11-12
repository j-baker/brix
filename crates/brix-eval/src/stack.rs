//! The stack check on the evaluator's recursion.
//!
//! Evaluation recurses as deep as the Nix program does, so every force of
//! a thunk and every function call first makes sure at least
//! [`STACK_RED_ZONE`] bytes of stack remain, and moves to a fresh
//! [`STACK_SEGMENT`]-byte segment (`stacker::grow`) when they do not. The
//! depth Nix programs see is bounded by `max-call-depth`
//! ([`Evaluator::enter`]), never by the stack.
//!
//! `stacker::maybe_grow` makes that decision by calling an out-of-line
//! assembly function for the stack pointer and reading the segment's
//! limit from a lazily initialised thread local, about three million
//! times for `firefox.drvPath`. [`Evaluator::with_stack`] makes the same
//! decision inline: the evaluator remembers a [`StackWindow`], an address
//! range on the current segment in which at least the red zone is known to
//! remain, and compares the address of a local against it. Anything
//! outside the window (the first check, a frame above the window's top, a
//! segment the window does not describe) goes to the out-of-line
//! [`Evaluator::with_stack_slow`], which asks `stacker`, grows exactly when
//! `stacker::maybe_grow` would, and sets the window for the segment it
//! continues on. A window lies inside one segment, so an address in it
//! means the code runs on that segment with more than the red zone left:
//! the inline check never skips a growth `stacker` would have made.

use crate::evaluator::Evaluator;

/// How much stack must remain before evaluation moves to a fresh segment.
pub(crate) const STACK_RED_ZONE: usize = 128 * 1024;
/// The size of each segment `stacker` allocates on demand.
pub(crate) const STACK_SEGMENT: usize = 8 * 1024 * 1024;

/// Addresses on one stack segment at which more than [`STACK_RED_ZONE`]
/// bytes remain: `floor <= address < floor + span`. The empty window
/// (`span == 0`) contains nothing, so the first check goes to `stacker`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StackWindow {
    floor: usize,
    span: usize,
}

crate::runtime::thread_cell!(StackWindowCell, StackWindow, StackWindow { floor: 0, span: 0 });

impl StackWindow {
    /// The window from `top`, an address on the running segment, down to
    /// the red zone, given that `remaining` bytes of the segment lie below
    /// `top`; empty if they are not more than the red zone.
    fn below(top: usize, remaining: usize) -> StackWindow {
        StackWindow {
            floor: top - remaining + STACK_RED_ZONE,
            span: remaining.saturating_sub(STACK_RED_ZONE),
        }
    }

    /// Whether `address` lies in the window: one subtraction and one
    /// unsigned comparison, since an address below `floor` wraps around
    /// to a huge offset.
    #[inline(always)]
    fn contains(self, address: usize) -> bool {
        address.wrapping_sub(self.floor) < self.span
    }
}

/// The address of a local in the caller's frame: the stack pointer, give
/// or take the frame. Inlined, so the local is the caller's own.
#[inline(always)]
fn stack_address() -> usize {
    let marker = 0u8;
    std::ptr::addr_of!(marker) as usize
}

/// Restores the window of the segment a grown segment returns to, on
/// return and on unwinding alike.
struct RestoreWindow<'e, 'a> {
    ev: &'e Evaluator<'a>,
    outer: StackWindow,
}

impl Drop for RestoreWindow<'_, '_> {
    fn drop(&mut self) {
        self.ev.stack_window.set(self.outer);
    }
}

impl<'a> Evaluator<'a> {
    /// Runs `f` with at least [`STACK_RED_ZONE`] bytes of stack, on a fresh
    /// segment if this one is nearly used up (`stacker::maybe_grow`'s
    /// decision, made inline when the window allows; see the module docs).
    #[inline(always)]
    pub(crate) fn with_stack<R>(&self, f: impl FnOnce() -> R) -> R {
        if self.stack_window.get().contains(stack_address()) {
            f()
        } else {
            self.with_stack_slow(f)
        }
    }

    /// [`Evaluator::with_stack`] outside the window: grows exactly when
    /// `stacker::maybe_grow` would, and sets the window for the segment
    /// `f` runs on.
    #[cold]
    #[inline(never)]
    fn with_stack_slow<R>(&self, f: impl FnOnce() -> R) -> R {
        match stacker::remaining_stack() {
            Some(remaining) if remaining >= STACK_RED_ZONE => {
                // Room on a segment the window does not describe yet, or
                // above its top: describe it from this frame down.
                self.stack_window
                    .set(StackWindow::below(stack_address(), remaining));
                f()
            }
            _ => stacker::grow(STACK_SEGMENT, || {
                let remaining = stacker::remaining_stack()
                    .expect("stacker knows the limit of a segment it allocated");
                let _restore = RestoreWindow {
                    ev: self,
                    outer: self
                        .stack_window
                        .replace(StackWindow::below(stack_address(), remaining)),
                };
                f()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_bounds() {
        let w = StackWindow::below(0x10_0000, 0x8_0000);
        assert_eq!(w.floor, 0x10_0000 - 0x8_0000 + STACK_RED_ZONE);
        assert!(w.contains(w.floor));
        assert!(!w.contains(w.floor - 1));
        assert!(w.contains(0x10_0000 - 1));
        assert!(!w.contains(0x10_0000));
        assert!(!w.contains(usize::MAX));
        assert!(!w.contains(0));
    }

    #[test]
    fn no_window_without_room_beyond_the_red_zone() {
        let w = StackWindow::below(0x10_0000, STACK_RED_ZONE);
        assert_eq!(w.span, 0);
        assert!(!w.contains(0x10_0000 - 1));
        let w = StackWindow::below(0x10_0000, STACK_RED_ZONE / 2);
        assert_eq!(w.span, 0);
        assert!(!StackWindow::default().contains(0));
    }
}
