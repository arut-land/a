# Persistent compiler state

Proposal #2 needs cheap forks of both settled byte-fragment references and
origin-preparation ID maps. The prototype's byte runs were linked, but its ID
vectors still copied all preceding references. Keeping flat vectors there would
leave a linear fork cost and quadratic residency for retained child checkpoints.

Use `rpds` 1.2.1's thread-safe persistent vector and red-black maps/sets. This
replaces the bespoke linked byte graph and its destruction/iteration code and
shares prepared ID state as well. The engine uses the same persistent map for
unresolved input metadata: checkpointing a pending input shares its immutable
value, and only consuming a still-shared value copies that requested input.
A real approximately 5 MiB pending-input invariant failed with the prototype's
deep snapshot copy and passes with this ownership change. The [crate API](https://docs.rs/rpds/1.2.1/rpds/)
provides these persistent structures with structural sharing. No definition or
client-history interner is introduced.

Before adding the dependency, an isolated release microbenchmark compared real
standard vectors with the maintained structures. Same M3, `cargo build-release`,
opt-level z/fat LTO, one target, two build jobs. Three fresh processes per arm
and size, interleaving arms and reversing scale order. Each source has N entries;
64 retained forks add four private IDs or one private byte run. IDs use shared
string values; byte runs share a 32-byte payload, so this isolates reference and
index ownership rather than repeated large byte buffers. The timing is wall
microseconds per clone-and-append, not daemon CPU. Native live malloc is captured
after all 64 forks, outside that timing, and includes process/root overhead.

Values are median [minimum–maximum], n=3. This is a primitive-selection
measurement, not acceptance of the real daemon workload.

| Entries | Structure | Fork µs | Live malloc MiB |
| --- | --- | --- | --- |
| 256 | flat | 2.683 [2.608–3.162] | 1.556 [1.556–1.556] |
| 256 | persistent | 2.380 [2.373–2.394] | 0.216 [0.216–0.216] |
| 256 | chunks-flat | 1.156 [1.107–1.257] | 0.530 [0.530–0.530] |
| 256 | chunks-persistent | 0.266 [0.242–0.272] | 0.054 [0.054–0.054] |
| 4096 | flat | 33.232 [32.798–34.539] | 24.408 [24.408–24.408] |
| 4096 | persistent | 3.277 [3.108–3.440] | 1.185 [1.185–1.185] |
| 4096 | chunks-flat | 12.347 [12.284–12.711] | 8.089 [8.089–8.089] |
| 4096 | chunks-persistent | 0.259 [0.259–0.276] | 0.208 [0.208–0.208] |
| 32768 | flat | 246.934 [246.205–251.078] | 195.033 [195.033–195.033] |
| 32768 | persistent | 3.972 [3.838–4.287] | 8.208 [8.208–8.208] |
| 32768 | chunks-flat | 93.656 [93.320–95.078] | 64.526 [64.526–64.526] |
| 32768 | chunks-persistent | 0.205 [0.205–0.207] | 1.354 [1.354–1.354] |

At 32,768 IDs, persistent fork-and-append is about 62× faster and the retained
64-fork process has about 24× less live malloc. For byte references the vector
fork cost drops from about 94 µs to 0.2 µs. The byte-vector baseline is the old
flat representation; this does not claim it was the already-linked prototype.

The scratch lock resolves rpds → archery + smallvec, with archery → triomphe.
Smallvec is already shipped; the other three packages are new. Replacing the
linked prototype and flat ID/pending-input maps added 16,768 bytes to the
release CLI, measured with the same profile. This delta includes the new
ownership implementation, rather than isolating library text alone. The
end-to-end acceptance matrix is separate. Raw primitive samples include RSS
and physical footprint as well.

Source, raw native captures, dependency lock and hashes are retained under
`/private/tmp/claude-502/-Users-rajaniraiyn-work-a/a536a1f2-a8c3-4ba1-8862-89a02f8c91c0/scratchpad/r3/context-graph/persistent-probe`
and `persistent-matrix`, with the runner `persistent-bench.py` beside them.
