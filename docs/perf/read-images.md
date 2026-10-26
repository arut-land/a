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
