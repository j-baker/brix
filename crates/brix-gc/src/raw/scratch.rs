//! Reusable storage, never reusable metadata. Every logical vector is emptied
//! before collection rebuilds it; no previous address, mark or claim is read.
use super::*;

#[derive(Default)]
pub(super) struct Scratch {
    pub bits: Vec<usize>,
    pub small: Vec<Option<Pointer>>,
    pub variables: Vec<VariableObject>,
    pub ranges: Vec<ChunkRange>,
    pub slots: Vec<usize>,
    pub blocks: Vec<VariableBlock>,
    #[cfg(not(target_family = "wasm"))]
    pub claimed: Vec<std::sync::atomic::AtomicU8>,
}

impl Scratch {
    // A startup nursery can be much larger than the steady-state nursery.
    // Bound retained scratch globally, and discard a buffer when its latest
    // demand is less than a quarter of its capacity (except tiny buffers).
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    const SMALL_BYTES: usize = 64 * 1024;

    pub fn retain(&mut self) {
        let mut remaining = Self::MAX_BYTES;
        // Variable records are the largest fully overwritten buffer. Retain
        // them first, then the parallel claim array and object-start bitmap.
        retain_buffer(&mut self.variables, &mut remaining);
        #[cfg(not(target_family = "wasm"))]
        retain_buffer(&mut self.claimed, &mut remaining);
        retain_buffer(&mut self.blocks, &mut remaining);
        retain_buffer(&mut self.bits, &mut remaining);
        retain_buffer(&mut self.small, &mut remaining);
        retain_buffer(&mut self.slots, &mut remaining);
        retain_buffer(&mut self.ranges, &mut remaining);
    }

    #[cfg(test)]
    fn capacity_bytes(&self) -> usize {
        let total = self.variables.capacity() * size_of::<VariableObject>()
            + self.blocks.capacity() * size_of::<VariableBlock>()
            + self.bits.capacity() * size_of::<usize>()
            + self.small.capacity() * size_of::<Option<Pointer>>()
            + self.slots.capacity() * size_of::<usize>()
            + self.ranges.capacity() * size_of::<ChunkRange>();
        #[cfg(not(target_family = "wasm"))]
        let total = total + self.claimed.capacity();
        total
    }
}

fn retain_buffer<T>(buffer: &mut Vec<T>, remaining: &mut usize) {
    let bytes = buffer.capacity().saturating_mul(size_of::<T>());
    let demand = buffer.len().saturating_mul(size_of::<T>());
    if bytes > *remaining || bytes > Scratch::SMALL_BYTES && bytes > demand.saturating_mul(4) {
        *buffer = Vec::new();
    } else {
        *remaining -= bytes;
        buffer.clear();
    }
}

/// Detach capacity from the heap before preflight. On any error, collection's
/// local vectors are dropped; only successful collections return clean storage.
/// This does not initialize elements. Callers must fill every logical entry.
pub(super) fn take<T>(buffer: &mut Vec<T>, capacity: usize) -> Result<Vec<T>> {
    let mut result = std::mem::take(buffer);
    result.clear();
    if result.capacity() < capacity {
        #[cfg(test)]
        reservation_checkpoint()?;
        result
            .try_reserve_exact(capacity)
            .map_err(|_| Error::OutOfMemory)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_capacity_skips_reservation_and_discards_logical_contents() {
        let mut buffer = vec![usize::MAX; 128];
        let original = buffer.as_ptr();
        // Reuse must not even reach an allocator reservation checkpoint.
        FAIL_RESERVATION_AFTER.set(Some(0));
        let result = take(&mut buffer, 64);
        FAIL_RESERVATION_AFTER.set(None);
        let mut reused = result.unwrap();
        assert_eq!(reused.as_ptr(), original);
        assert!(reused.is_empty());
        reused.resize(64, 0);
        assert!(reused.iter().all(|&word| word == 0));
        assert_eq!(buffer.capacity(), 0);
    }

    #[test]
    fn retention_is_bounded_and_releases_startup_high_water() {
        // Use slot words for a large allocation without populating payloads.
        let mut scratch = Scratch {
            slots: reserved(Scratch::MAX_BYTES / size_of::<usize>() + 1).unwrap(),
            ..Scratch::default()
        };
        scratch.slots.resize(scratch.slots.capacity(), 0);
        scratch.retain();
        assert_eq!(scratch.slots.capacity(), 0);
        assert!(scratch.capacity_bytes() <= Scratch::MAX_BYTES);

        scratch.slots = vec![0; Scratch::SMALL_BYTES / size_of::<usize>() * 8];
        scratch.slots.truncate(1);
        scratch.bits = vec![0; 128];
        scratch.retain();
        assert_eq!(scratch.slots.capacity(), 0);
        assert!(scratch.bits.capacity() >= 128);
        assert!(scratch.bits.is_empty());

        let mut first = vec![0usize; 8];
        let mut second = vec![0usize; 8];
        let mut budget = first.capacity() * size_of::<usize>();
        retain_buffer(&mut first, &mut budget);
        retain_buffer(&mut second, &mut budget);
        assert_eq!(budget, 0);
        assert_eq!(second.capacity(), 0, "all buffers share one budget");
    }

    #[test]
    fn repeated_mixed_collections_rebuild_indices_marks_and_small_forwarding() {
        unsafe fn trace(_: Pointer, _: usize, _: &mut Tracer<'_>) {}
        static DESCRIPTORS: [Descriptor; 3] = [
            Descriptor {
                name: "word",
                fixed_size: Some(8),
                alignment: 8,
                trace,
                drop: None,
            },
            Descriptor {
                name: "variable",
                fixed_size: None,
                alignment: 8,
                trace,
                drop: None,
            },
            Descriptor {
                name: "tiny",
                fixed_size: Some(1),
                alignment: 1,
                trace,
                drop: None,
            },
        ];
        for workers in [1, 4] {
            let mut heap = RawHeap::new(
                Config {
                    workers,
                    chunk_bytes: 4096,
                    threshold_bytes: usize::MAX,
                    ..Config::default()
                },
                &DESCRIPTORS,
            )
            .unwrap();
            // SAFETY: these callbacks touch no shared state.
            unsafe { heap.enable_parallel_tracing() };
            let mut first_variables = std::ptr::null();
            for (cycle, count) in [64, 16, 96, 4, 64].into_iter().enumerate() {
                let mut roots = Vec::new();
                let mut weak = None;
                for i in 0..count {
                    for kind in 0..3 {
                        let bytes = match kind {
                            0 => 8,
                            1 => 2 + i % 79,
                            _ => 1,
                        };
                        let value = (i + cycle + kind) as u8;
                        // SAFETY: initialize the whole payload before any collection.
                        unsafe {
                            let pointer = heap.allocate_slow(kind, bytes).unwrap();
                            pointer.as_ptr().write_bytes(value, bytes);
                            if i % 2 == 0 {
                                // Exercise interiors in variable objects and exact
                                // starts in fixed/tiny ones on every reused cycle.
                                let offset = if kind == 1 { bytes - 1 } else { 0 };
                                roots.push((
                                    Pointer::new_unchecked(pointer.as_ptr().add(offset)),
                                    value,
                                ));
                            } else if kind == 1 {
                                weak = Some(pointer);
                            }
                        }
                    }
                }
                if cycle == 2 {
                    // A larger nursery must grow some previously reused buffer.
                    // Failing that preflight must not run callbacks or mutate roots.
                    let before = roots.clone();
                    let collections = heap.stats().collections;
                    FAIL_RESERVATION_AFTER.set(Some(0));
                    let result = unsafe {
                        heap.collect_minor(|_| panic!("failed preflight traced roots"), |_| {})
                    };
                    FAIL_RESERVATION_AFTER.set(None);
                    assert_eq!(result, Err(Error::OutOfMemory));
                    assert_eq!(heap.stats().collections, collections);
                    assert_eq!(roots, before);
                }
                // SAFETY: these are every strong edge; weak updates never retain.
                unsafe {
                    let trace_roots = |tracer: &mut Tracer<'_>| {
                        for (root, _) in &mut roots {
                            tracer.visit(root);
                        }
                    };
                    let update = |relocation: &Relocation<'_>| {
                        weak = weak.and_then(|pointer| relocation.lookup(pointer));
                    };
                    if cycle % 2 == 0 {
                        heap.collect_minor(trace_roots, update).unwrap();
                    } else {
                        heap.collect(trace_roots, update).unwrap();
                    }
                }
                assert!(
                    weak.is_none(),
                    "old forwarding state must not retain dead objects"
                );
                for (root, value) in roots {
                    assert_eq!(unsafe { root.as_ptr().read() }, value);
                }
                assert!(heap.scratch.capacity_bytes() <= Scratch::MAX_BYTES);
                assert!(heap.scratch.variables.is_empty());
                if cycle == 0 {
                    first_variables = heap.scratch.variables.as_ptr();
                } else if cycle == 1 {
                    assert_eq!(
                        heap.scratch.variables.as_ptr(),
                        first_variables,
                        "the second collection must reuse variable-record storage"
                    );
                }
            }
        }
    }
}
