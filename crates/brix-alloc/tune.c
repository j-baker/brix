/* Turns off mimalloc's eager arena commit before mimalloc reserves its
 * first arena.
 *
 * The constructor sets the option's *default* (`mi_option_set_default`
 * leaves the option uninitialised), so a MIMALLOC_ARENA_EAGER_COMMIT the
 * user set still wins when mimalloc reads its options, and nothing is
 * added to the environment that child processes would inherit. mimalloc
 * reads its options from its own constructor (priority 101 under clang,
 * the default priority under GCC); this one may run before or after it,
 * and either way it runs before the first allocation through mimalloc,
 * which is what reserves an arena: nothing allocates through Rust's
 * global allocator before `main`. Priorities 0 to 100 are reserved for
 * the implementation.
 *
 * A Rust `#[used]` static in `.init_array` is discarded under thin LTO; a
 * C object is linked as-is, so the constructor lives here. The object is
 * only pulled into the link when something references it, which
 * `brix_alloc::init` does through `brix_alloc_arena_eager_commit`, and
 * `init` checks the option's value to make sure the constructor ran.
 *
 * mimalloc.h is the header of the mimalloc libmimalloc-sys compiles (see
 * build.rs), so the option's number is the one that library uses. */

#include <mimalloc.h>

__attribute__((constructor(101))) static void brix_alloc_tune_mimalloc(void) {
    mi_option_set_default(mi_option_arena_eager_commit, 0);
}

/* The option's value in force: 0 unless the user set it. */
long brix_alloc_arena_eager_commit(void) {
    return mi_option_get(mi_option_arena_eager_commit);
}
