# The provider layer (`crates/llm`)

One request in, one streamed reply out, over any HTTP client and any async
runtime. The wire codecs and their host quirks are ported from rig v0.44.0;
everything around them is our own and small.

## Layers

| Piece | File | Role |
| --- | --- | --- |
| Spec | `spec.rs` | Provider-agnostic, serde-stable types. Messages are one enum per role (`System`, `User`, `Assistant`, `Tool`), each holding only the parts that role may. Every part carries `meta`: provider state keyed by the protocol that wrote it. |
| Protocol | `protocol.rs` | A wire protocol is a pure `encode` (request to body) and a pure `Decoder` (frames to events). A `Provider` is data: id, base URL, auth, and a protocol value carrying that host's quirks. `Host` builds one from the environment. |
| Conversation | `conversation.rs` | A conversation's messages, shared by every request that carries them, with what each protocol encoded of them: a fold writes each message's items once, as bytes, and later requests splice those bytes. A tool set keeps its definitions the same way, as bytes per wire. |
| Writer | `writer.rs` | rig's completion fold as a sans-IO helper every decoder writes through (`Writes`). It owns the rules all wires share (see its module doc). |
| JSON | `json.rs` | Lenient reads of what providers send, the borrowing frame reader every decoder shares (`json/read.rs`), and tool arguments parsed so a cut-off call never fails a reply. |
| Models | `models/` | What model ids say: reasoning (levels, budgets, whether it turns off), sampling, vision, output limits and context windows, as id rules and const tables, one module per vendor. Protocols ask here and keep only how their wire spells an option. |
| Replay | `replay.rs` | How an encoder sends back a part its own protocol produced: the provider item verbatim while the part is unchanged (fingerprint), its identity when edited, rebuilt otherwise. |
| Prepare | `prepare.rs` | Runs before every encode: turns from another protocol, provider or model lose opaque items and state, and keep their readable reasoning as tagged context naming its source; foreign tool ids are fitted; unanswered calls get error results. |
| Fit | `fit.rs` | Keeps the newest images within each request's provider size and count limits, replacing older ones with `fit::IMAGE_LEFT_OUT` for that request. The stored conversation keeps every image. |
| Tags | `tag.rs` | Model-facing markers (`Tag`, one renderer) and their vocabulary (`Name`, every tag name): pastes, steers, inbox messages, background results, another model's reasoning, a tool result's moved media. The spelling is a render-only `Syntax` picked at build time by `Markers`, XML-style by default. Text inside a tag is escaped only where a `<` would open or close a tag of ours (`<steer` → `&lt;steer`), so pasted HTML and XML reach the model as given; attribute values also escape `&` and `"`, and no longer `<`, `>` or newlines, so a log whose attributes held those renders differently once (a one-time prompt-cache miss). Tags inside a tag (a paste in a steer) stay tags; a presented result inside a background result keeps spill's `<truncated/>` and escapes every other tag of ours. |
| Client | `client.rs` | `stream` joins a provider, a protocol and an `Http` into a stream of events that ends with exactly one `Finish` or one error. `generate` accumulates it. No retries: `Error::is_retryable` says whether to. |
| HTTP | `crates/transport` | `Http` sends one request and returns status, headers and a body stream. The transport crate owns reusable HTTP pools and the `Ureq` backend (feature `ureq`). |
| Hosts | `hosts.rs` | `host:model` specs (`openrouter:anthropic/claude-sonnet-5`, `openai@chat:gpt-4.1`) to a configured provider, over every `Hosted` protocol's table. |

Protocols live in `protocols/<name>/` with a `KEY`, a protocol type holding
`Quirks`, and a `HOSTS` table:

| Protocol | Key | Hosts |
| --- | --- | --- |
| OpenAI Responses | `openai.responses` | OpenAI, OpenRouter, xAI |
| OpenAI Chat Completions | `openai.chat` | 24, from OpenAI and Azure to Groq, DeepSeek, Mistral, OpenRouter, Together, z.ai, Moonshot, MiniMax, Ollama and llama.cpp |
| Anthropic Messages | `anthropic.messages` | Anthropic, z.ai, MiniMax, Moonshot, Xiaomi MiMo |
| Gemini generateContent | `google.gemini` | Gemini Developer API |

## Adding a host or a protocol

A host on an existing wire is one `HOSTS` entry: id, base URL, key and
base-URL environment variables, key style, and the quirks it needs. A new
wire is a new `protocols/` module implementing `Protocol` and `Hosted`,
listed in `hosts.rs`, whose decoder reads frames through `json::read` and
writes through `Writer`, asking it for what it holds rather than keeping a
copy. A fact about a model goes in `models/`.

## Testing

- `fixtures/rig/cassettes`: rig's recorded streamed exchanges, verbatim.
  `fixtures/rig/expected`: what rig itself decodes from each, written by
  `fixtures/rig/oracle` (`mise run fixtures:rig`). `tests/parity.rs`
  replays every exchange through our client and requires the same parts,
  finish and usage.
- Unit tests beside each module, many ported from rig's.
- `tests/live.rs` (ignored): real calls, including a tool round trip whose
  second request replays the first reply's provider items.

## Deliberate departures from rig

- No model catalog: the few facts request shaping needs are model-id rules
  (`models/`).
- Options a model cannot take are omitted rather than refused: sampling on
  OpenAI reasoning models and on Claude models with fixed sampling, efforts
  and budgets a model has no level for, a forced tool choice where Claude
  rejects one (sent as `auto`).
- Reasoning streams by default: Responses asks for `summary: "auto"` and
  Gemini for `includeThoughts`.
- Readable reasoning from another model is kept, as tagged context where
  it was in that turn: `<reasoning-summary source="anthropic/claude-…">`
  for a summary the provider wrote, `<reasoning source="…">` for raw
  reasoning. The tag names the source so the model does not take it for
  its own answer. Which kind a part is belongs to its producer: each
  protocol module has a `foreign` conversion (Claude 4 and later, Gemini
  thoughts and Responses summaries are summaries; Chat reasoning fields,
  Responses `reasoning_text` and Claude 3.7 or other models over the
  Messages wire are raw), and the target's prepare pass calls the
  source's. Encrypted and redacted reasoning is dropped, with its ids and
  signatures: they mean nothing to another model. The same protocol,
  provider and model still get their own items back verbatim, encrypted
  ones included. `tests/encode.rs` checks this against every recorded
  reply that reasons.
- Tool results carry images. `Source::Bytes` holds bytes the caller has
  (an image a tool read, kept as a blob) and becomes base64 only as a codec
  writes the body. A model that reads images inside a result gets them
  there; any other gets them in a user message after the results, headed
  by `<tool-result-media call="…" tool="…"/>` naming the call they answer
  (`tag::ResultMedia`), where rig wrote one untagged heading for all.
  `tests/media.rs` checks every wire's shape.
- `read` detects PNG, JPEG, WebP, GIF and BMP by signature and a valid
  header. Upright PNG/JPEG/WebP within 2000 px and 3,750,000 bytes pass
  through; GIF becomes its first frame (PNG when it fits), BMP and oversized
  images are converted, and EXIF orientation is applied. `with_held` exposes
  PNG/JPEG/WebP blobs as image parts while preserving existing media.
  See [read measurements](perf/read-images.md).
- Replayed items drop `null` fields, and replayed Anthropic thinking, text and
  `tool_use` blocks keep only their input fields: strict input validators
  reject what a stream added (a reasoning item's `status: null`, Vertex's
  `estimated_tokens`). Both were found by live round trips.

## Request image limits

`fit.rs` counts images in user messages and tool results and measures the
complete encoded body, including text, tools, options, overlays, image
framing and base64. It leaves out the oldest images until both the byte
and count caps are met. Each omitted image becomes `fit::IMAGE_LEFT_OUT`
in this request alone; its blob and conversation part remain available for
replay and other providers. Non-image content is preserved even when it
alone exceeds the cap, leaving that rejection to the provider. Fitting
does not change the model's context limit. Limits follow the wire protocol;
compatible hosts can impose their own lower limits.

| Protocol | Request cap | Image count | Source |
| --- | ---: | ---: | --- |
| Anthropic Messages | 32 MB | 100 for context windows up to 200k (also the unknown-model default); 600 above 200k | [API overview](https://platform.claude.com/docs/en/api/overview), [vision](https://platform.claude.com/docs/en/build-with-claude/vision) |
| Gemini | 20 MB with inline data | 3,600 | [Image understanding](https://ai.google.dev/gemini-api/docs/image-understanding) |
| OpenAI Chat and Responses | 512 MB | 1,500 | [Images and vision: input requirements](https://developers.openai.com/api/docs/guides/images-vision#image-input-requirements) |

OpenAI limits were checked against the official documentation on
2026-10-26. Its [production guide's 128 MiB body limit](https://developers.openai.com/api/docs/guides/production-best-practices#compress-request-bodies)
is documented for compressed Responses requests; this client sends
uncompressed JSON and uses the vision guide's image limits.
