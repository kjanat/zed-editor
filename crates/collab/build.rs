fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // This debug-test size warning is about unwind performance. Keep both
        // unwind formats intact: disabling compact unwind breaks panic cleanup.
        println!("cargo:rustc-link-arg-tests=-Wl,-no_warn_eh_frame_too_large");
    }
}
