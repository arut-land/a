# Native build tuning

Hosted measurements and reproduction commands are in [hosted.md](hosted.md).
Per-target decisions and paired CPU/RSS observations are in [hosted-tuning.md](hosted-tuning.md).
Binary size is reported on every target, without a cap. Decisions prioritize
CPU, latency, idle and peak RSS, reliability and native platform behaviour.

## Release design

| Setting | Design constraint |
| --- | --- |
| Release level z | Main profile; package overrides require CPU wins beyond spread on most targets with idle and peak RSS within noise |
| Fat LTO and one codegen unit | Cross-crate optimization; candidates must improve measured CPU and latency without an unacceptable memory cost |
| Panic unwinding | A panicking session actor cannot terminate the daemon |
| Packed split debuginfo and stripped executable symbols | Preserve offline crash symbolication without mapping debug data into the process |
| Rebuilt standard library | The `build-release` alias controls the same std features for shipped, fixture and benchmark builds |
| Linux GNU: clang, mold, safe ICF, RELR | Preserve function address identity; use LLVM address-significance metadata only in release builds |
| Windows MSVC: static CRT, 8 MiB stack | Standalone runtime deployment and sufficient stack for nested decode paths |
| macOS: synchronous unwind tables | Retain panic unwinding while allowing compact native unwind records |
| Native TLS and platform roots | Delegate cryptography and trust policy to the host |

Linux GNU needs OpenSSL 3 (`libssl.so.3`, `libcrypto.so.3`) and a RELR-capable
loader, glibc 2.36 or newer. Building needs clang, pkg-config and libssl-dev.
Safe ICF preserves address identity; unrestricted folding is not enabled.
Windows uses the native MSVC ABI on both architectures. Apple silicon uses
the toolchain's Apple CPU baseline; x86 targets retain the conservative
toolchain baseline unless a platform-specific comparison justifies raising it.

`sys::small_footprint` sets glibc's `M_ARENA_MAX` to two before workers exist.
This bounds mostly empty arenas without removing per-thread caches. The
system allocator remains native on other platforms. Replacement allocators
must demonstrate a joint CPU/RSS benefit before becoming dependencies.

## Measurement boundaries

| Axis | Measurement |
| --- | --- |
| Shipped and fixture size | Exact file bytes; shipped binary scanned for fixture exclusion |
| Process startup | Hyperfine against a native empty Rust process floor |
| Cold first reply and replay attach p50/p99 | Five fresh daemon homes; native protocol probe |
| Idle daemon RSS | Native resident set or working set after three seconds |
| Idle daemon and line-client CPU | Native user+kernel counter deltas after fifteen seconds settling; ten-second idle window |
| Driver, JSON, reload, concurrent commits, TUI | Saved release benchmark executables; process wall time, CPU and peak RSS |

Builds finish before measurement. Small gains require alternating same-runner
reference/candidate rounds; separate VMs can differ in CPU and storage. Raw
samples and hardware metadata accompany medians and spreads. Size and dependency growth must be proportionate to the measured benefit.

Development builds use Cranelift for workspace code and LLVM for dependencies.
Dependencies default to opt-level zero for fast builds; serde_json, vte,
unicode-width, rustix and memchr use level one. The mbx compiler store is
shared through CI caches. Intel macOS uses Cargo artifact caching because the
pinned mbx version has no native Intel macOS executable.
