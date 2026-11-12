use super::*;

#[test]
fn cached_out_path_tracks_cells_and_falls_back_after_collection() {
    for workers in [1, 4] {
        with_owned_heap(
            Config {
                workers,
                ..Config::default()
            },
            |mut heap| {
                heap.with_roots(|heap, roots| {
                    let set = heap.with_roots(|heap, temp| {
                        let cell = heap.cell(temp, temp.int(17)).unwrap();
                        let set = heap
                            .alloc_flat_bindings(
                                temp,
                                SrcPos::NONE,
                                &[RootedAttr {
                                    name: crate::symbol::sym::OUT_PATH,
                                    pos: SrcPos::NONE,
                                    value: cell,
                                }],
                            )
                            .unwrap();
                        roots.copy(set)
                    });
                    for major in [false, false, true] {
                        heap.cache_derivation(set, true);
                        let cell = heap.derivation_out_path(set).unwrap().root(roots);
                        assert!(matches!(heap.read(cell), ValueView::Int(17)));
                        // Cache the cell, not a snapshot of its value: thunk forcing
                        // changes the cell in place without changing the bindings.
                        heap.store(cell, roots.int(23));
                        assert!(matches!(
                            heap.read_view(heap.derivation_out_path(set).unwrap()),
                            ValueView::Int(23)
                        ));
                        if major {
                            heap.collect().unwrap();
                        } else {
                            heap.collect_minor().unwrap();
                        }
                        assert_eq!(heap.cached_derivation(set), None);
                        let reloaded = heap.derivation_out_path(set).unwrap().root(roots);
                        assert!(heap.same_cell(cell, reloaded));
                        assert!(matches!(heap.read(reloaded), ValueView::Int(23)));
                        heap.store(reloaded, roots.int(17));
                    }
                    // A cached negative lookup must remain distinguishable from
                    // a cache miss (which also performs a fresh attribute lookup).
                    let missing = heap.alloc_flat_bindings(roots, SrcPos(17), &[]).unwrap();
                    heap.cache_derivation(missing, true);
                    assert!(heap.derivation_out_path(missing).is_none());
                    heap.collect().unwrap();
                    assert!(heap.derivation_out_path(missing).is_none());
                });
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), 0, "cached cells must not retain objects");
            },
        )
        .unwrap();
    }
}

#[test]
fn derivation_cache_does_not_survive_collection_or_keep_sets_alive() {
    for workers in [1, 4] {
        with_owned_heap(
            Config {
                workers,
                ..Config::default()
            },
            |mut heap| {
                heap.with_roots(|heap, roots| {
                    let set = heap.alloc_flat_bindings(roots, SrcPos(17), &[]).unwrap();
                    assert_eq!(heap.cached_derivation(set), None);
                    heap.cache_derivation(set, false);
                    assert_eq!(heap.cached_derivation(set), Some(false));
                    heap.collect_minor().unwrap();
                    assert_eq!(heap.cached_derivation(set), None);
                    heap.cache_derivation(set, true);
                    assert_eq!(heap.cached_derivation(set), Some(true));
                    // Even a stationary old object must not reuse a prior epoch.
                    let identity = heap.bindings_identity(set).address;
                    heap.collect_minor().unwrap();
                    assert_eq!(heap.bindings_identity(set).address, identity);
                    assert_eq!(heap.cached_derivation(set), None);
                    heap.cache_derivation(set, false);
                    heap.collect().unwrap();
                    assert_eq!(heap.cached_derivation(set), None);
                    heap.cache_derivation(set, false);
                });
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), 0, "cache entries must not retain sets");
            },
        )
        .unwrap();
    }
}

use crate::error::{ErrorKind, EvalError};

fn stress() -> Config {
    Config { workers: 0,
        chunk_bytes: 256,
        steady_threshold_bytes: None,
        trace_collections: false,
        threshold_bytes: 512,
        stress: true,
    }
}

#[test]
fn cell_snapshot_survives_blackholing_and_moving_environment_cycle() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let cell = heap.cell(roots, roots.null()).unwrap();
            let env = heap.env(roots, None, &[cell]).unwrap();
            let thunk = heap.thunk(roots, env, GlobalNode(17));
            heap.store(cell, thunk);
            let saved = heap.value(roots, cell);
            heap.store(cell, roots.blackhole());
            for _ in 0..12 {
                heap.collect().unwrap();
                assert!(matches!(heap.read(cell), ValueView::Blackhole));
                let ValueView::Thunk { env, node } = heap.view(saved) else {
                    panic!("saved thunk")
                };
                assert_eq!(node, GlobalNode(17));
                let back = env.get(0).unwrap().root(roots);
                assert!(heap.same_cell(back, cell));
            }
            heap.store(cell, roots.int(42));
            heap.collect().unwrap();
            let ValueView::Thunk { env, .. } = heap.view(saved) else {
                panic!("saved thunk")
            };
            let back = env.get(0).unwrap().root(roots);
            assert!(matches!(heap.read(back), ValueView::Int(42)));
        })
    })
    .unwrap();
}

#[test]
fn value_from_borrowed_environment_outlives_its_original_cell() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, outer| {
            let value = heap.with_roots(|heap, inner| {
                let text = heap.string(inner, b"kept by the value snapshot").unwrap();
                let cell = heap.cell(inner, text).unwrap();
                let env = heap.env(inner, None, &[cell]).unwrap();
                heap.value_from_view(outer, heap.env_view(env).get(0).unwrap())
            });
            for _ in 0..8 {
                heap.collect().unwrap();
                let ValueView::String(text) = heap.view(value) else {
                    panic!("string snapshot")
                };
                assert_eq!(text.bytes(), b"kept by the value snapshot");
            }
        });
        heap.collect().unwrap();
        assert_eq!(heap.used_bytes(), 0);
    })
    .unwrap();
}

#[test]
fn strings_contexts_and_interior_slices_survive_every_allocation() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, outer| {
            let part = heap.with_roots(|heap, roots| {
                let drv = heap
                    .bytes(roots, b"00000000000000000000000000000000-test.drv")
                    .unwrap();
                let output = heap.bytes(roots, b"out\xff").unwrap();
                let context = heap
                    .contexts(
                        roots,
                        &[
                            ContextInput::Opaque(drv),
                            ContextInput::Built { drv, output },
                            ContextInput::LazyPath(3),
                        ],
                    )
                    .unwrap();
                let source = heap
                    .string_with_context(roots, b"prefix/hello/suffix", context)
                    .unwrap();
                let part = heap.substring(roots, source, 7..12).unwrap();
                outer.copy(part)
            });
            for _ in 0..12 {
                heap.collect().unwrap();
                let ValueView::String(s) = heap.view(part) else {
                    panic!("string")
                };
                assert_eq!(s.bytes(), b"hello");
                let contexts: Vec<_> = s.contexts().collect();
                assert!(matches!(
                    contexts[0],
                    ContextView::Opaque(b"00000000000000000000000000000000-test.drv")
                ));
                assert!(matches!(
                    contexts[1],
                    ContextView::Built {
                        output: b"out\xff",
                        ..
                    }
                ));
                assert!(matches!(contexts[2], ContextView::LazyPath(3)));
            }
            let empty = heap.substring(outer, part, 5..5).unwrap();
            heap.collect().unwrap();
            let ValueView::String(s) = heap.view(empty) else {
                panic!("empty string")
            };
            assert!(s.bytes().is_empty());
            assert!(matches!(
                heap.substring(outer, part, 6..6),
                Err(Error::InvalidRange)
            ));
        })
    })
    .unwrap();
}

#[test]
fn lists_paths_functions_and_mutable_environment_slots_are_rooted() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let one = heap.cell(roots, roots.int(1)).unwrap();
            let two = heap.cell(roots, roots.int(2)).unwrap();
            let list = heap.list(roots, &[one, two]).unwrap();
            let path = heap.path(roots, AccessorId(7), b"/caf\xff").unwrap();
            let app = heap.app(roots, one, two, false).unwrap();
            let partial = heap.app(roots, one, two, true).unwrap();
            let env = heap.env(roots, None, &[one]).unwrap();
            let lambda = heap.lambda(roots, env, GlobalNode(12));
            let primop = heap.primop(roots, 99);
            heap.env_set(env, 0, two);
            heap.collect().unwrap();
            let ValueView::List(l) = heap.view(list) else {
                panic!("list")
            };
            assert_eq!(l.len(), 2);
            let first = l.get(0).unwrap().root(roots);
            assert!(heap.same_cell(first, one));
            let ValueView::Path(p) = heap.view(path) else {
                panic!("path")
            };
            assert_eq!(p.bytes(), b"/caf\xff");
            assert_eq!(p.accessor(), AccessorId(7));
            let ValueView::App(a) = heap.view(app) else {
                panic!("application")
            };
            assert!(heap.same_cell(a.fun().root(roots), one));
            assert!(heap.same_cell(a.arg().root(roots), two));
            assert!(matches!(heap.view(partial), ValueView::PrimOpApp(_)));
            let ValueView::Lambda { env, node } = heap.view(lambda) else {
                panic!("lambda")
            };
            assert_eq!(node, GlobalNode(12));
            assert!(heap.same_cell(env.get(0).unwrap().root(roots), two));
            assert!(matches!(heap.view(primop), ValueView::PrimOp(99)));
        })
    })
    .unwrap();
}

#[test]
fn fused_application_cells_reload_roots_between_allocations() {
    for stress_enabled in [false, true] {
        with_heap(
            Config {
                stress: stress_enabled,
                ..stress()
            },
            |heap| {
                heap.with_roots(|heap, roots| {
                    let leaf = heap.cell(roots, roots.int(42)).unwrap();
                    let head = roots.slot(leaf);
                    for _ in 0..32 {
                        heap.with_roots(|heap, temporary| {
                            let previous = head.root();
                            let app = heap.app_cell(temporary, previous, previous).unwrap();
                            head.set(app);
                        });
                    }
                    // Only the head and leaf are independently rooted now.
                    // Every intermediate cell and app must survive via edges.
                    heap.collect().unwrap();
                    let mut cursor = head.root();
                    for _ in 0..32 {
                        let ValueView::App(app) = heap.read(cursor) else {
                            panic!("application")
                        };
                        let fun = app.fun().root(roots);
                        let arg = app.arg().root(roots);
                        assert!(heap.same_cell(fun, arg));
                        cursor = fun;
                    }
                    assert!(heap.same_cell(cursor, leaf));
                    assert!(matches!(heap.read(cursor), ValueView::Int(42)));
                });
            },
        )
        .unwrap();
    }
}

#[test]
fn persistent_leases_keep_only_live_values_across_scopes() {
    with_owned_heap(stress(), |mut heap| {
        let provider = heap.root_registry();
        let lease = provider.with_roots(|roots| {
            let text = heap.string(roots, b"retained by lease").unwrap();
            heap.persist(text)
        });
        heap.collect().unwrap();
        let ValueView::String(s) = heap.view(lease.root()) else {
            panic!("leased string")
        };
        assert_eq!(s.bytes(), b"retained by lease");
        let other = lease.clone();
        drop(lease);
        heap.collect().unwrap();
        let ValueView::String(s) = heap.view(other.root()) else {
            panic!("cloned lease")
        };
        assert_eq!(s.bytes(), b"retained by lease");
        drop(other);
        heap.collect().unwrap();
        assert_eq!(heap.used_bytes(), 0);
    })
    .unwrap();
}

#[test]
fn bounded_churn_collects_inside_a_live_root_scope() {
    with_heap(
        Config { workers: 0,
            stress: false,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 512,
        },
        |heap| {
            heap.with_roots(|heap, outer| {
                let last = outer.slot(outer.null());
                for _ in 0..1000 {
                    heap.with_roots(|heap, roots| {
                        let value = heap.string(roots, &[37; 1000]).unwrap();
                        last.set(value);
                    });
                }
                heap.collect().unwrap();
                let ValueView::String(s) = heap.view(last.root()) else {
                    panic!("last value")
                };
                assert_eq!(s.bytes(), &[37; 1000]);
                assert!(heap.used_bytes() < 1200, "{}", heap.used_bytes());
                assert!(heap.stats().collections > 100);
            })
        },
    )
    .unwrap();
}

#[test]
fn root_vectors_and_weak_seen_sets_follow_movement_and_release_dead_cells() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let vector = roots.vector::<CellKind>();
            for i in 0..80 {
                heap.with_roots(|heap, inner| {
                    let cell = heap.cell(inner, inner.int(i)).unwrap();
                    assert!(heap.mark_seen(cell));
                    vector.push(cell);
                });
            }
            heap.collect().unwrap();
            assert_eq!(vector.len(), 80);
            assert_eq!(heap.seen_count(), 80);
            let initial = heap.cell(roots, roots.null()).unwrap();
            let slot = roots.slot(initial);
            for i in 0..80 {
                assert!(vector.get_into(slot, i));
                assert!(matches!(heap.read(slot.root()), ValueView::Int(n) if n == i as i64));
                assert!(!heap.mark_seen(slot.root()));
            }
            vector.clear();
            heap.collect().unwrap();
            assert_eq!(heap.seen_count(), 1, "only the last slot remains rooted");
            assert!(heap.used_bytes() <= 32);
        })
    })
    .unwrap();
}

#[test]
fn failed_errors_release_rc_once_after_the_last_root() {
    let error = EvalError::new(ErrorKind::Eval, "failure");
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let failed = heap.failed(roots, error.clone()).unwrap();
            let cell = heap.cell(roots, failed).unwrap();
            for _ in 0..5 {
                heap.collect().unwrap();
                assert_eq!(Rc::strong_count(&error), 2);
                let ValueView::Failed(actual) = heap.read(cell) else {
                    panic!("failed value")
                };
                assert!(Rc::ptr_eq(actual, &error));
            }
        });
        heap.collect().unwrap();
        assert_eq!(Rc::strong_count(&error), 1);
    })
    .unwrap();
    assert_eq!(Rc::strong_count(&error), 1);
}

#[test]
fn binding_blocks_and_tree_nodes_survive_stress_and_share_children() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let mut attrs = Vec::new();
            for i in 0..80 {
                let cell = heap.cell(roots, roots.int(i)).unwrap();
                attrs.push(RootedAttr {
                    name: Symbol(i as u32),
                    pos: SrcPos(i as u32),
                    value: cell,
                });
            }
            let flat = heap
                .alloc_flat_bindings(roots, SrcPos(5), &attrs[..3])
                .unwrap();
            let first = heap.alloc_leaf(roots, &attrs[..40]).unwrap();
            let second = heap.alloc_leaf(roots, &attrs[40..]).unwrap();
            let node = heap.alloc_inner(roots, &[first, second]).unwrap();
            let tree = heap
                .alloc_tree_bindings(roots, SrcPos(6), 80, node)
                .unwrap();
            let value = heap.bindings_value(roots, tree);
            heap.collect().unwrap();
            assert_eq!(heap.bindings_view(flat).len(), 3);
            let ValueView::Attrs(tree_view) = heap.view(value) else {
                panic!("attrs")
            };
            assert_eq!(tree_view.pos(), SrcPos(6));
            let all: Vec<_> = tree_view.iter().map(|a| a.root(roots)).collect();
            assert_eq!(all.len(), 80);
            for (i, attr) in all.iter().enumerate() {
                assert_eq!(attr.name, Symbol(i as u32));
                assert!(matches!(heap.read(attr.value), ValueView::Int(n) if n == i as i64));
            }
            let found = heap
                .bindings_view(tree)
                .get(Symbol(51))
                .unwrap()
                .root(roots);
            assert!(matches!(heap.read(found.value), ValueView::Int(51)));
            let NodeContents::Inner { children, firsts } = heap.node_snapshot(roots, node) else {
                panic!("inner")
            };
            assert_eq!(firsts, vec![Symbol(0), Symbol(40)]);
            assert!(heap.same_node(children[0], first));
            assert!(heap.same_node(children[1], second));
        })
    })
    .unwrap();
}

#[test]
fn unwinding_a_root_scope_releases_only_its_roots() {
    let error = EvalError::new(ErrorKind::Eval, "unwind");
    with_owned_heap(stress(), |mut heap| {
        let provider = heap.root_registry();
        provider.with_roots(|outer| {
            let stable = heap.string(outer, b"outer").unwrap();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                provider.with_roots(|inner| {
                    let _failed = heap.failed(inner, error.clone()).unwrap();
                    assert_eq!(Rc::strong_count(&error), 2);
                    panic!("unwind inner frame");
                });
            }));
            assert!(result.is_err());
            heap.collect().unwrap();
            assert_eq!(Rc::strong_count(&error), 1);
            let ValueView::String(s) = heap.view(stable) else {
                panic!("outer root")
            };
            assert_eq!(s.bytes(), b"outer");
        });
        heap.collect().unwrap();
        assert_eq!(heap.used_bytes(), 0);
    })
    .unwrap();
}

#[test]
fn scoped_visited_sets_are_independent_weak_and_follow_moves() {
    with_heap(stress(), |heap| {
        let first = heap.visited();
        let second = heap.visited();
        heap.with_roots(|heap, roots| {
            let live = heap.cell(roots, roots.int(7)).unwrap();
            assert!(heap.mark_visited(&first, live));
            assert!(!heap.mark_visited(&first, live));
            assert!(heap.mark_visited(&second, live));
            heap.with_roots(|heap, temporary| {
                let dead = heap.cell(temporary, temporary.int(9)).unwrap();
                assert!(heap.mark_visited(&first, dead));
            });
            heap.collect().unwrap();
            assert_eq!(first.cells.borrow().len(), 1);
            assert!(!heap.mark_visited(&first, live));
            assert!(!heap.mark_visited(&second, live));
            for _ in 0..10 {
                heap.collect().unwrap();
                assert!(!heap.mark_visited(&first, live));
            }
        });
        heap.collect().unwrap();
        assert!(first.cells.borrow().is_empty());
        assert!(second.cells.borrow().is_empty());
        drop(first);
        drop(second);
        for _ in 0..100 {
            let _visit = heap.visited();
        }
        assert_eq!(heap.visited.len(), 1);
        heap.collect().unwrap();
        assert!(heap.visited.is_empty());
    })
    .unwrap();
}

#[test]
fn canonical_empty_values_reuse_allocations_across_root_frames_and_moves() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            heap.string(roots, b"").unwrap();
            heap.list(roots, &[]).unwrap();
            heap.alloc_flat_bindings(roots, SrcPos::NONE, &[]).unwrap();
        });
        heap.collect().unwrap();
        let baseline = heap.used_bytes();
        for _ in 0..8 {
            heap.with_roots(|heap, roots| {
                let string = heap.string(roots, b"").unwrap();
                let list = heap.list(roots, &[]).unwrap();
                let attrs = heap.alloc_flat_bindings(roots, SrcPos::NONE, &[]).unwrap();
                assert!(matches!(heap.view(string), ValueView::String(s) if s.bytes().is_empty()));
                assert!(matches!(heap.view(list), ValueView::List(l) if l.is_empty()));
                assert!(heap.bindings_view(attrs).is_empty());
                assert_eq!(heap.used_bytes(), baseline);
                heap.collect().unwrap();
                assert!(matches!(heap.view(list), ValueView::List(l) if l.is_empty()));
            });
        }
        heap.with_roots(|heap, roots| {
            // Explicit positions and string contexts must not be discarded.
            let positioned = heap.alloc_flat_bindings(roots, SrcPos(19), &[]).unwrap();
            assert_eq!(heap.bindings_view(positioned).pos(), SrcPos(19));
            let bytes = heap.bytes(roots, b"path").unwrap();
            let context = heap.contexts(roots, &[ContextInput::Path(bytes)]).unwrap();
            let string = heap.string_with_context(roots, b"", context).unwrap();
            let ValueView::String(value) = heap.view(string) else {
                panic!("string")
            };
            assert_eq!(value.contexts().count(), 1);
        });
    })
    .unwrap();
}

#[test]
fn list_concat_preserves_empty_aliases_and_children_through_both_allocations() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, outer| {
            let (joined, alias) = heap.with_roots(|heap, roots| {
                let left = heap.with_roots(|heap, build| {
                    let text = heap.string(build, b"left").unwrap();
                    let cell = heap.cell(build, text).unwrap();
                    roots.copy(heap.list(build, &[cell]).unwrap())
                });
                let right = heap.with_roots(|heap, build| {
                    let text = heap.string(build, b"right").unwrap();
                    let cell = heap.cell(build, text).unwrap();
                    roots.copy(heap.list(build, &[cell, cell]).unwrap())
                });
                let empty = heap.list(roots, &[]).unwrap();
                let before = heap.used_bytes();
                let both_empty = heap.concat_lists(roots, empty, empty).unwrap();
                let from_left = heap.concat_lists(roots, left, empty).unwrap();
                let from_right = heap.concat_lists(roots, empty, right).unwrap();
                assert_eq!(heap.used_bytes(), before);
                assert!(matches!(heap.view(both_empty), ValueView::List(v) if v.is_empty()));
                assert!(matches!(heap.view(from_left), ValueView::List(v) if v.len() == 1));
                assert!(matches!(heap.view(from_right), ValueView::List(v) if v.len() == 2));
                let joined = heap.concat_lists(roots, left, right).unwrap();
                let alias = heap.concat_lists(roots, left, left).unwrap();
                (outer.copy(joined), outer.copy(alias))
            });
            for _ in 0..3 {
                heap.collect().unwrap();
                let ValueView::List(list) = heap.view(joined) else { panic!("list") };
                assert_eq!(list.len(), 3);
                for (index, expected) in [b"left".as_slice(), b"right", b"right"].iter().enumerate() {
                    assert!(matches!(heap.read_view(list.get(index).unwrap()), ValueView::String(v) if v.bytes() == *expected));
                }
                let ValueView::List(list) = heap.view(alias) else { panic!("list") };
                assert!(heap.same_cell(list.get(0).unwrap().root(outer), list.get(1).unwrap().root(outer)));
            }
        });
    }).unwrap();
}

#[test]
fn list_concat_collects_on_the_real_heap_threshold() {
    with_heap(
        Config { workers: 0,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1024,
            stress: false,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let source = heap.with_roots(|heap, build| {
                    let cell = heap.cell(build, build.int(42)).unwrap();
                    roots.copy(heap.list(build, &[cell; 64]).unwrap())
                });
                let before = heap.stats().collections;
                let result = heap.concat_lists(roots, source, source).unwrap();
                assert!(heap.stats().collections > before);
                heap.collect().unwrap();
                let ValueView::List(list) = heap.view(result) else {
                    panic!("list")
                };
                assert_eq!(list.len(), 128);
                for index in 0..list.len() {
                    assert!(matches!(
                        heap.read_view(list.get(index).unwrap()),
                        ValueView::Int(42)
                    ));
                }
            });
        },
    )
    .unwrap();
}

fn map_fixture<'r, 'h>(
    heap: &mut Heap<'h>,
    roots: &Roots<'r, 'h>,
    len: usize,
) -> (
    BindingsRoot<'r, 'h>,
    CellRoot<'r, 'h>,
    Vec<Option<Lease<'h, CellKind>>>,
) {
    let value = heap.cell(roots, roots.int(42)).unwrap();
    let function = heap.cell(roots, roots.bool(false)).unwrap();
    let mut names = Vec::new();
    for index in 0..len {
        let lease = heap.with_roots(|heap, temporary| {
            let name = heap.cell(temporary, temporary.int(index as i64)).unwrap();
            heap.persist(name)
        });
        names.push(Some(lease));
    }
    let attrs: Vec<_> = (0..len)
        .map(|index| RootedAttr {
            name: Symbol(index as u32),
            pos: SrcPos(77),
            value,
        })
        .collect();
    let input = crate::managed_bindings::from_sorted(heap, roots, &attrs, SrcPos(99)).unwrap();
    (input, function, names)
}

fn check_lazy_mapping<'h>(
    heap: &Heap<'h>,
    roots: &Roots<'_, 'h>,
    output: BindingsRoot<'_, 'h>,
    len: usize,
) {
    let view = heap.bindings_view(output);
    assert_eq!(view.len(), len);
    assert_eq!(view.pos(), SrcPos::NONE);
    for (index, attr) in view.iter().enumerate() {
        assert_eq!(attr.name, Symbol(index as u32));
        assert_eq!(attr.pos, SrcPos::NONE);
        let ValueView::App(second) = heap.read_view(attr.value) else {
            panic!("second application")
        };
        assert!(matches!(heap.read_view(second.arg()), ValueView::Int(42)));
        let ValueView::App(first) = heap.read_view(second.fun()) else {
            panic!("first application")
        };
        assert!(matches!(
            heap.read_view(first.fun()),
            ValueView::Bool(false)
        ));
        assert!(matches!(heap.read_view(first.arg()), ValueView::Int(n) if n == index as i64));
        // Both applications are still suspended; invalid function was not forced.
        assert!(!heap.is_whnf(attr.value.root(roots)));
    }
}

#[test]
fn lazy_map_attrs_preserves_graph_after_inputs_and_name_leases_drop() {
    for len in [0, 1, 64, 65, 129, 4097] {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let output = heap.with_roots(|heap, temporary| {
                    let (input, function, names) = map_fixture(heap, temporary, len);
                    let before = heap.stats().collections;
                    let output = heap
                        .map_attrs_lazy(temporary, input, function, &names)
                        .unwrap();
                    assert!(heap.stats().collections <= before + 1);
                    roots.copy(output)
                });
                // No source/function/name lease roots remain. The mapped graph
                // alone must retain all application arguments through movement.
                for _ in 0..2 {
                    heap.collect().unwrap();
                    check_lazy_mapping(heap, roots, output, len);
                }
            });
        })
        .unwrap();
    }
}

#[test]
fn lazy_map_attrs_accounts_for_pending_applications_before_batch() {
    with_heap(
        Config { workers: 0,
            chunk_bytes: 128,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 12 * 1024,
            stress: false,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let (input, function, names) = map_fixture(heap, roots, 129);
                let before = heap.stats().collections;
                // Source fits below the threshold; output apps plus tree exceed it.
                assert!(heap.used_bytes() < 12 * 1024);
                let output = heap.map_attrs_lazy(roots, input, function, &names).unwrap();
                assert_eq!(heap.stats().collections, before + 1);
                check_lazy_mapping(heap, roots, output, 129);
            });
        },
    )
    .unwrap();
}

#[test]
fn partial_lazy_map_attrs_failures_leave_heap_valid() {
    for (len, allocations) in [(1, 2), (64, 2), (65, 10)] {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let (input, function, names) = map_fixture(heap, roots, len);
                heap.collect().unwrap();
                let baseline = heap.used_bytes();
                for failure in 0..=allocations {
                    heap.with_roots(|heap, temporary| {
                        let result = with_map_attrs_failure(failure, || {
                            heap.map_attrs_lazy(temporary, input, function, &names)
                        });
                        if failure < allocations {
                            assert!(
                                matches!(result, Err(Error::OutOfMemory)),
                                "allocation {failure}"
                            );
                        } else {
                            check_lazy_mapping(heap, temporary, result.unwrap(), len);
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(heap.used_bytes(), baseline, "allocation {failure}");
                    assert_eq!(heap.bindings_view(input).pos(), SrcPos(99));
                    assert!(matches!(heap.read(function), ValueView::Bool(false)));
                }
            });
        })
        .unwrap();
    }
}

#[test]
fn lazy_map_attrs_fixed_runs_keep_objects_independently_reclaimable() {
    for len in [64, 65, 129] {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let kept = heap.with_roots(|heap, temporary| {
                    let (input, function, names) = map_fixture(heap, temporary, len);
                    let output = heap
                        .map_attrs_lazy(temporary, input, function, &names)
                        .unwrap();
                    heap.bindings_view(output)
                        .get(Symbol(0))
                        .unwrap()
                        .value
                        .root(roots)
                });
                heap.collect().unwrap();
                // Only two apps, their two cells, function, argument and name
                // remain. Neighbouring objects from both fixed runs are dead.
                assert_eq!(
                    heap.used_bytes(),
                    2 * size_of::<RawApp>() + 5 * size_of::<RawCell>()
                );
                let ValueView::App(second) = heap.read(kept) else {
                    panic!("second app")
                };
                assert!(matches!(heap.read_view(second.arg()), ValueView::Int(42)));
                let ValueView::App(first) = heap.read_view(second.fun()) else {
                    panic!("first app")
                };
                assert!(matches!(
                    heap.read_view(first.fun()),
                    ValueView::Bool(false)
                ));
                assert!(matches!(heap.read_view(first.arg()), ValueView::Int(0)));
                heap.store(kept, roots.int(99));
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), size_of::<RawCell>());
                assert!(matches!(heap.read(kept), ValueView::Int(99)));
            });
        })
        .unwrap();
    }
}

#[test]
fn lazy_map_attrs_missing_names_leave_initialized_reclaimable_garbage() {
    for stress_gc in [false, true] {
        for len in [1, 64, 65, 129] {
            for missing in [0, len / 2, len - 1] {
                for truncated in [false, true] {
                    with_heap(
                        Config {
                            stress: stress_gc,
                            ..stress()
                        },
                        |heap| {
                            heap.with_roots(|heap, roots| {
                                let (input, function, mut names) = map_fixture(heap, roots, len);
                                if truncated {
                                    names.truncate(missing);
                                } else {
                                    names[missing] = None;
                                }
                                heap.collect().unwrap();
                                let baseline = heap.used_bytes();
                                let before = heap.stats().collections;
                                let result = heap.map_attrs_lazy(roots, input, function, &names);
                                assert!(matches!(result, Err(Error::InvalidRange)));
                                assert!(heap.stats().collections <= before + 1);
                                heap.collect().unwrap();
                                assert_eq!(heap.used_bytes(), baseline);
                                let input = heap.bindings_view(input);
                                assert_eq!(input.len(), len);
                                assert_eq!(input.pos(), SrcPos(99));
                                for attr in input.iter() {
                                    assert!(matches!(
                                        heap.read_view(attr.value),
                                        ValueView::Int(42)
                                    ));
                                }
                                assert!(matches!(heap.read(function), ValueView::Bool(false)));
                            });
                        },
                    )
                    .unwrap();
                }
            }
        }
    }
}

#[test]
fn missing_attr_names_scan_cached_and_sparse_leaf_ranges() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let (input, _, mut names) = map_fixture(heap, roots, 129);
            let cached = heap.missing_attr_names(input, &names);
            assert!(cached.is_empty());
            assert_eq!(cached.capacity(), 0);
            let missing = [0, 42, 43, 85, 86, 128];
            for index in missing {
                names[index] = None;
            }
            heap.collect().unwrap();
            assert_eq!(
                heap.missing_attr_names(input, &names),
                missing.map(|index| Symbol(index as u32))
            );
            names.truncate(100);
            let expected: Vec<_> = [0, 42, 43, 85, 86]
                .into_iter()
                .chain(100..129)
                .map(Symbol)
                .collect();
            assert_eq!(heap.missing_attr_names(input, &names), expected);
        });
    })
    .unwrap();
}

#[test]
fn filled_environments_keep_parent_and_placeholder_aliases_through_collection() {
    with_heap(stress(), |heap| {
        for len in [0, 1, 8, 9, 65] {
            for with_parent in [false, true] {
                heap.with_roots(|heap, outer| {
                    let frame = heap.with_roots(|heap, roots| {
                        let parent = if with_parent {
                            let value = heap.cell(roots, roots.int(42)).unwrap();
                            Some(heap.env(roots, None, &[value]).unwrap())
                        } else {
                            None
                        };
                        let uninit = heap.cell(roots, roots.blackhole()).unwrap();
                        let placeholder = heap.persist(uninit);
                        let frame = heap
                            .env_filled(roots, parent, len, placeholder.root())
                            .unwrap();
                        for index in 0..len {
                            let slot = heap.env_view(frame).get(index).unwrap().root(roots);
                            assert!(heap.same_cell(slot, placeholder.root()));
                        }
                        if len != 0 {
                            let replacement = heap.cell(roots, roots.int(99)).unwrap();
                            heap.env_set(frame, len / 2, replacement);
                        }
                        outer.copy(frame)
                    });
                    // Only the resulting frame is rooted: its parent, unchanged
                    // placeholder aliases and replaced slot must all survive.
                    for _ in 0..2 {
                        heap.collect().unwrap();
                        let view = heap.env_view(frame);
                        assert_eq!(view.len(), len);
                        if with_parent {
                            let parent = view.up().unwrap();
                            assert!(matches!(
                                heap.read_view(parent.get(0).unwrap()),
                                ValueView::Int(42)
                            ));
                        } else {
                            assert!(view.up().is_none());
                        }
                        for index in 0..len {
                            let value = heap.read_view(view.get(index).unwrap());
                            if index == len / 2 {
                                assert!(matches!(value, ValueView::Int(99)));
                            } else {
                                assert!(matches!(value, ValueView::Blackhole));
                            }
                        }
                    }
                });
            }
        }
        heap.with_roots(|heap, roots| {
            let cell = heap.cell(roots, roots.int(1)).unwrap();
            let before = heap.used_bytes();
            assert!(matches!(
                heap.env_filled(roots, None, usize::MAX, cell),
                Err(Error::AllocationTooLarge)
            ));
            assert_eq!(heap.used_bytes(), before);
        });
    })
    .unwrap();
}

#[test]
fn single_slot_environments_trace_mixed_parents_and_arguments() {
    for stress_gc in [false, true] {
        with_heap(
            Config {
                stress: stress_gc,
                steady_threshold_bytes: None,
                trace_collections: false,
                threshold_bytes: usize::MAX,
                ..stress()
            },
            |heap| {
                heap.with_roots(|heap, roots| {
                    let frame = heap.with_roots(|heap, build| {
                        let number = heap.cell(build, build.int(42)).unwrap();
                        let parent = heap.env(build, None, &[number]).unwrap();
                        let text = heap.string(build, b"kept by the single slot").unwrap();
                        let argument = heap.cell(build, text).unwrap();
                        let first = heap.env_one(build, parent, argument).unwrap();
                        let middle = heap.env(build, Some(first), &[number, argument]).unwrap();
                        let frame = heap.env_one(build, middle, argument).unwrap();
                        roots.copy(frame)
                    });
                    for _ in 0..3 {
                        heap.collect().unwrap();
                        let view = heap.env_view(frame);
                        assert_eq!(view.len(), 1);
                        let ValueView::String(text) = heap.read_view(view.get(0).unwrap()) else {
                            panic!("argument string")
                        };
                        assert_eq!(text.bytes(), b"kept by the single slot");
                        let middle = view.up().unwrap();
                        assert_eq!(middle.len(), 2);
                        let first = middle.up().unwrap();
                        assert_eq!(first.len(), 1);
                        assert_eq!(first.get(0).unwrap().pointer, view.get(0).unwrap().pointer);
                        let parent = first.up().unwrap();
                        assert!(parent.up().is_none());
                        assert!(matches!(
                            heap.read_view(parent.get(0).unwrap()),
                            ValueView::Int(42)
                        ));
                        let usage = heap.raw.space_usage();
                        assert_eq!(usage.len(), DESCRIPTORS.len());
                        assert_eq!(usage[ENV_ONE].0, "single-slot environment");
                        assert_eq!(usage[ENV_ONE].1, 2 * size_of::<RawEnvOne>());
                    }
                    let replacement = heap.cell(roots, roots.int(99)).unwrap();
                    heap.env_set(frame, 0, replacement);
                    heap.collect().unwrap();
                    assert!(matches!(
                        heap.read_view(heap.env_view(frame).get(0).unwrap()),
                        ValueView::Int(99)
                    ));
                });
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), 0);
            },
        )
        .unwrap();
    }
}

#[test]
fn lambda_owned_single_slot_frame_reloads_and_keeps_captured_graph() {
    for collect_each_allocation in [false, true] {
        with_heap(
            Config {
                stress: collect_each_allocation,
                ..stress()
            },
            |heap| {
                heap.with_roots(|heap, roots| {
                    let frame = heap.with_roots(|heap, sources| {
                        let function = heap.with_roots(|heap, capture| {
                            let text = heap
                                .string(capture, b"only the lambda owns this capture")
                                .unwrap();
                            let captured = heap.cell(capture, text).unwrap();
                            let environment = heap.env(capture, None, &[captured]).unwrap();
                            let function = heap.lambda(capture, environment, GlobalNode(123));
                            sources.copy(function)
                        });
                        let argument = heap.cell(sources, sources.int(42)).unwrap();
                        let before = heap.stats().collections;
                        let frame = heap
                            .env_one_from_lambda(sources, function, argument)
                            .unwrap();
                        if collect_each_allocation {
                            assert_eq!(heap.stats().collections, before + 1);
                        }
                        roots.copy(frame)
                    });
                    for _ in 0..3 {
                        heap.collect().unwrap();
                        let frame = heap.env_view(frame);
                        assert_eq!(frame.len(), 1);
                        assert!(matches!(
                            heap.read_view(frame.get(0).unwrap()),
                            ValueView::Int(42)
                        ));
                        let parent = frame.up().unwrap();
                        let ValueView::String(text) = heap.read_view(parent.get(0).unwrap()) else {
                            panic!("captured string")
                        };
                        assert_eq!(text.bytes(), b"only the lambda owns this capture");
                    }
                });
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), 0);
            },
        )
        .unwrap();
    }
}

#[test]
fn lambda_owned_single_slot_frame_rejects_invalid_source_and_recovers_from_oom() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let argument = heap.cell(roots, roots.int(42)).unwrap();
            let parent = heap.env(roots, None, &[argument]).unwrap();
            let function = heap.lambda(roots, parent, GlobalNode(123));
            let before = heap.used_bytes();
            let collections = heap.stats().collections;
            assert!(matches!(
                heap.env_one_from_lambda(roots, roots.null(), argument),
                Err(Error::InvalidRange)
            ));
            assert_eq!(heap.used_bytes(), before);
            assert_eq!(heap.stats().collections, collections);
            let failure = with_slow_reservation_failure(|| {
                heap.env_one_from_lambda(roots, function, argument)
            });
            assert!(matches!(failure, Err(Error::OutOfMemory)));
            assert_eq!(heap.stats().collections, collections + 1);
            heap.collect().unwrap();
            assert_eq!(heap.used_bytes(), before);
            let frame = heap.env_one_from_lambda(roots, function, argument).unwrap();
            heap.collect().unwrap();
            let view = heap.env_view(frame);
            assert!(matches!(
                heap.read_view(view.get(0).unwrap()),
                ValueView::Int(42)
            ));
            assert!(matches!(
                heap.read_view(view.up().unwrap().get(0).unwrap()),
                ValueView::Int(42)
            ));
        });
    })
    .unwrap();
}

#[test]
fn single_slot_environment_reservation_failure_preserves_relocated_inputs() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let value = heap.cell(roots, roots.int(42)).unwrap();
            let parent = heap.env(roots, None, &[value]).unwrap();
            heap.collect().unwrap();
            let baseline = heap.used_bytes();
            let before = heap.stats().collections;
            let result = with_slow_reservation_failure(|| heap.env_one(roots, parent, value));
            assert!(matches!(result, Err(Error::OutOfMemory)));
            assert_eq!(heap.stats().collections, before + 1);
            heap.collect().unwrap();
            assert_eq!(heap.used_bytes(), baseline);
            assert!(matches!(heap.read(value), ValueView::Int(42)));
            assert!(matches!(
                heap.read_view(heap.env_view(parent).get(0).unwrap()),
                ValueView::Int(42)
            ));
            // The injected failure must restore its state during unwinding too.
            let panic = std::panic::catch_unwind(|| {
                with_slow_reservation_failure(|| panic!("test unwinding"));
            });
            assert!(panic.is_err());
            let frame = heap.env_one(roots, parent, value).unwrap();
            heap.collect().unwrap();
            let view = heap.env_view(frame);
            assert_eq!(view.len(), 1);
            assert!(view.up().is_some());
            assert!(matches!(
                heap.read_view(view.get(0).unwrap()),
                ValueView::Int(42)
            ));
        });
    })
    .unwrap();
}

#[test]
fn tree_merge_rejects_a_flat_union_without_publishing_a_malformed_header() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let value = heap.cell(roots, roots.int(7)).unwrap();
            let attrs: Vec<_> = (0..40)
                .map(|i| RootedAttr {
                    name: Symbol(i),
                    pos: SrcPos(i + 1),
                    value,
                })
                .collect();
            let set =
                crate::managed_bindings::from_sorted(heap, roots, &attrs, SrcPos(42)).unwrap();
            heap.collect().unwrap();
            let occupied = heap.used_bytes();
            assert!(matches!(
                heap.merge_tree_bindings(roots, set, set),
                Err(Error::InvalidRange)
            ));
            heap.collect().unwrap();
            assert_eq!(heap.used_bytes(), occupied);
            assert_eq!(heap.bindings_view(set).len(), 40);
            assert_eq!(heap.bindings_view(set).pos(), SrcPos(42));
        });
    })
    .unwrap();
}

#[test]
fn comparison_batches_resume_in_symbol_order_after_moving_collections() {
    for len in [0, 1, 31, 32, 33, 64, 65, 129, 4097] {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let a = heap.cell(roots, roots.int(42)).unwrap();
                let b = heap.cell(roots, roots.int(43)).unwrap();
                let make = |cell, start, stride| {
                    (0..len)
                        .map(|i| RootedAttr {
                            name: Symbol(start + i as u32 * stride),
                            pos: SrcPos::NONE,
                            value: cell,
                        })
                        .collect::<Vec<_>>()
                };
                let left = crate::managed_bindings::from_sorted(
                    heap,
                    roots,
                    &make(a, 10, 2),
                    SrcPos::NONE,
                )
                .unwrap();
                let right = crate::managed_bindings::from_sorted(
                    heap,
                    roots,
                    &make(b, 11, 3),
                    SrcPos::NONE,
                )
                .unwrap();
                let mut after = None;
                let mut index = 0;
                loop {
                    let count = roots.with_roots(|temp| {
                        let pairs = heap.bindings_view(left).comparison_batch(
                            heap.bindings_view(right),
                            temp,
                            after,
                        );
                        assert!(pairs.len() <= 32);
                        let count = pairs.len();
                        heap.collect().unwrap();
                        for (name, other, p, q) in pairs {
                            assert_eq!(name, Symbol(10 + index as u32 * 2));
                            assert_eq!(other, Symbol(11 + index as u32 * 3));
                            assert!(matches!(heap.read(p), ValueView::Int(42)));
                            assert!(matches!(heap.read(q), ValueView::Int(43)));
                            after = Some((name, other));
                            index += 1;
                        }
                        count
                    });
                    if count == 0 {
                        break;
                    }
                }
                assert_eq!(index, len);
            });
        })
        .unwrap();
    }
}

#[test]
fn borrowed_binding_lookup_preserves_fields_and_misses_across_layouts_and_moves() {
    for len in [0, 1, 8, 9, 64, 65, 129, 4097] {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let value = heap.cell(roots, roots.int(42)).unwrap();
                let attrs: Vec<_> = (0..len)
                    .map(|index| RootedAttr {
                        name: Symbol(10 + index as u32 * 2),
                        pos: SrcPos(100 + index as u32),
                        value,
                    })
                    .collect();
                let set =
                    crate::managed_bindings::from_sorted(heap, roots, &attrs, SrcPos(7)).unwrap();
                for _ in 0..2 {
                    heap.collect().unwrap();
                    let view = heap.bindings_view(set);
                    assert!(view.get(Symbol(1)).is_none());
                    assert!(view.get(Symbol(10 + len as u32 * 2)).is_none());
                    for index in 0..len {
                        let name = Symbol(10 + index as u32 * 2);
                        let found = view.get(name).unwrap();
                        assert_eq!(found.name, name);
                        assert_eq!(found.pos, SrcPos(100 + index as u32));
                        assert!(heap.same_cell(found.value.root(roots), value));
                        assert!(view.get(Symbol(name.0 + 1)).is_none());
                    }
                }
            });
        })
        .unwrap();
    }
}

fn thunk_plan_attrs(len: usize) -> Vec<ThunkAttr> {
    (0..len)
        .map(|i| ThunkAttr {
            name: Symbol(i as u32),
            pos: SrcPos(100 + i as u32),
            node: GlobalNode(1000 + i as u32),
        })
        .collect()
}

fn check_thunk_attrs<'h>(heap: &Heap<'h>, output: BindingsRoot<'_, 'h>, len: usize) {
    let output = heap.bindings_view(output);
    assert_eq!(output.len(), len);
    assert_eq!(output.pos(), SrcPos(77));
    let mut shared_env = None;
    for (i, attr) in output.iter().enumerate() {
        assert_eq!(attr.name, Symbol(i as u32));
        assert_eq!(attr.pos, SrcPos(100 + i as u32));
        let ValueView::Thunk { env, node } = heap.read_view(attr.value) else {
            panic!("unforced thunk")
        };
        assert_eq!(node, GlobalNode(1000 + i as u32));
        if let Some(previous) = shared_env {
            assert_eq!(env.pointer, previous);
        }
        shared_env = Some(env.pointer);
        assert!(matches!(
            heap.read_view(env.get(0).unwrap()),
            ValueView::Int(42)
        ));
    }
}

#[test]
fn flat_thunk_plan_checks_bounds_and_order_once() {
    assert!(matches!(
        FlatThunkPlan::new(SrcPos::NONE, &[]),
        Err(Error::InvalidRange)
    ));
    let mut attrs = thunk_plan_attrs(65);
    assert!(matches!(
        FlatThunkPlan::new(SrcPos::NONE, &attrs),
        Err(Error::InvalidRange)
    ));
    attrs.truncate(64);
    assert!(FlatThunkPlan::new(SrcPos::NONE, &attrs).is_ok());
    attrs.swap(0, 1);
    assert!(matches!(
        FlatThunkPlan::new(SrcPos::NONE, &attrs),
        Err(Error::InvalidRange)
    ));
    attrs[1].name = attrs[0].name;
    assert!(matches!(
        FlatThunkPlan::new(SrcPos::NONE, &attrs),
        Err(Error::InvalidRange)
    ));
}

fn mixed_lazy_attrs() -> Vec<LazyAttr> {
    [
        LazyAttrSource::Thunk(GlobalNode(100)),
        LazyAttrSource::Variable {
            level: 1,
            displ: 0,
            fallback: GlobalNode(101),
        },
        LazyAttrSource::Variable {
            level: 0,
            displ: 0,
            fallback: GlobalNode(102),
        },
        LazyAttrSource::Literal(0),
        LazyAttrSource::EmptyList,
        LazyAttrSource::Literal(0),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, source)| LazyAttr {
        name: Symbol(index as u32),
        pos: SrcPos(100 + index as u32),
        source,
    })
    .collect()
}

#[test]
fn mixed_flat_lazy_batch_shares_cells_and_keeps_fallback_thunks_after_moves() {
    let attrs = mixed_lazy_attrs();
    let plan = FlatLazyPlan::new(SrcPos(77), &attrs).unwrap();
    for stress_gc in [false, true] {
        with_heap(Config { stress: stress_gc, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: usize::MAX, ..stress() }, |heap| {
            heap.with_roots(|heap, roots| {
                let output = roots.with_roots(|build| {
                    let sentinel = heap.cell(build, build.blackhole()).unwrap();
                    let variable = heap.cell(build, build.int(42)).unwrap();
                    let parent = heap.env(build, None, &[variable]).unwrap();
                    let env = heap.env(build, Some(parent), &[sentinel]).unwrap();
                    let text = heap.string(build, b"literal\xff").unwrap();
                    let literal = heap.cell(build, text).unwrap();
                    let literals = [Some(heap.persist(literal))];
                    let empty = heap.list(build, &[]).unwrap();
                    let empty = heap.cell(build, empty).unwrap();
                    let before = heap.stats().collections;
                    let output = heap.flat_lazy_attrs(build, env, &plan, &literals, empty, sentinel).unwrap();
                    assert_eq!(heap.stats().collections, before + usize::from(stress_gc));
                    let view = heap.bindings_view(output);
                    assert!(heap.same_cell(view.get(Symbol(1)).unwrap().value.root(build), variable));
                    assert!(heap.same_cell(view.get(Symbol(3)).unwrap().value.root(build), literal));
                    assert!(heap.same_cell(view.get(Symbol(4)).unwrap().value.root(build), empty));
                    roots.copy(output)
                });
                for _ in 0..3 {
                    heap.collect().unwrap();
                    let view = heap.bindings_view(output);
                    assert_eq!(view.pos(), SrcPos(77));
                    assert_eq!(view.len(), 6);
                    for (i, attr) in view.iter().enumerate() { assert_eq!(attr.pos, SrcPos(100 + i as u32)); }
                    assert!(matches!(heap.read_view(view.get(Symbol(0)).unwrap().value), ValueView::Thunk { node: GlobalNode(100), .. }));
                    assert!(matches!(heap.read_view(view.get(Symbol(2)).unwrap().value), ValueView::Thunk { node: GlobalNode(102), .. }));
                    assert!(matches!(heap.read_view(view.get(Symbol(1)).unwrap().value), ValueView::Int(42)));
                    assert!(matches!(heap.read_view(view.get(Symbol(3)).unwrap().value), ValueView::String(s) if s.bytes() == b"literal\xff"));
                    assert!(heap.same_cell(view.get(Symbol(3)).unwrap().value.root(roots), view.get(Symbol(5)).unwrap().value.root(roots)));
                    assert!(matches!(heap.read_view(view.get(Symbol(4)).unwrap().value), ValueView::List(l) if l.is_empty()));
                }
            });
        }).unwrap();
    }
}

#[test]
fn mixed_flat_lazy_batch_validates_inputs_and_recovers_at_every_allocation_boundary() {
    let attrs = mixed_lazy_attrs();
    let plan = FlatLazyPlan::new(SrcPos(77), &attrs).unwrap();
    assert!(FlatLazyPlan::new(SrcPos::NONE, &[]).is_err());
    let mut unordered = attrs.clone();
    unordered.swap(0, 1);
    assert!(FlatLazyPlan::new(SrcPos::NONE, &unordered).is_err());
    for fail_after in 0..=3 {
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let sentinel = heap.cell(roots, roots.blackhole()).unwrap();
                let parent = heap.env(roots, None, &[sentinel]).unwrap();
                let env = heap.env(roots, Some(parent), &[sentinel]).unwrap();
                let literal = heap.cell(roots, roots.int(42)).unwrap();
                let literals = [Some(heap.persist(literal))];
                let before = heap.stats().collections;
                assert!(matches!(
                    heap.flat_lazy_attrs(roots, env, &plan, &[], literal, sentinel),
                    Err(Error::InvalidRange)
                ));
                assert!(matches!(
                    heap.flat_lazy_attrs(roots, parent, &plan, &literals, literal, sentinel),
                    Err(Error::InvalidRange)
                ));
                assert_eq!(heap.stats().collections, before);
                heap.collect().unwrap();
                let before = heap.used_bytes();
                let result = with_flat_thunk_failure(fail_after, || {
                    heap.flat_lazy_attrs(roots, env, &plan, &literals, literal, sentinel)
                });
                // One explicit thunk, two sentinel fallbacks, then the header.
                assert!(matches!(result, Err(Error::OutOfMemory)));
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), before);
                let result = heap
                    .flat_lazy_attrs(roots, env, &plan, &literals, literal, sentinel)
                    .unwrap();
                assert_eq!(heap.bindings_view(result).len(), 6);
            })
        })
        .unwrap();
    }
}

#[test]
fn mixed_flat_lazy_batch_budgets_only_cells_that_need_allocation() {
    let attrs: Vec<_> = (0..64)
        .map(|index| LazyAttr {
            name: Symbol(index),
            pos: SrcPos::NONE,
            source: LazyAttrSource::Variable {
                level: 0,
                displ: 0,
                fallback: GlobalNode(0),
            },
        })
        .collect();
    let plan = FlatLazyPlan::new(SrcPos::NONE, &attrs).unwrap();
    with_heap(
        Config { workers: 0,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1536,
            stress: false,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let sentinel = heap.cell(roots, roots.blackhole()).unwrap();
                let value = heap.cell(roots, roots.int(42)).unwrap();
                let env = heap.env(roots, None, &[value]).unwrap();
                let before = heap.stats().collections;
                let used = heap.used_bytes();
                let result = heap
                    .flat_lazy_attrs(roots, env, &plan, &[], value, sentinel)
                    .unwrap();
                assert_eq!(heap.stats().collections, before);
                assert_eq!(
                    heap.used_bytes() - used,
                    heap.raw
                        .allocation_extent(FLAT_BINDINGS, plan.binding_bytes)
                        .unwrap()
                );
                assert_eq!(heap.bindings_view(result).len(), 64);
            });
        },
    )
    .unwrap();
}

#[test]
fn mixed_flat_lazy_scratch_rebuild_handles_the_highest_mask_bit() {
    let attrs: Vec<_> = (0..64)
        .map(|index| LazyAttr {
            name: Symbol(index),
            pos: SrcPos(index + 1),
            source: match index {
                0 | 63 => LazyAttrSource::Thunk(GlobalNode(1000 + index)),
                31 => LazyAttrSource::Variable {
                    level: 0,
                    displ: 1,
                    fallback: GlobalNode(1031),
                },
                _ => LazyAttrSource::Variable {
                    level: 0,
                    displ: 0,
                    fallback: GlobalNode(0),
                },
            },
        })
        .collect();
    let plan = FlatLazyPlan::new(SrcPos(77), &attrs).unwrap();
    for stress_gc in [false, true] {
        with_heap(Config { stress: stress_gc, steady_threshold_bytes: None, trace_collections: false, threshold_bytes: usize::MAX, ..stress() }, |heap| {
            heap.with_roots(|heap, roots| {
                let sentinel = heap.cell(roots, roots.blackhole()).unwrap();
                let variable = heap.cell(roots, roots.int(42)).unwrap();
                let env = heap.env(roots, None, &[variable, sentinel]).unwrap();
                let before = heap.stats().collections;
                let first = heap.flat_lazy_attrs(roots, env, &plan, &[], variable, sentinel).unwrap();
                assert_eq!(heap.stats().collections, before + usize::from(stress_gc));
                let replacement = heap.cell(roots, roots.int(99)).unwrap();
                heap.env_set(env, 0, replacement);
                let second = heap.flat_lazy_attrs(roots, env, &plan, &[], variable, sentinel).unwrap();
                heap.collect().unwrap();
                for (set, expected) in [(first, 42), (second, 99)] {
                    let view = heap.bindings_view(set);
                    assert_eq!(view.len(), 64);
                    for attr in view.iter() {
                        let index = attr.name.0;
                        if matches!(index, 0 | 31 | 63) {
                            assert!(matches!(heap.read_view(attr.value), ValueView::Thunk { node, .. } if node == GlobalNode(1000 + index)));
                        } else {
                            assert!(matches!(heap.read_view(attr.value), ValueView::Int(value) if value == expected));
                        }
                    }
                }
            });
        }).unwrap();
    }
}

#[test]
fn merge_cursor_identity_clears_only_for_consumed_fragments_and_exhaustion() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let cell = heap.cell(roots, roots.int(42)).unwrap();
            let attrs: Vec<_> = (0..129)
                .map(|index| RootedAttr {
                    name: Symbol(index),
                    pos: SrcPos::NONE,
                    value: cell,
                })
                .collect();
            let tree =
                crate::managed_bindings::from_sorted(heap, roots, &attrs, SrcPos::NONE).unwrap();
            let flat = heap
                .alloc_flat_bindings(roots, SrcPos::NONE, &attrs[..1])
                .unwrap();
            let empty = heap.alloc_flat_bindings(roots, SrcPos::NONE, &[]).unwrap();
            let mut cursor = MergeLeafCursor::new(heap.bindings_view(tree));
            let first = cursor.current_whole.expect("whole first leaf");
            cursor.consume(0);
            assert_eq!(cursor.current_whole, Some(first));
            cursor.consume(1);
            assert_eq!(cursor.current_whole, None);
            cursor.consume(0);
            assert_eq!(cursor.current_whole, None);
            cursor.consume(cursor.inner.leaf.len());
            assert!(cursor.current_whole.is_some());
            assert_ne!(cursor.current_whole, Some(first));
            while !cursor.inner.leaf.is_empty() {
                assert_eq!(cursor.current_whole, cursor.inner.whole_leaf_node());
                cursor.consume(cursor.inner.leaf.len());
            }
            assert_eq!(cursor.current_whole, None);
            cursor.consume(0);
            assert_eq!(cursor.current_whole, None);
            for set in [flat, empty] {
                let mut cursor = MergeLeafCursor::new(heap.bindings_view(set));
                assert_eq!(cursor.current_whole, None);
                cursor.consume(cursor.inner.leaf.len());
                cursor.consume(0);
                assert!(cursor.inner.leaf.is_empty());
                assert_eq!(cursor.current_whole, None);
            }
        })
    })
    .unwrap();
}

#[test]
fn flat_thunk_batch_preserves_captured_environment_positions_and_laziness() {
    for stress_gc in [false, true] {
        for len in [1, 16, 64] {
            let attrs = thunk_plan_attrs(len);
            let plan = FlatThunkPlan::new(SrcPos(77), &attrs).unwrap();
            with_heap(
                Config {
                    stress: stress_gc,
                    steady_threshold_bytes: None,
                    trace_collections: false,
                    threshold_bytes: usize::MAX,
                    ..stress()
                },
                |heap| {
                    heap.with_roots(|heap, roots| {
                        let output = heap.with_roots(|heap, build| {
                            let value = heap.cell(build, build.int(42)).unwrap();
                            let env = heap.env(build, None, &[value]).unwrap();
                            let before = heap.stats().collections;
                            let output = heap.flat_thunk_attrs(build, env, &plan).unwrap();
                            assert_eq!(heap.stats().collections - before, usize::from(stress_gc));
                            roots.copy(output)
                        });
                        for _ in 0..3 {
                            heap.collect().unwrap();
                            check_thunk_attrs(heap, output, len);
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(heap.used_bytes(), 0);
                },
            )
            .unwrap();
        }
    }
}

#[test]
fn flat_thunk_batch_accounts_for_pending_cells_and_attrs() {
    let attrs = thunk_plan_attrs(64);
    let plan = FlatThunkPlan::new(SrcPos(77), &attrs).unwrap();
    with_heap(
        Config { workers: 0,
            stress: false,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1024,
            chunk_bytes: 128,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let value = heap.cell(roots, roots.int(42)).unwrap();
                let env = heap.env(roots, None, &[value]).unwrap();
                assert!(heap.used_bytes() < 1024);
                let before = heap.stats().collections;
                let output = heap.flat_thunk_attrs(roots, env, &plan).unwrap();
                assert_eq!(heap.stats().collections, before + 1);
                check_thunk_attrs(heap, output, 64);
            });
        },
    )
    .unwrap();
}

#[test]
fn partial_flat_thunk_batches_are_initialized_at_every_failure_boundary() {
    for len in [3, 64] {
        let attrs = thunk_plan_attrs(len);
        let plan = FlatThunkPlan::new(SrcPos(77), &attrs).unwrap();
        with_heap(stress(), |heap| {
            heap.with_roots(|heap, roots| {
                let value = heap.cell(roots, roots.int(42)).unwrap();
                let env = heap.env(roots, None, &[value]).unwrap();
                heap.collect().unwrap();
                let baseline = heap.used_bytes();
                for failure in 0..=len + 1 {
                    heap.with_roots(|heap, temporary| {
                        let result = with_flat_thunk_failure(failure, || {
                            heap.flat_thunk_attrs(temporary, env, &plan)
                        });
                        if failure <= len {
                            assert!(
                                matches!(result, Err(Error::OutOfMemory)),
                                "boundary {failure}"
                            );
                        } else {
                            check_thunk_attrs(heap, result.unwrap(), len);
                        }
                    });
                    heap.collect().unwrap();
                    assert_eq!(heap.used_bytes(), baseline, "boundary {failure}");
                    assert!(matches!(
                        heap.read_view(heap.env_view(env).get(0).unwrap()),
                        ValueView::Int(42)
                    ));
                }
            });
        })
        .unwrap();
    }
}

#[test]
fn concatenated_bytes_reload_aliases_and_interior_parts_after_collection() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let joined = roots.with_roots(|temporary| {
                let original = heap.string(temporary, b"ab\xffcd").unwrap();
                let interior = heap.substring(temporary, original, 1..4).unwrap();
                let empty = heap.string(temporary, b"").unwrap();
                let before = heap.stats().collections;
                let joined = heap.concat_string_bytes(temporary, &[original, empty, interior, original]).unwrap();
                assert_eq!(heap.stats().collections, before + 1);
                assert_eq!(heap.bytes_view(joined), b"ab\xffcdb\xffcab\xffcd");
                roots.copy(joined)
            });
            heap.collect().unwrap();
            assert_eq!(heap.bytes_view(joined), b"ab\xffcdb\xffcab\xffcd");
            let text = heap.string_from_bytes(roots, joined, roots.empty_context()).unwrap();
            heap.collect().unwrap();
            assert!(matches!(heap.view(text), ValueView::String(value) if value.bytes() == b"ab\xffcdb\xffcab\xffcd"));
            let before = heap.used_bytes();
            let empty = heap.concat_string_bytes(roots, &[]).unwrap();
            assert!(heap.bytes_view(empty).is_empty());
            assert_eq!(heap.used_bytes(), before);
        });
    }).unwrap();
}

#[test]
fn concatenated_bytes_reservation_failure_preserves_relocated_inputs() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let left = heap.string(roots, b"left").unwrap();
            let right = heap.string(roots, b"right").unwrap();
            heap.collect().unwrap();
            let before = heap.used_bytes();
            let result =
                with_slow_reservation_failure(|| heap.concat_string_bytes(roots, &[left, right]));
            assert!(matches!(result, Err(Error::OutOfMemory)));
            heap.collect().unwrap();
            assert_eq!(heap.used_bytes(), before);
            assert!(
                matches!(heap.view(left), ValueView::String(value) if value.bytes() == b"left")
            );
            assert!(
                matches!(heap.view(right), ValueView::String(value) if value.bytes() == b"right")
            );
            let joined = heap.concat_string_bytes(roots, &[left, right]).unwrap();
            assert_eq!(heap.bytes_view(joined), b"leftright");
        });
    })
    .unwrap();
}

fn owned_contexts<'h>(
    heap: &Heap<'h>,
    value: ValueRoot<'_, 'h>,
) -> Vec<crate::managed_builtins_strings::OwnedContext> {
    use crate::managed_builtins_strings::OwnedContext as O;
    let ValueView::String(value) = heap.view(value) else {
        panic!("string")
    };
    value
        .contexts()
        .map(|entry| match entry {
            ContextView::LazyHash { offset, tree } => O::LazyHash { offset, tree },
            ContextView::LazyPath(tree) => O::LazyPath(tree),
            ContextView::Opaque(path) => O::Opaque(path.to_vec()),
            ContextView::DrvDeep(path) => O::DrvDeep(path.to_vec()),
            ContextView::Built { drv, output } => O::Built {
                drv: drv.to_vec(),
                output: output.to_vec(),
            },
            ContextView::Path(path) => O::Path(path.to_vec()),
        })
        .collect()
}

#[test]
fn concatenated_contexts_share_bytes_sort_by_contents_and_survive_owner_drop() {
    for force_gc in [false, true] {
        let mut config = stress();
        config.stress = force_gc;
        config.threshold_bytes = usize::MAX;
        with_heap(config, |heap| {
            heap.with_roots(|heap, roots| {
                let (result, expected) = roots.with_roots(|temporary| {
                    let owner = heap.string(temporary, b"!drv\xff!").unwrap();
                    let interior = heap.substring(temporary, owner, 1..5).unwrap();
                    let ValueView::String(interior) = heap.view(interior) else {
                        unreachable!()
                    };
                    let drv = interior.bytes_root(temporary);
                    let duplicate_drv = heap.bytes(temporary, b"drv\xff").unwrap();
                    let out_a = heap.bytes(temporary, b"a").unwrap();
                    let out_z = heap.bytes(temporary, b"z").unwrap();
                    let context = heap
                        .contexts(
                            temporary,
                            &[
                                ContextInput::Path(drv),
                                ContextInput::Built { drv, output: out_z },
                                ContextInput::Opaque(drv),
                                ContextInput::LazyHash { offset: 3, tree: 9 },
                                ContextInput::LazyHash { offset: 3, tree: 2 },
                                ContextInput::DrvDeep(drv),
                                ContextInput::LazyPath(7),
                                ContextInput::Built {
                                    drv: duplicate_drv,
                                    output: out_a,
                                },
                                ContextInput::Opaque(duplicate_drv),
                            ],
                        )
                        .unwrap();
                    let source = heap
                        .string_with_context(temporary, b"text", context)
                        .unwrap();
                    let mut expected = owned_contexts(heap, source);
                    for entry in &mut expected {
                        if let crate::managed_builtins_strings::OwnedContext::LazyHash {
                            offset,
                            ..
                        } = entry
                        {
                            *offset += 11;
                        }
                    }
                    expected.sort_unstable();
                    expected.dedup();
                    let byte_usage = heap.space_usage()[BYTES].1;
                    let before = heap.stats().collections;
                    let context = heap
                        .concat_string_contexts(temporary, &[(source, 11), (source, 11)])
                        .unwrap();
                    assert_eq!(
                        heap.space_usage()[BYTES].1,
                        byte_usage,
                        "context bytes must be shared"
                    );
                    assert_eq!(heap.stats().collections, before + usize::from(force_gc));
                    let result = heap.string_with_context(temporary, b"", context).unwrap();
                    (roots.copy(result), expected)
                });
                for _ in 0..3 {
                    heap.collect().unwrap();
                    assert_eq!(owned_contexts(heap, result), expected);
                }
                let before = heap.used_bytes();
                let empty = heap.concat_string_contexts(roots, &[]).unwrap();
                assert_eq!(empty.raw.read().len, 0);
                assert_eq!(heap.used_bytes(), before);
            })
        })
        .unwrap();
    }
}

#[test]
fn concatenated_contexts_validate_before_gc_and_recover_from_allocation_failure() {
    with_heap(stress(), |heap| {
        heap.with_roots(|heap, roots| {
            let context = heap
                .contexts(
                    roots,
                    &[ContextInput::LazyHash {
                        offset: u32::MAX,
                        tree: 7,
                    }],
                )
                .unwrap();
            let source = heap.string_with_context(roots, b"x", context).unwrap();
            let before = heap.stats().collections;
            assert!(matches!(
                heap.concat_string_contexts(roots, &[(source, 1)]),
                Err(Error::AllocationTooLarge)
            ));
            assert!(matches!(
                heap.concat_string_contexts(roots, &[(roots.int(0), 0)]),
                Err(Error::InvalidRange)
            ));
            assert_eq!(heap.stats().collections, before);
            let expected = owned_contexts(heap, source);
            heap.collect().unwrap();
            let before = heap.used_bytes();
            let failure = with_slow_reservation_failure(|| {
                heap.concat_string_contexts(roots, &[(source, 0)])
            });
            assert!(matches!(failure, Err(Error::OutOfMemory)));
            heap.collect().unwrap();
            assert_eq!(heap.used_bytes(), before);
            assert_eq!(owned_contexts(heap, source), expected);
            let context = heap.concat_string_contexts(roots, &[(source, 0)]).unwrap();
            let result = heap.string_with_context(roots, b"", context).unwrap();
            assert_eq!(owned_contexts(heap, result), expected);
        })
    })
    .unwrap();
}

#[test]
fn concatenated_contexts_count_pending_extent_at_real_threshold() {
    with_heap(
        Config { workers: 0,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1024,
            stress: false,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let entries: Vec<_> = (0..12).map(ContextInput::LazyPath).collect();
                let context = heap.contexts(roots, &entries).unwrap();
                let source = heap.string_with_context(roots, b"", context).unwrap();
                let before = heap.stats().collections;
                let result = heap
                    .concat_string_contexts(roots, &[(source, 0), (source, 0)])
                    .unwrap();
                assert_eq!(heap.stats().collections, before + 1);
                assert_eq!(result.raw.read().len, 12);
            });
        },
    )
    .unwrap();
}

#[test]
fn fused_concat_initializes_every_failure_boundary_and_retries_after_gc() {
    for fail_after in 0..=3 {
        with_heap(stress(), |heap| heap.with_roots(|heap, roots| {
            let left = heap.string(roots, b"left\xff").unwrap();
            let path = heap.bytes(roots, b"drv\xff").unwrap();
            let context = heap.contexts(roots, &[
                ContextInput::Opaque(path),
                ContextInput::LazyHash { offset: 1, tree: 7 },
            ]).unwrap();
            let right = heap.string_with_context(roots, b"right", context).unwrap();
            heap.collect().unwrap();
            let before = heap.used_bytes();
            let collections = heap.stats().collections;
            let result = with_concat_failure(fail_after, || heap.concat_strings(roots, &[left, right], &[(right, 5)]));
            assert_eq!(heap.stats().collections, collections + 1);
            if fail_after < 3 {
                assert!(matches!(result, Err(Error::OutOfMemory)));
                heap.collect().unwrap();
                assert_eq!(heap.used_bytes(), before, "failed output is reclaimable");
            } else {
                assert!(result.is_ok());
            }
            let joined = heap.concat_strings(roots, &[left, right], &[(right, 5)]).unwrap();
            assert!(matches!(heap.view(joined), ValueView::String(s) if s.bytes() == b"left\xffright"));
            assert_eq!(owned_contexts(heap, joined), vec![
                crate::managed_builtins_strings::OwnedContext::LazyHash { offset: 6, tree: 7 },
                crate::managed_builtins_strings::OwnedContext::Opaque(b"drv\xff".to_vec()),
            ]);
        })).unwrap();
    }
}

#[test]
fn fused_concat_shares_context_owners_and_preserves_empty_cache() {
    with_heap(stress(), |heap| heap.with_roots(|heap, roots| {
        let joined = roots.with_roots(|temporary| {
            let owner = heap.string(temporary, b"!drv\xff!").unwrap();
            let part = heap.substring(temporary, owner, 1..5).unwrap();
            let ValueView::String(part_view) = heap.view(part) else { unreachable!() };
            let bytes = part_view.bytes_root(temporary);
            let context = heap.contexts(temporary, &[ContextInput::Opaque(bytes)]).unwrap();
            let source = heap.string_with_context(temporary, b"x", context).unwrap();
            let before = heap.stats().collections;
            let usage = heap.space_usage()[BYTES].1;
            // Context-only separator contributes no output bytes, even empty.
            let joined = heap.concat_strings(temporary, &[], &[(source, 0)]).unwrap();
            assert_eq!(heap.stats().collections, before + 1);
            assert_eq!(heap.space_usage()[BYTES].1, usage);
            roots.copy(joined)
        });
        heap.collect().unwrap();
        assert_eq!(owned_contexts(heap, joined), vec![crate::managed_builtins_strings::OwnedContext::Opaque(b"drv\xff".to_vec())]);
        assert!(matches!(heap.view(joined), ValueView::String(s) if s.bytes().is_empty()));
        let empty = heap.concat_strings(roots, &[], &[]).unwrap();
        heap.collect().unwrap();
        let before = heap.used_bytes();
        let collections = heap.stats().collections;
        let again = heap.concat_strings(roots, &[empty], &[]).unwrap();
        assert_eq!(heap.used_bytes(), before);
        assert_eq!(heap.stats().collections, collections);
        let (ValueView::String(a), ValueView::String(b)) = (heap.view(empty), heap.view(again)) else { unreachable!() };
        assert_eq!(a.pointer, b.pointer);
        let plain = heap.string(roots, b"plain").unwrap();
        let before = heap.stats().collections;
        let plain_joined = heap.concat_strings(roots, &[plain, plain], &[]).unwrap();
        assert_eq!(heap.stats().collections, before + 1);
        assert!(matches!(heap.view(plain_joined), ValueView::String(s) if s.bytes() == b"plainplain"));
    })).unwrap();
}

#[test]
fn fused_concat_preflights_the_combined_extent_before_allocating_bytes() {
    with_heap(
        Config { workers: 0,
            chunk_bytes: 256,
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: 1024,
            stress: false,
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let entries: Vec<_> = (0..4).map(ContextInput::LazyPath).collect();
                let context = heap.contexts(roots, &entries).unwrap();
                let source = heap
                    .string_with_context(roots, &[b'x'; 400], context)
                    .unwrap();
                let bytes_extent = heap.raw.allocation_extent(BYTES, 400).unwrap();
                assert!(!heap.raw.collection_due(bytes_extent));
                let before = heap.stats().collections;
                let result = heap
                    .concat_strings(roots, &[source], &[(source, 0)])
                    .unwrap();
                assert_eq!(heap.stats().collections, before + 1);
                assert_eq!(owned_contexts(heap, result), owned_contexts(heap, source));
                assert!(
                    matches!(heap.view(result), ValueView::String(s) if s.bytes() == [b'x'; 400])
                );
            });
        },
    )
    .unwrap();
}

#[test]
fn old_cells_and_environments_keep_new_edges_through_minor_collection() {
    with_heap(
        Config {
            steady_threshold_bytes: None,
            trace_collections: false,
            threshold_bytes: usize::MAX,
            ..Config::default()
        },
        |heap| {
            heap.with_roots(|heap, roots| {
                let cell = heap.cell(roots, roots.null()).unwrap();
                let env = heap.env(roots, None, &[cell]).unwrap();
                heap.collect().unwrap();
                let old_bytes = heap.raw.old_bytes();
                heap.with_roots(|heap, inner| {
                    let child = heap.cell(inner, inner.int(71)).unwrap();
                    let captured = heap.env(inner, None, &[child]).unwrap();
                    heap.store_thunk(cell, captured, GlobalNode(19));
                });
                heap.collect_minor().unwrap();
                assert!(heap.raw.old_bytes() > old_bytes);
                let ValueView::Thunk {
                    env: captured,
                    node,
                } = heap.read(cell)
                else {
                    panic!("thunk lost");
                };
                assert_eq!(node, GlobalNode(19));
                assert!(matches!(
                    heap.read_view(captured.get(0).unwrap()),
                    ValueView::Int(71)
                ));
                heap.with_roots(|heap, inner| {
                    let child = heap.cell(inner, inner.int(99)).unwrap();
                    heap.env_set(env, 0, child);
                    let parent = heap.env(inner, None, &[child]).unwrap();
                    heap.env_set_up(env, Some(parent));
                    let value = heap.string(inner, b"young value in old cell").unwrap();
                    heap.store(cell, value);
                });
                heap.collect_minor().unwrap();
                let ValueView::String(text) = heap.read(cell) else {
                    panic!("string lost");
                };
                assert_eq!(text.bytes(), b"young value in old cell");
                assert!(matches!(
                    heap.read_view(heap.env_view(env).get(0).unwrap()),
                    ValueView::Int(99)
                ));
                let parent = heap.env_view(env).up().unwrap();
                assert!(matches!(
                    heap.read_view(parent.get(0).unwrap()),
                    ValueView::Int(99)
                ));
                assert_eq!(heap.stats().minor_collections, 2);
                heap.collect().unwrap();
                assert!(matches!(
                    heap.read_view(heap.env_view(env).get(0).unwrap()),
                    ValueView::Int(99)
                ));
            });
        },
    )
    .unwrap();
}
