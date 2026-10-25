//! Embeds the Windows application manifest (`a.exe.manifest`) in `a.exe`.

fn main() {
    println!("cargo:rerun-if-changed=a.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo");
        let mut manifest = std::path::Path::new(&dir).join("a.exe.manifest");
        // Measured against the default heap (docs/perf/allocators.md).
        if std::env::var_os("CARGO_FEATURE_SEGMENT_HEAP").is_some() {
            let text = std::fs::read_to_string(&manifest).expect("the manifest is readable");
            let heap = "<heapType xmlns=\"http://schemas.microsoft.com/SMI/2020/WindowsSettings\">SegmentHeap</heapType>\n    </windowsSettings>";
            manifest = std::path::Path::new(&std::env::var_os("OUT_DIR").expect("set by cargo"))
                .join("a.exe.manifest");
            std::fs::write(&manifest, text.replace("</windowsSettings>", heap))
                .expect("OUT_DIR is writable");
        }
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
