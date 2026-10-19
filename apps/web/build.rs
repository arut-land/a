//! Deflates topcoat's browser runtime once, at build time: the binary
//! embeds it, and serves it, compressed.

use std::path::PathBuf;

fn main() {
    let script = "assets/topcoat-runtime-0.10.0.js";
    println!("cargo::rerun-if-changed={script}");
    let bytes = std::fs::read(script).expect("the runtime script");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let deflated = miniz_oxide::deflate::compress_to_vec_zlib(&bytes, 10);
    std::fs::write(out.join("topcoat-runtime.js.zz"), deflated).expect("written");
}
