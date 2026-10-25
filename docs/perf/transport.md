# HTTP transport (2026-10-25)

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
the task that awaits them; nothing is spawned and no thread is held.

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

TLS runs the TLS library's ordinary calls over the non-blocking socket.
A would-block is recorded with the direction the socket refused, and the
call waits for that readiness on the reactor before retrying: the shape
tokio-native-tls and tokio-rustls use, without their runtime.
`futures-rustls` would cover rustls only, and the binary ships native TLS.

## Parity with ureq

What ureq gave the daemon, and what the client does now:

| | ureq | `transport::Client` |
| --- | --- | --- |
| Pool | 10 idle, 3 per host, 15 s | Same limits; expiry checked on use, never on a timer |
| Stale pooled socket | probe before reuse | Same: a non-blocking peek |
| Connect timeout | 30 s over the addresses | 30 s for resolve, connect and TLS, split over the addresses |
| Reply head timeout | 300 s | 300 s |
| Body timeouts | none | none |
| Redirects | 10, auth headers dropped | Same (ureq-proto's rules) |
| Proxy | `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`, `NO_PROXY`, CONNECT | Same for `http://` proxies; an `https://` or SOCKS proxy fails each request rather than being bypassed |
| Decompression, cookies | off | none |
| Cancellation | socket shutdown from the dropping task | dropping the future or body closes the socket |
| DNS | `getaddrinfo` on the request's thread | `rt::resolve`: `getaddrinfo` on the network lane, only for a name and a new connection |

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
- **Memory** per active stream is its 16 KiB read buffer; an idle pooled
  connection holds none. RSS at 128 concurrent streams equals ureq's
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
- **CPU** falls by two thirds on macos-latest and is flat on Linux arm,
  but rises by 22–34% on ubuntu-latest and on Windows at 32 and 128.
  What the readiness model costs per event: a read that would block, a
  re-arm of the one-shot registration (`epoll_ctl`, or AFD on Windows) and
  the reactor thread's wake, where a blocked `read` had none of them.
  That is the reactor's cost, below the client: a persistent
  edge-triggered registration or a completion backend (io_uring, IOCP)
  removes it without changing the client (see "What the client needs
  from rt").
- **Idle RSS after 128 sessions** is 1–3 MiB higher on Linux (128
  replies' buffers, retained by the allocator) and 7.5 MiB on
  macos-latest, against 30–37 MiB more for 128 network workers.

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
