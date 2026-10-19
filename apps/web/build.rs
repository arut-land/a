//! Deflates Datastar's browser script once, at build time: the binary
//! embeds it, and serves it, compressed.

use std::path::PathBuf;

fn main() {
    let script = "assets/datastar-1.0.4.js";
    println!("cargo::rerun-if-changed={script}");
    let bytes = std::fs::read(script).expect("the browser script");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let deflated = miniz_oxide::deflate::compress_to_vec_zlib(&bytes, 10);
    std::fs::write(out.join("datastar.js.zz"), deflated).expect("written");
}
