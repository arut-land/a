# Images that `read` sends

`read` (in `ext-edit`) recognizes enabled formats by magic bytes and a
valid header. Upright PNG, JPEG and WebP pass through as their own bytes
when at most 2000 px on the longest edge and 3,750,000 bytes (5 MB as
base64). Header inspection does not decode pixels. PNG metadata is scanned
through the end of the file so a trailing eXIf orientation is read too;
rotated PNGs are converted instead of passed through. GIF is decoded to its first frame. BMP and larger
images are decoded too, scaled by area sampling and encoded as PNG when
they have alpha and the encoded PNG fits, otherwise JPEG at quality 85.
GIF frames normally take the PNG path. EXIF orientation is applied before sending;
a non-upright JPEG is always converted, even when small enough to pass
through. Image reads ignore text offsets and limits and say so in the result.

Text past 8 MiB needs a line window and is streamed to that window; a
zero-line window returns no text. Lines outside the window are skipped
with buffered chunks, so their length does not allocate memory; only the
selected text counts against the output cap. Image files have a separate 512 MiB
limit, matching the decoder allocation limit rather than a worker pool's
concurrency budget.

## Crates and release size

`image` 0.25 enables only `png`, `jpeg`, `gif`, `webp` and `bmp` (no rayon,
TIFF or AVIF/EXR). Its maintained Rust decoders provide format coverage;
`jpeg-decoder` 0.3 with default features off adds DCT-scaled JPEG decoding.
`png` is also used directly for row streaming, at the same version `image`
uses, so there is one PNG decoder in the dependency tree. `base64` was
already in the shipped dependency tree through `ureq`.

Measured 2026-10-26 on macOS arm64, nightly-2026-10-17, with
`cargo build --release -p a` and the repository's release profile. All
builds ran from the multimodal worktree with scratchpad target directories;
comparison sources were detached scratchpad worktrees. This compares the
ordinary release profile, without the shipped build's `build-release`
rebuild of std.

| Build | Bytes | Delta from main |
| --- | ---: | ---: |
| main (`0c1b9773`) | 5,607,936 | |
| multimodal (`2cc6f727`), without BMP | 6,239,872 | +631,936 (+11.27%) |
| multimodal (`2cc6f727`), with BMP | 6,256,624 | +648,688 (+11.57%) |

BMP adds 16,752 bytes (16.36 KiB, 0.27% of the
binary without it), so it stays enabled. TIFF is disabled. The total branch
delta includes the multimodal codecs, blob projection, read conversion and
JPEG scaling; it is not solely the image dependency's cost.

## Time and memory

Release build, M-series Mac, through the extension rig (file read, header,
conversion, blob write). Earlier whole-decode measurements:

| File | Result | Time |
| --- | --- | ---: |
| PNG 1600x1000 | its own bytes | 0.6 ms |
| BMP 1600x1000 (4.8 MB) | JPEG 1600x1000, 145 KB | 65 ms |
| JPEG 4000x3000 (1.1 MB), whole decode | JPEG 2000x1500, 497 KB | 281 ms |
| PNG 5120x2880 RGBA (a 5K screenshot) | PNG 2000x1125 | 156 ms |

Area sampling (`thumbnail`) took the 12 MP JPEG from 525 ms with a triangle
filter to 281 ms. DCT scaling then addresses the remaining decode cost:

| 24 MP JPEG decode | Time | Decoded pixels held |
| --- | ---: | ---: |
| Whole decode | 380 ms | 72 MB |
| `jpeg-decoder` DCT scaling | 171 ms | 18 MB |

JPEG decoding selects the smallest DCT scale still large enough for the
output; unsupported pixel formats fall back to whole decoding. A
non-interlaced PNG is read row by row and folded into the scaled image,
holding the input, one row and the scaled result instead of a whole 59 MB
RGBA buffer for a 5K screenshot.

Conversion runs on the runtime's CPU workers. Its reservation accounts for
the file, decoded pixels (or the scaled image if larger), and encoded
result, capped at `rt::CPU_BUDGET`. Work larger than the pool budget
reserves the whole budget and runs alone; the budget controls concurrency
and does not refuse large images. Storage reservations follow the same
rule with `rt::STORAGE_BUDGET`.

## Retained image admission

Measured 2026-10-27 on the shared M3, macOS 27.0.1 arm64, with
nightly-2026-10-17. Before is `1552a41f`; after is the source committed as
`0e0ee358`, before merging unrelated main changes. Both binaries used the
same shipped release profile with the echo fixture enabled:

```sh
CARGO_BUILD_JOBS=2 CARGO_TARGET_DIR=<external-target> mise exec -- cargo build-release -p a --features fixtures
```

Each sample starts a fresh daemon under an isolated `A_HOME`, configured
with the echo agent and automatic tool policy. After three seconds settling,
1, 4 or 8 synchronized CLI clients each run `call read` on a 4000×3000,
36,000,054-byte, uncompressed 24-bit BMP. Every row repeats the same BGR
pattern: `x % 256`, `(x / 16) % 256`, `(7 * x) % 256`, with integer division.
Every result must succeed and hold exactly one image blob. The daemon is
stopped through its isolated home after each sample.

Each concurrency has ten separate before/after pairs. Pair order alternates
before/after and after/before; concurrency order alternates 1/4/8 and 8/4/1.
The first five pairs had noisy timing shifts, so five more were collected;
all ten pairs are included. Concurrency 1 and 4 use the same measurement
procedure and sample count as 8.

Peak RSS is the maximum native daemon RSS sampled every 10 ms. Throughput
is completed reads divided by the longest client duration in that wave.
Slowest-call latency is that longest duration, from immediately after the
launch barrier through CLI startup, read, conversion, blob storage and
result delivery. CPU is the daemon's user-plus-system counter delta across
the wave. These latency samples describe finite waves, with up to eight
calls per wave; they do not estimate a long-run p99.

Values are median (minimum–maximum), ten samples per binary and concurrency.

| Concurrent reads | Before peak RSS, KiB | After peak RSS, KiB |
| ---: | ---: | ---: |
| 1 | 100,432 (100,288–100,480) | 100,224 (100,160–100,320) |
| 4 | 207,080 (206,912–207,904) | 101,256 (101,040–101,440) |
| 8 | 348,840 (348,560–349,744) | 102,408 (102,144–103,152) |

| Concurrent reads | Before throughput, reads/s | After throughput, reads/s |
| ---: | ---: | ---: |
| 1 | 1.705 (0.957–1.794) | 1.687 (0.790–1.805) |
| 4 | 1.805 (1.196–1.862) | 1.763 (1.353–1.868) |
| 8 | 1.798 (1.346–1.885) | 1.793 (1.393–1.894) |

| Concurrent reads | Before slowest-call latency, s | After slowest-call latency, s |
| ---: | ---: | ---: |
| 1 | 0.587 (0.557–1.045) | 0.593 (0.554–1.266) |
| 4 | 2.217 (2.149–3.345) | 2.270 (2.141–2.957) |
| 8 | 4.450 (4.243–5.942) | 4.463 (4.224–5.741) |

| Concurrent reads | Before daemon CPU, s | After daemon CPU, s |
| ---: | ---: | ---: |
| 1 | 0.552 (0.532–0.952) | 0.546 (0.527–1.139) |
| 4 | 2.163 (2.136–2.888) | 2.237 (2.096–2.864) |
| 8 | 4.397 (4.252–5.854) | 4.384 (4.168–5.586) |

The paired changes below are computed within each round as
`100 * (after / before - 1)`, then summarized as median (minimum–maximum).
They differ from ratios of the aggregate medians above. Positive throughput
means faster; positive latency, CPU or RSS means more.

| Concurrent reads | RSS change | Throughput change | Slowest-call change | CPU change |
| ---: | ---: | ---: | ---: | ---: |
| 1 | -0.18% (-0.32–+0.03%) | +0.14% (-17.39–+8.43%) | -0.14% (-7.78–+21.05%) | -1.81% (-11.63–+19.57%) |
| 4 | -51.11% (-51.33–-50.99%) | -0.86% (-23.32–+22.59%) | +0.87% (-18.43–+30.42%) | -1.38% (-7.15–+31.77%) |
| 8 | -70.64% (-70.74–-70.51%) | -0.34% (-7.38–+8.87%) | +0.35% (-8.15–+7.96%) | -1.40% (-8.73–+5.46%) |

At eight concurrent reads, median peak RSS falls by 246,432 KiB
(240.7 MiB), approximately 70.6%; at four it falls approximately 51.1%.
Single-read memory stays essentially unchanged. Aggregate slowest-call
medians rise 1.1%, 2.4% and 0.3% at 1/4/8 reads; paired median changes are
−0.14%, +0.87% and +0.35%. All paired timing ranges span both directions.
The shared-host measurements establish the memory reduction; they do not
establish a reproducible latency regression, prove a timing improvement or
exclude a small cost. The four-read aggregate CPU median rises from 2.163
to 2.237 seconds while its paired median change is −1.38%; both summaries
are retained because host load varies between samples.

Before the fix, storage returned whole image inputs before CPU admission,
so waiting CPU jobs already held their files. The registered read tool now
shares a 64 MiB retained-data budget. It acquires credit before reading the
whole file, reserving the larger of the input size and maximum prepared
output size. The credit follows the input and prepared output through
conversion and blob storage, including accepted workers whose caller has
cancelled. Waiting calls hold paths and metadata; cancelling a waiter
unregisters it without returning credit it never acquired.

Storage and CPU execution remain separately admitted. An image larger than
the retained-data budget reserves all its credit and runs alone. The budget
controls concurrent retention, and total process memory can exceed it during
conversion or oversized work. Supported formats and existing file limits
are unchanged. A real 5000×5000 BMP above 64 MiB completes in the oversized
resource test. The eight-read live-heap regression test fails before the fix
at 324,024,171 bytes and passes afterward at 72,023,218 bytes.

The fixture-enabled measurement binary grew from 6,756,272 to 6,772,800
bytes (+16,528 bytes, 0.245%). These sizes do not measure the shipped binary
without fixtures. Performance here was measured on M3/macOS; native Linux
and Windows timing and mixed-image workloads were not measured in this
campaign.
