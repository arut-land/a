# rig fixtures

Recorded provider replies from [rig](https://github.com/0xPlaygrounds/rig)
v0.44.0, and what rig itself decodes from them. `crates/llm` must decode the
same cassettes to the same results (`tests/parity.rs`).

- `cassettes/`: rig's recordings, verbatim (`crates/rig-cassette/fixtures/cassettes`),
  limited to streamed completions on the wires `crates/llm` speaks.
- `expected/`: per cassette, the normalized result of each exchange as rig
  0.44 decodes it, written by `oracle/`.

Regenerate both with `mise run fixtures:rig`. Never edit them by hand: a
mismatch is a decoder bug to fix, or a deliberate divergence to record in
the parity test.
