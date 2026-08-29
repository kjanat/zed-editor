fn main() {
    let cargo_toml =
        std::fs::read_to_string("../zed/Cargo.toml").expect("Failed to read crates/zed/Cargo.toml");
    let manifest = cargo_toml
        .parse::<toml::Table>()
        .expect("Failed to parse crates/zed/Cargo.toml");
    let version = manifest["package"]["version"]
        .as_str()
        .expect("Version not found in crates/zed/Cargo.toml");
    println!("cargo:rustc-env=ZED_PKG_VERSION={}", version);
}
