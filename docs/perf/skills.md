# Skill frontmatter parser

Skills use `yaml-rust2` 0.13 with default features disabled. A pull-event adapter
validates one mapping without constructing a YAML document tree. The loader
limits files to 64 KiB; the parser admits at most 8192 header bytes and 16 node
levels, rejects anchors, aliases and tags before expanding anything, and rejects
duplicate scalar keys in every mapping. Required name/description fields must
be text; unknown metadata is syntax-checked and discarded. YAML parsing has no
background tasks or idle wakes. The retained catalog contains only the same
name, normalized description, directory and body as before.

The 17 verbatim headers in `crates/ext-skills/tests/corpus` have commit-pinned
sources in `sources.json`. `expected.json` is the normalized field oracle.
Parser changes never rewrite either file. Tests also exercise quoted and
multiline scalars, YAML escapes, malformed metadata, nulls, duplicate keys,
resource limits, file-line diagnostics and arbitrary UTF-8 via Bolero.

## Candidate comparison

| Candidate | Reliability and resource behavior | Dependency cost | Decision |
| --- | --- | --- | --- |
| Custom bounded subset | Bounded input/depth, no alias expansion; maintains its own escaping, indentation, flow and block syntax, and rejects valid portable YAML | std and existing serde_json | Remove from production; retain only the exact measurement baseline |
| yaml-rust2 0.13, events | Maintained pure Rust YAML syntax; immediate typed rejection at depth/alias/tag boundary; no whole-document tree; unknown values freed as consumed | arraydeque, hashlink, hashbrown, foldhash; UTF-8 transcoding disabled | Adopt; application validation remains in its owning crate |
| yaml-rust2 0.13, tree | Same syntax engine; allocates and retains the entire document before field extraction; cannot stop the loader at the application's depth boundary | Same parser graph | Measure as a reference; event interface better fits bounded metadata |
| serde_yaml_ng 0.10 | Direct typed deserialization is convenient, but ignored metadata does not supply the same explicit event-level alias/tag/duplicate/depth rejection; uses unsafe-libyaml (test-only advisory exception in deny.toml) | serde, indexmap, itoa, ryu, unsafe-libyaml | Do not promote the existing test dependency into production |
| saphyr 0.2.2 | Maintained pure Rust YAML alternative; document loader retains unknown metadata and resolves aliases; parser-only use could support the same adapter | Separate parser/tree dependencies; captured by hosted cargo tree | Compare as a maintained alternative; no demonstrated reliability advantage for this adapter |

CPU and memory are measured rather than inferred from executable bytes. The
hosted `crate comparison` workflow builds all five probes using the shipped
release profile and rebuilt std. It checks every result against the recorded
oracle, then runs five batches of 170,000 header decodes per candidate. The JSON
artifact records ns/header, sampled peak process RSS, exact executable bytes
and enabled normal dependency trees. RSS includes code/allocator/runtime pages;
5 ms sampling can miss a short peak. It is not live allocation accounting.
The custom probe is a test-only copy, not a supported parser implementation.

Reproduce on a hosted runner with Python 3.13 and psutil 7.2.2:

```sh
python crates/ext-skills/tests/measure/run.py
```

Full binary size and paired daemon startup, attach, idle and loaded measurements
come from `perf` with `paired_ref` pinned to the main revision. Results are
reported per native target; no local release builds or timing runs are used.

Sources: [yaml-rust2 event API](https://docs.rs/yaml-rust2/0.13.0/yaml_rust2/parser/struct.Parser.html),
[saphyr](https://docs.rs/saphyr/0.2.2/saphyr/),
[serde_yaml_ng](https://docs.rs/serde_yaml_ng/0.10.0/serde_yaml_ng/).
