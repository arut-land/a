# Skill frontmatter parser

The measured subset parser supersedes the earlier flat-parser comparison below.
It parses the portable skill header structures we collected, rather than treating
colon-separated lines as YAML. The production crate no longer depends on a YAML
library; the comparison libraries live only in an isolated measurement workspace.

## Corpus and contract

`crates/ext-skills/tests/corpus` holds 17 verbatim frontmatter excerpts, with
commit-pinned source URLs in `sources.json`. `refresh.py` retrieves those exact
headers without their instruction bodies. The sources are
[Anthropic skills](https://github.com/anthropics/skills),
[Claude Code plugins](https://github.com/anthropics/claude-code),
[OpenAI skills](https://github.com/openai/skills),
[pi skills](https://github.com/badlogic/pi-skills),
[pi loader fixtures](https://github.com/badlogic/pi-mono/tree/main/packages/coding-agent/test/fixtures/skills),
and [Trail of Bits skills](https://github.com/trailofbits/skills).
The corpus includes plain and double-quoted scalars, folded and literal block
scalars, nested metadata, and scalar and list forms of `allowed-tools`.
Single quotes, escaped quotes, comments, flow collections, deeper nested maps,
invalid syntax, CRLF, delimiters, and resource limits have separate grammar tests.
These constructed cases are not presented as upstream excerpts.

The oracle is the normalized `(name, description)` pair: all three adapters
must agree on every real header before `tests/measure/run.py` writes
`expected.json`. Ordinary crate tests use that snapshot without linking either
comparison library. Decoding is separate from catalog admission: the long
Anthropic `claude-api` description and Claude Code's `Agent Development` name violate this
product's existing 1024-byte description / lower-case command-name rules and
remain rejected at admission. No source header was shortened or renamed to pass.

The custom parser has an 8192-byte header limit and 16-level nesting limit;
the loader independently limits files to 64 KiB. Errors carry a typed kind and
1-based file line, including the opening delimiter. Metadata is validated but
never used as permissions. The parser has no unsafe code or panic paths.
It supports indented maps, scalar sequences and single-line flow collections;
plain, single-quoted and JSON-escaped double-quoted text; and description blocks
with chomping/indent indicators. Descriptions are whitespace-normalized, so no
YAML tree or exact block-chomping result needs to survive. Quoted block-map keys,
multiline plain/quoted scalars, tags, anchors, aliases and directives are rejected.
This is a documented subset, not a replacement general-purpose YAML library.

## Measurement

Measured on 2026-10-21, macOS arm64, nightly-2026-10-17, always
`CARGO_BUILD_JOBS=2`. Run from the repository root:

```sh
CARGO_BUILD_JOBS=2 python3 crates/ext-skills/tests/measure/run.py
```

The three probes use fresh, separate target directories, an empty compiler
wrapper, pinned dependencies and the shipped release settings (`s`, fat LTO,
one codegen unit, stripped symbols, serde_json at 3), including rebuilt std with
`panic-unwind,optimize_for_size`. Each executable reads files from argv and
prints the normalized fields, preventing parser elimination. Compile times
include rebuilding std and proc macros, with downloads/index/cache effects;
the machine was shared and the runs were sequential, so these are observations,
not quiet-machine performance budgets. No startup or budget timings were run.

| Parser | Executable bytes | Delta over custom | Fresh build wall seconds | Corpus |
| --- | ---: | ---: | ---: | ---: |
| Custom bounded subset | 153216 | +0 | 207.25 | 17/17 |
| serde_yaml_ng 0.10.0 | 219520 | +66304 | 237.85 | 17/17 |
| yaml-rust2 0.13.0 | 236048 | +82832 | 196.47 | 17/17 |

`mise run size`, before: **3844240 bytes (3754 KiB)**; final: **3794544 bytes (3705 KiB)**. The integrated binary saves **49696 bytes** and remains below 4096 KiB. Main was already merged (`git merge main`: already up to date). A final parser-only rebuild after diagnostic corrections still measured **153216 bytes**, with all 17 headers matching (93.81 s incremental rebuild, separate from the fresh-build table). The earlier intermediate integrated build measured 3689 KiB; the final source and full link above are the reported result.

Choose the custom subset parser: it passes the complete sourced corpus and
retains diagnostics, validation and bounds at the lowest measured shipped-profile
size. `serde_yaml_ng` 0.10.0 and `yaml-rust2` 0.13.0 (default features disabled)
remain reproducible comparison candidates, not shipped or dev dependencies.
The maintained alternatives are linked above through their primary source
package pages: [serde_yaml_ng](https://crates.io/crates/serde_yaml_ng) and
[yaml-rust2](https://crates.io/crates/yaml-rust2).

`mise run fuzz skill_frontmatter --time 10s` exercises arbitrary UTF-8 and binary
input through both delimiter extraction and header parsing using the repository's
Bolero/libFuzzer setup. The real headers seed the generated corpus under
`target/fuzz/skill_frontmatter/corpus`; ordinary tests also run Bolero's random
input engine. The measured coverage run completed 11267 executions in 11 seconds
(the requested fuzz duration was 10 seconds), without a crash. Discovery, loading
and publication semantics are unchanged.

Validation: `mise run check` (1148 passed, 30 skipped), `acceptance` (23 passed),
`machete`, `deny`, `check:no-tokio`, the coverage run above, and `size` before
and after. All Cargo invocations used two build jobs. No budgets or startup
timings were run.

# Historical flat-parser probe

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

The earlier choice was [serde_yaml_ng](https://crates.io/crates/serde_yaml_ng): the maintained
Serde parser is smaller than the measured tree adapter, supports ordinary
portable YAML skill headers, and fits the 4 MiB product budget. The saved 97 KiB
of a tiny parser does not justify maintaining a second quoting and block-scalar
language. `serde_yaml` 0.9 is deprecated and was excluded rather than added as a
production dependency. `yaml-rust2` is the measured alternate maintained parser.
At that point no handwritten YAML parser was shipped. Frontmatter has an 8 KiB limit and a
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
Containment reuses `rustix`'s existing descriptor-relative filesystem operations
and the Windows handle path query, adding no dependency or parser change.

After the independent-review fixes and the protocol 4 / flush-epoch main merge,
`mise run size` measures **3155 KiB**, against **3139 KiB** immediately before
those fixes (+16 KiB). Publication text and identity now occupy one sealed entry;
edit-aware projections and bounded regular-file discovery share general seams.

After merging fork's protocol 7 work and integrating recorded skill catalogs,
protocol **8** measures **3203 KiB** (`mise run size`, `CARGO_BUILD_JOBS=3`),
against the prior validated **3155 KiB** (+48 KiB, including main's fork work).
Catalog images add no dependency and preserve the historical generation without
placing instruction bodies in model context. Startup and budgets remain for
the coordinator's quiet-machine run.

The side-call merge on protocol **9** measures **3236 KiB** (`mise run size`,
`CARGO_BUILD_JOBS=3`). The side branch's last validated build was **3072 KiB**;
this +164 KiB includes the skills implementation and its dependencies from
main. Against main's **3203 KiB** above, the combined build adds 33 KiB.
The combined compaction/fork regression links `ext-skills` only as an engine
dev-dependency; it adds no shipped dependency beyond main's skills feature.
