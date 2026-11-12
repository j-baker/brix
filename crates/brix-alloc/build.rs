//! Compiles `tune.c`, the constructor that configures mimalloc before it
//! initialises (see the comment in that file for why it is C), against
//! the `mimalloc.h` of the mimalloc that libmimalloc-sys builds.

fn main() {
    println!("cargo:rerun-if-changed=tune.c");
    println!("cargo:rerun-if-env-changed=DEP_MIMALLOC_INCLUDE_DIR");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // libmimalloc-sys (`links = "mimalloc"`) publishes its include
        // directory to the crates that depend on it directly.
        let include = std::env::var_os("DEP_MIMALLOC_INCLUDE_DIR").expect(
            "DEP_MIMALLOC_INCLUDE_DIR is not set: libmimalloc-sys must be a direct dependency",
        );
        cc::Build::new()
            .file("tune.c")
            .include(include)
            // Optimised in every profile: Nix's compiler wrapper defines
            // _FORTIFY_SOURCE, which the C library's headers (through
            // mimalloc.h) warn about without optimisation.
            .opt_level(2)
            .compile("brix_alloc_tune");
    }
}
