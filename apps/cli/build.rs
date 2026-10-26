//! Native executable metadata and code paging.

fn main() {
    println!("cargo:rerun-if-changed=a.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_INDEX").is_some()
    {
        // Keep the index implementation out of daemon startup pages.
        println!("cargo:rerun-if-changed=cold.symbols");
        let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo");
        let symbols = std::path::Path::new(&dir).join("cold.symbols");
        println!("cargo:rustc-link-arg-bins=-Wl,-move_to_ro_segment,__TEXT_EXEC");
        println!("cargo:rustc-link-arg-bins=-Xlinker");
        println!("cargo:rustc-link-arg-bins={}", symbols.display());
    }
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo");
        let manifest = std::path::Path::new(&dir).join("a.exe.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
