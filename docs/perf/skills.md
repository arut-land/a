# Skill frontmatter parser

Measured on 2026-10-21, macOS arm64, nightly-2026-10-17, with
`CARGO_BUILD_JOBS=3`. The machine was shared. Compile durations below describe
this run, including cache/index/download contention; they establish no runtime,
startup or quiet-machine budget.

A standalone executable reads frontmatter from argv, decodes `name` and
`description`, and prints both, preventing elimination of the parser. All three
use the same `serde::Deserialize` header and release profile: `opt-level="s"`,
fat LTO, one codegen unit, stripped output. Dependencies were built sequentially
into one temporary target. The tiny implementation collects `split_once(':')`
lines into a BTreeMap and trims values; it deliberately accepts a flat subset
without YAML quoting, multiline strings or duplicate-field diagnostics.

| Parser | Executable bytes | Delta over tiny | Build wall seconds |
| --- | ---: | ---: | ---: |
| Tiny flat subset, std only | 302768 | 0 | 9.55 |
| serde_yaml_ng 0.10.0 | 402256 | 99488 | 3.83 |
| yaml-rust2 0.13.0, default features off | 418800 | 116032 | 3.57 |

`serde_yaml_ng` decodes directly into the owning crate's header type.
`yaml-rust2` first constructs a document tree and then extracts string fields.
The compiler was warm for the later builds, so compile times cannot be compared
as cold builds. The probe used the standard shipped std, rather than this
repository's build-std profile; compare deltas, not absolute binary sizes.

Choose [serde_yaml_ng](https://crates.io/crates/serde_yaml_ng): the maintained
Serde parser is smaller than the measured tree adapter, supports ordinary
portable YAML skill headers, and fits the 4 MiB product budget. The saved 97 KiB
of a tiny parser does not justify maintaining a second quoting and block-scalar
language. `serde_yaml` 0.9 is deprecated and was excluded rather than added as a
production dependency. `yaml-rust2` is the measured alternate maintained parser.
No handwritten YAML parser is shipped. Frontmatter has an 8 KiB limit and a
skill file a 64 KiB limit before decoding; aggregate catalog limits constrain
snapshot memory independently of model context. Filesystem calls have no
OS-enforced deadline.

The probe source is reproducible with this body (select one decode statement):

```rust
#[derive(serde::Deserialize)]
struct Header { name: String, description: String }
let text = std::env::args().nth(1).unwrap();
// tiny:
let fields: std::collections::BTreeMap<_, _> =
    text.lines().filter_map(|line| line.split_once(':')).collect();
let header = Header {
    name: fields["name"].trim().into(),
    description: fields["description"].trim().into(),
};
// serde_yaml_ng:
let header: Header = serde_yaml_ng::from_str(&text).unwrap();
// yaml-rust2:
let docs = yaml_rust2::YamlLoader::load_from_str(&text).unwrap();
let header = Header {
    name: docs[0]["name"].as_str().unwrap().into(),
    description: docs[0]["description"].as_str().unwrap().into(),
};
println!("{}: {}", header.name, header.description);
```

`mise run size`: **2975 KiB before, 3123 KiB before the main merge**
(+148 KiB for the extension, parser, watcher and SDK plumbing together).
After merging main and sharing workspace subscriptions, the integrated build
measures **3139 KiB**, below the 4096 KiB budget. The integrated delta includes
main's intervening changes. Budgets and startup are left to the coordinator
on a quiet machine, as requested.

Discovery also uses the existing `sys` crate's regular-file open seam. This
adds no third-party dependency: metadata rejects devices and FIFOs before open,
and Unix `O_NONBLOCK` prevents a replacement FIFO from waiting for a writer.
