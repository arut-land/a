# HTTP transport (2026-10-26)

`transport::Client` is the HTTP client behind every provider and MCP call.
Until this change it was ureq, run on `rt`'s network blocking lane: a
streamed reply held one of the lane's eight workers until its last byte, so
at 128 concurrent sessions turns queued (ubuntu-latest turn p50 3945 ms
against 532 ms with 128 network workers, [runtime-demand.md](runtime-demand.md)).
The owner's direction: no network call holds a thread, regular async Rust
over the `rt` seam, lighter than tokio, modern protocols where they pay.

The client is now HTTP/1.1 from `ureq-proto` (ureq's own sans-IO
protocol) driven over sockets registered with the `rt` reactor
(`rt::Async<TcpStream>`), with native TLS or rustls run over the
non-blocking socket. A request and its reply body are futures polled by
the task that awaits them; no thread is held. A dropped body at a chunk
boundary gets one background drain for its terminator, bounded by the idle
deadline; another data byte closes it.

## Candidates

| Candidate | Runs on rt without tokio | Dependencies | Verdict |
| --- | --- | --- | --- |
| hyper 1.x, own executor and IO adapters | No: `hyper` depends on `tokio` (sync) unconditionally, `h2` on `tokio` and `tokio-util` | +tokio, tokio-util, h2, want, httparse, http-body… | Excluded by `check:no-tokio` and `deny.toml` |
| async-h1 2.3.4 / http-client / surf | Yes (futures-io) | 129 crates; `async-io` 1 and 2, `async-channel`, `async-lock`, `event-listener`, `bitflags` in two versions each | Fails `multiple-versions = "deny"`; http-types is unmaintained |
| isahc | No: libcurl's multi loop on a thread of its own | curl-sys, a second event loop | Excluded: a second reactor, a C dependency |
| Hand-rolled HTTP/1.1 over `httparse` | Yes | httparse only | Reimplements what ureq-proto already tests: chunked bodies, close delimiting, 1xx, redirects |
| **ureq-proto 0.6 over `rt::Async`** | **Yes: sans-IO** | **Already linked by ureq: base64, http, httparse, log** | **Chosen** |

ureq-proto is the protocol ureq ran for us before, maintained by the same
author, typestate (`Call<SendRequest>`, `Call<RecvBody>`, …) so a wrong
transition does not compile. Taking it directly removes ureq, `der`,
`base64ct`, `pem-rfc7468` and `utf8-zero` from the lockfile and adds no
crate: `native-tls`, `rustls`, `rustls-platform-verifier` and
`futures-lite` were already in the tree.

TLS calls read the non-blocking socket and write to an owned output
buffer. Ciphertext drains in 16 KiB slices between calls, including each
handshake step. The TLS library never sees a refused write: this matters
on Security.framework, which queues a refused write and reports it sent.
A read would-block waits on the reactor before retrying.
`futures-rustls` would cover rustls only, and the binary ships native TLS.

## Parity with ureq

What ureq gave the daemon, and what the client does now:

| | ureq | `transport::Client` |
| --- | --- | --- |
| Pool | 10 idle, 3 per host, 15 s | Same limits; expiry checked on use, never on a timer |
| Stale pooled socket | probe before reuse | Same: a non-blocking peek |
| Connect timeout | 30 s over the addresses | 30 s overall for DNS, TCP, proxy and TLS; Happy Eyeballs interleaves families and staggers attempts by 250 ms |
| Reply head timeout | 300 s | 300 s without progress |
| Body timeouts | none | 300 s idle between reads and individual socket writes, reset on progress; no total request duration limit |
| TCP keep-alive | platform defaults | Kernel probes: 60 s idle, 10 s interval, 5 probes, with native socket options on each OS |
| Redirects | 10, auth headers dropped | 301/302/303 become GET (HEAD stays); 307/308 preserve the method but body resends are refused; Authorization, Proxy-Authorization, Cookie and body headers dropped; x-api-key, x-goog-api-key and api-key also dropped when the host changes |
| Proxy | Environment, NO_PROXY, CONNECT | http_proxy for HTTP (uppercase HTTP_PROXY ignored); https_proxy before HTTPS_PROXY for HTTPS; scheme-specific before all_proxy / ALL_PROXY; NO_PROXY before no_proxy; bypass checked before reporting an unusable proxy |
| Proxy schemes | HTTP CONNECT | HTTP CONNECT; an HTTPS or SOCKS proxy fails the requests routed through it, while bypassed requests still work |
| Response compression | off | Advertises gzip, br, zstd and decodes incrementally, including SSE; removes Content-Encoding and Content-Length after decoding |
| Request compression | off | Provider endpoint capability; gzip/zstd available, enabled only for documented OpenAI Responses zstd uploads |
| Cookies | off | No cookie jar |
| Cancellation | socket shutdown from dropping task | Dropping the future or partial body closes it; at a yielded chunk boundary, a terminator-only drain returns it to the pool or closes by IDLE |
| DNS | getaddrinfo on request's thread | rt::resolve: getaddrinfo on the network lane, only for a name and a new connection |

The response head remains bounded at 64 KiB and the CONNECT response at
16 KiB. Idle pool limits constrain retained connections, never active
requests. Request header lines grow the send buffer beyond its initial
16 KiB; a test sends a 32 KiB URL and a 128 KiB header. The `http::Uri`
constructor itself rejects URLs above its own 64 KiB representation limit.
Bodies have no transport size limit, compressed or decoded. Codecs retain
their format's streaming history (zstd's default maximum window is 128 MiB),
not the whole response. A 16 MiB slow upload test exceeds the test idle
deadline in total while continuing to make progress.

## Compression

No compression implementation was already linked in the client's normal
dependency tree. Candidates were checked before selecting the backend:

| Candidate | Additional stripped release probe size | Build and dependency result |
| --- | ---: | --- |
| async-compression, gzip/br/zstd (libzstd) | 486 KiB | 16 new lockfile packages; Windows cross-clippy fails compiling C without a Windows C SDK |
| **async-compression gzip/br + mzstd** | **860 KiB** | **Pure Rust, no C SDK or second runtime; workspace Windows cross-clippy passes** |
| ruzstd | Not built | Decode only; still needs a separate encoder for documented request compression |

Isolated copies of the final transport source, with only the compression
backend changed, built the identical probe at 523,584 bytes without
compression, 1,021,488 with
libzstd and 1,404,528 with mzstd (macOS arm64, release, opt-level z, LTO,
one codegen unit, stripped). These are probe deltas, not full daemon sizes.
The selected backend adds 17 lockfile entries (16 normal/build dependency
entries, plus the optional zlib-rs resolution). Existing futures-io,
thiserror, memchr and the runtime are reused. The
maintained codecs own gzip/Brotli framing and zstd sessions. The local
zstd adapter only drives input/output progress over AsyncRead: its upstream
async-compression adapter is currently behind an unstable cfg. No codec,
checksum or compression format is reimplemented here. This costs more
code than C zstd but preserves the workspace's SDK-free Windows check.

Requests carry an optional `transport::Compression` extension. A host's
capability defaults to None; only a documented endpoint sets it. Custom
base URLs clear the built-in capability. Payload pieces feed the encoder
without concatenating the conversation; output stays in pieces with a
known wire Content-Length. Compression yields between output pieces and
is selected only when smaller. OpenAI documents a 128 MiB compressed and
decoded limit and a 100:1 maximum ratio for zstd: outside that envelope the
client selects the original payload, without imposing a new request size
error. Gzip has no provider enabled by default.

Provider audit, 2026-10-26: “off” means no explicit request-body coding
guarantee was found in the cited official API documentation; it does not
claim the server rejects compressed requests. Response negotiation is
independent and enabled for every host.

| Provider / endpoint | Upload default | Official source / finding |
| --- | --- | --- |
| OpenAI Responses | zstd, level 3 | [Production best practices](https://developers.openai.com/api/docs/guides/production-best-practices), “Compress request bodies”, explicitly documents POST /v1/responses and the envelope above |
| OpenAI Chat Completions | off | The documented guarantee above names Responses only |
| Anthropic Messages | off | [Create a Message](https://platform.claude.com/docs/en/api/messages/create) does not document gzip/zstd uploads; Claude Code gzip reports do not establish an API contract |
| Gemini generateContent / streamGenerateContent | off | [Generating content](https://ai.google.dev/api/generate-content) documents JSON requests, no request content coding guarantee |
| Bedrock InvokeModel | off | [InvokeModel](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_InvokeModel.html) specifies JSON content type, no request coding contract; not a built-in host in llm |
| OpenRouter | off | [API reference](https://openrouter.ai/docs/api-reference/overview); context compression changes messages, not HTTP content coding |
| xAI | off | [Responses API](https://docs.x.ai/developers/rest-api-reference/inference/responses), no documented gzip/zstd upload guarantee |
| DeepSeek | off | [API introduction](https://api-docs.deepseek.com/), OpenAI JSON compatibility does not establish zstd request support |
| Ollama | off | [Chat API](https://docs.ollama.com/api/chat), no upload coding guarantee |
| Groq, Mistral, other compatible hosts | off | [Groq OpenAI compatibility](https://console.groq.com/docs/openai), [Mistral API](https://docs.mistral.ai/api/); compatibility alone does not opt a host into compression |

Apple M3 upload probe, same release profile, three runs per row. Synthetic
conversation JSON alternates user/assistant messages containing sampled
repository text and unique 128-bit log identifiers; raw sizes are 1,076,884
and 8,607,049 bytes. A separate loopback server reads at a nominal 10 Mbit/s
(16 KiB reads plus a rate sleep) and replies when the body ends. Scheduler
and TCP effects make achieved throughput lower. Latency is request-to-head;
CPU is the entire client process, including startup and reading the payload.
These are upload savings, not model inference speedups.

| Conversation | Coding | Wire bytes | Client CPU (ms) | Request-to-head (ms) |
| --- | --- | ---: | ---: | ---: |
| 1 MiB | identity | 1,076,884 | 4.56 | 1124.89 |
| 1 MiB | gzip | 490,893 | 26.03 | 546.92 |
| 1 MiB | zstd | 325,487 | 9.84 | 353.50 |
| 8 MiB | identity | 8,607,049 | 7.37 | 9801.62 |
| 8 MiB | gzip | 3,907,034 | 177.37 | 4769.45 |
| 8 MiB | zstd | 2,491,638 | 74.49 | 2922.00 |

Zstd saves 70–71% of upload bytes here, with 5–67 ms additional process CPU;
gzip saves 54–55%, with 21–170 ms additional CPU. Faster links reduce the
latency benefit; no provider/model latency is inferred from this probe.
 Tests gate the second SSE event
and compression footer until the first decoded event reaches the caller,
for gzip, Brotli and zstd; truncated streams report errors. Upload tests
round-trip every piece over a real socket and verify the wire coding and
Content-Length. The TLS slow-reader test trusts a self-signed RSA certificate
with a 397-day validity (expires 2027-11-26); macOS rejects the former
100-year certificate. Regenerate it before expiry with the existing key:

```sh
openssl req -x509 -new -key crates/transport/src/client/testdata/localhost.key \
  -out crates/transport/src/client/testdata/localhost.pem -days 397 \
  -subj /CN=localhost -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1' \
  -addext 'extendedKeyUsage=serverAuth' -addext 'basicConstraints=critical,CA:FALSE'
```

## Measurements

Apple M3, macOS 27.0, release builds of a probe linking `transport` from
main (`a43606cd`, ureq) and from this branch, `tls-native`, on
`rt::Threads` with the daemon's configuration (four async workers, the
default network lane). The server is a separate process: HTTP/1.1
keep-alive, 30 SSE events 10 ms apart per reply, 8 KiB request body. Each
row is three turns of N concurrent streams, median of three runs.
Runs alternate ureq and async against the same server. First event and
reply latencies are from the request; CPU is the client process over the
three turns; RSS and threads are the client's after the turns.

| Streams | Impl | First event p50 / p99 (ms) | Reply p50 / p99 (ms) | CPU (ms) | RSS (MiB) | Threads |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | ureq | 15.1 / 15.1 | 426 / 426 | 5 | 6.3 | 6 |
| 1 | async | 15.2 / 15.2 | 425 / 425 | 8 | 6.1 | 6 |
| 8 | ureq | 15.4 / 15.7 | 400 / 414 | 23 | 9.1 | 13 |
| 8 | async | 15.4 / 15.9 | 411 / 426 | 40 | 6.5 | 6 |
| 32 | ureq | 446 / 1285 | 860 / 1682 | 80 | 10.1 | 13 |
| 32 | async | 15.5 / 16.3 | 400 / 419 | 53 | 7.5 | 6 |
| 128 | ureq | 2912 / 6164 | 3308 / 6564 | 331 | 11.3 | 13 |
| 128 | ureq, 128 network workers | 15.9 / 18.2 | 400 / 418 | 201 | 52.0 | 133 |
| 128 | async | 16.2 / 18.4 | 399 / 416 | 168 | 11.0 | 6 |

CPU at 1 and 8 streams is within noise: ten more runs each gave medians
of 7.5 ms (ureq) and 9.1 ms (async) at one stream, 34.6 and 33.0 ms at
eight, with ranges overlapping.

- **No queueing.** At 32 and 128 streams ureq's replies wait for one of
  eight workers; the async client starts every stream at once, at the
  latency of one. Matching it with ureq takes 128 workers: 133 threads
  and five times the RSS.
- **CPU** is lower from 32 streams: no channel hop per chunk and no worker
  wake-ups.
- **Memory** per active stream includes its 16 KiB HTTP read buffer.
  Pooling drops the exchange buffers; TLS sessions retain their record
  buffers. Compressed replies also keep codec state and a decode buffer. RSS at 128 concurrent streams equals ureq's
  bounded eight.
- **Cold first request** against `https://api.openai.com/v1/models`
  (401 reply, three runs each, from the same host): ureq 289–367 ms,
  async 284–364 ms; warm requests 237–291 ms for both. The link's round
  trip dominates; neither client adds measurable time. rustls (the
  `tls-rustls` feature) was checked against OpenAI and Gemini the same way.
- **Binary**: the probe is 675 KiB against 760 KiB with ureq.

## Hosted

**Daemon under load.** The sessions suite (`gh workflow run perf -f
sessions=true`, [runtime-demand.md](runtime-demand.md)), run
[40216350510](https://github.com/arut-land/a/actions/runs/40216350510)
against the ureq run 40201401042, default layout `A`, five rounds:

| Runner | Sessions | Turn p50 (ms) | Turn p99 (ms) | Delta p99 (ms) | Active CPU (ms) | Idle RSS (MiB) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ubuntu-latest | 32 | 1046 → 348 | 1742 → 444 | 0.11 → 0.92 | 860 → 1150 | 16.3 → 15.2 |
| ubuntu-latest | 128 | 3945 → 612 | 4997 → 754 | 0.32 → 7.2 | 3730 → 4550 | 31.0 → 33.6 |
| ubuntu-24.04-arm | 32 | 1046 → 331 | 1370 → 392 | 0.14 → 0.35 | 800 → 890 | 14.6 → 14.3 |
| ubuntu-24.04-arm | 128 | 4079 → 578 | 4724 → 719 | 0.29 → 4.6 | 3820 → 3660 | 30.1 → 32.3 |
| macos-latest | 32 | 5024 → 1376 | 6723 → 1694 | 1.47 → 1.99 | 1578 → 911 | 19.0 → 19.3 |
| macos-latest | 128 | 19413 → 1360 | 27770 → 1878 | 1.73 → 6.4 | 6940 → 2518 | 35.2 → 42.7 |
| macos-15-intel | 128 | 13561 → 4300 | 21135 → 6689 | 1.96 → 17.1 | 11348 → 11521 | 28.0 → 28.8 |
| windows-latest | 128 | 10888 → 8394 | 17670 → 12529 | 0.15 → 0.26 | 5172 → 7531 | 41.0 → 38.3 |
| windows-11-arm | 128 | 7263 → 8330 | 11226 → 12447 | 0.80 → 0.62 | 6766 → 8891 | 37.2 → 36.4 |

At 1 and 8 sessions every runner is within noise of before, and idle RSS
is equal or lower (ubuntu-latest 8.2 → 7.7 MiB at one session).

- **Turns no longer queue.** Linux turn p50 at 128 sessions is 578–612
  ms, against 3945–4079 ms on eight workers and 497–532 ms on 128; macOS
  drops from 19 s to 1.4 s. On Windows a turn is bound by PowerShell
  start-up for the shell call, as before: 128 concurrent starts now
  overlap, which moves turn latency by ±25% and CPU up.
- **Delta p99 at 128** rises to 5–17 ms, as it did with 128 network
  workers (6.5 ms): 128 replies now stream at once over four async
  workers. It stays under a frame interval except on macos-15-intel,
  where this run also predates the fix for deltas dropped under load
  (the dropped counts it shows are that bug).
- **CPU** in this run fell by two thirds on macos-latest, was flat on
  Linux arm and rose by 22–34% on ubuntu-latest and Windows at 32 and 128.
  A second run on the branch merged with main
  ([40220709639](https://github.com/arut-land/a/actions/runs/40220709639),
  which also has main's late-delta fix) puts ubuntu-latest at 3010 ms and
  ubuntu-24.04-arm at 2980 ms for 128 sessions (ureq: 3730 and 3820), turn
  p50 520 and 483 ms, no failed calls; Windows stays PowerShell-bound
  (windows-latest turn p50 10652 ms, CPU 5500 ms, against 10888 and
  5172). Runner draws vary by a fifth between runs. What the readiness
  model does cost per event is a read that would block, a re-arm of the
  one-shot registration (`epoll_ctl`, or AFD on Windows) and the reactor
  thread's wake, where a blocked `read` had none of them. That is below
  the client: a persistent edge-triggered registration or a completion
  backend (io_uring, IOCP) removes it without changing the client (see
  "What the client needs from rt").
- **macos-15-intel's delta p99 at 128** is 715 ms in the second run (no
  dropped deltas): the deltas main used to drop now wait for their call's
  flush, which 128 sessions share; main measured the same with 128 network
  workers (280 ms). It is the flush lane, not the transport.
- **Idle RSS after 128 sessions** is 1–3 MiB higher on Linux (128
  replies' buffers, retained by the allocator) and 7.5–9.7 MiB on
  macos-latest (second run: 44.9 MiB, turn p50 1689 ms, CPU 3828 ms),
  against 30–43 MiB more for 128 network workers (macos-latest 79.5 MiB).
  From 1 to 32 sessions it is equal or lower.

**Idle, start-up and size**, paired against main on the same runners
(run [40216532258](https://github.com/arut-land/a/actions/runs/40216532258),
five interleaved rounds): idle RSS, cold reply and attach are within
noise on every runner (idle RSS ubuntu-latest 8616 KiB, +0.9%; macos-latest
9424 KiB, −0.5%), and idle CPU stays zero. The release binary is 82–103
KiB smaller everywhere (ubuntu-latest 9826 → 9733 KiB, macos-latest 5090
→ 5008 KiB, windows-latest 11656 → 11553 KiB).

## HTTP/2 and HTTP/3

Every provider host checked negotiates HTTP/2 over ALPN (`curl --http2`,
2026-10-25): api.openai.com, api.anthropic.com,
generativelanguage.googleapis.com, openrouter.ai, api.x.ai,
api.deepseek.com, api.groq.com, api.mistral.ai. OpenAI, Gemini, Groq and
Mistral also advertise `h3` in `alt-svc`; Anthropic, OpenRouter, xAI and
DeepSeek do not.

HTTP/2's gain was measured with hyper and tokio in a throwaway probe
outside the workspace (`h2probe`): one multiplexed TLS connection against
a pool of HTTP/1.1 TLS connections, loopback, the same 30-event replies,
three turns, four workers, three runs:

| Streams | | Client CPU (ms) | First event p99 (ms) | RSS over start (MiB) |
| ---: | --- | ---: | ---: | ---: |
| 8 | HTTP/1.1 pool | 12–19 | 13.6 | 1.6 |
| 8 | HTTP/2, one connection | 6–9 | 12.6 | 1.1 |
| 32 | HTTP/1.1 pool | 40–44 | 15.6 | 3.1 |
| 32 | HTTP/2, one connection | 19–31 | 13.0 | 1.6 |
| 128 | HTTP/1.1 pool | 133–153 | 28 | 8.7 |
| 128 | HTTP/2, one connection | 78–85 | 15 | 2.6 |

At 128 concurrent streams HTTP/2 halves client CPU and saves about 6 MiB
of TLS sessions and socket buffers, and over a real link it saves one TLS
handshake (two round trips) per connection the HTTP/1.1 pool has to open:
the pool keeps three idle connections per host, so a burst of 128 turns
opens up to 125. At 1 to 8 streams the difference is a few milliseconds
of CPU per turn.

It does not change the decision for this change:

- `h2`, the maintained implementation, depends on tokio's IO traits and
  `tokio-util` (excluded). `hreq-h2` (a tokio-free fork) was last released
  in 2021. `ringline-h2` (0.6.0, 2026-09) is a sans-IO HTTP/2 client with
  no dependencies, flow control and HPACK, six months old and half
  documented. It is the candidate when HTTP/2 is next taken up.
- The client is built so HTTP/2 slots in behind `Conn`: ALPN on the TLS
  handshake picks the protocol, and a multiplexed connection is shared
  rather than pooled. That is a follow-up for the owner to schedule.

HTTP/3 does not pay for these providers now. Its gains are handshake
round trips (0-RTT) and no head-of-line blocking on lossy links; a model's
reply takes seconds to generate, and connections are reused. Anthropic,
OpenRouter, xAI and DeepSeek do not offer it. `quinn` and `h3` both
require tokio, even with quinn's `runtime-smol`.

## What the client needs from rt

- `rt::Async::<TcpStream>::connect(reactor, addr)`: a non-blocking
  connect completed on the reactor (async-io's).
- `Async::read_with`, `Async::write_all`, `readable()`, `writable()`, and
  `get_ref()` for TLS's own reads and writes on the non-blocking socket.
- `Runtime::sleep` for the connect and head timeouts, `Runtime::now` for
  pool expiry, `rt::or` for the races.
- `rt::resolve(rt, host, port)`: name resolution. Today it is
  `getaddrinfo` on the network lane; an OS-native asynchronous resolver
  (`DNSServiceGetAddrInfo`, `GetAddrInfoExW` with an overlapped
  completion, or a resolver over `rt::Async` UDP) replaces it behind the
  same function.

A completion-based backend (io_uring, IOCP) needs owned-buffer reads
behind `read_with`, which the client already calls with a buffer it owns
for the call.
