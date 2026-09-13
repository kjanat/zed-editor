fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // These test binaries exceed the compact-unwind format's 24-bit DWARF
        // offset limit. Keep DWARF unwinding instead of generating that table.
        println!("cargo:rustc-link-arg-tests=-Wl,-no_compact_unwind");
        println!("cargo:rustc-link-arg-tests=-Wl,-keep_dwarf_unwind");
    }
}
