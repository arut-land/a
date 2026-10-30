# Elicitation destination dependencies

Measured on macOS arm64, 2026-10-30, before adding production dependencies.
A standalone executable consumes a runtime URL argument and prints its decoded
host; the Unicode variant also uses mixed-script and potential-confusable
checks. All variants use the same Cargo target directory, release profile,
fat LTO, one codegen unit and stripped output. Two interleaved sequences were
built: Unicode / IDN / baseline, then Unicode / IDN / baseline. No timing claim
is made; these are deterministic file sizes, not application latency samples.

| Variant | Median bytes [min–max], n=2 |
| --- | --- |
| Baseline: argument and print | 286,272 [286,272–286,272] |
| `url` 2.5.8 + `idna` 1.1.0 | 468,528 [468,528–468,528] |
| Above + `unicode-security` 0.1.2 | 501,568 [501,568–501,568] |

The Unicode checks add 33,040 bytes over URL/IDN handling in this probe.
`url` and `idna` already exist in the workspace dependency graph. IDNA supplies
Punycode conversion and normalization, but not UTS #39 script/confusable checks.
`unicode-security` adds maintained Unicode tables and `unicode-script`,
`unicode-normalization` and `tinyvec` (four new packages in total). This is preferable to maintaining Unicode tables or
a partial handwritten detector. Punycode itself also receives a warning, as MCP
recommends; the Unicode checks cover Unicode hosts after URL normalization.

These are standalone dependency measurements, not the incremental size of `a`.
The probe resolved ICU 2.4 while the workspace pins ICU 2.3, and shared code in
`a` may change linker retention. A shipped-size extrapolation would be a
projection; none is claimed here. Raw build logs, source, lockfile, executables
and `size-results.json` are retained under the task's scratch evidence directory
`r/elicit-safety` (not committed build output).

APIs: [IDNA conversion](https://docs.rs/idna/1.1.0/idna/fn.domain_to_unicode.html),
[Unicode security](https://docs.rs/unicode-security/0.1.2/unicode_security/).
