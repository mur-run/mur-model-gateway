# Codex Chat Completions Translation (Stage 2)

**Date**: 2026-08-11
**Status**: draft

## Context

Stage 1 (`2026-08-11-codex-oauth-design.md`) shipped a working Codex route: `/v1/responses*` reaches
ChatGPT's Codex backend with the user's `~/.codex/auth.json` credentials attached and refreshed on
401. It is reachable only by a client that speaks the Responses API.

No MUR agent speaks it. MUR's OpenAI client posts to `$base_url/chat/completions`
(`mur-agent-runtime/src/llm/openai.rs:296,373`) and has no Responses client, so the stage-1 route is
a path nobody can walk. Stage 1's own spec records this as the blocking item.

The gateway has never rewritten a body. `proxy()` picks an upstream from the path, attaches headers,
and forwards bytes (`src/lib.rs:53,278,420`). Stage 2 introduces body translation — the first such
capability in this codebase, and the reason it is scoped as its own spec.

Outcome: a MUR agent pointed at the gateway's Codex path can hold a normal tool-using conversation,
streaming or not, against a ChatGPT subscription, using the OpenAI client it already has.

## Decisions

| Question | Decision |
|---|---|
| How a Chat Completions request selects Codex | A distinct path prefix, `/codex/v1/chat/completions` |
| `X-Upstream` header override | Out of scope — no caller needs it, and two sources of truth for "which upstream" is a debugging trap |
| Translation scope | Non-streaming, SSE streaming, tool calls, and usage/`finish_reason`/errors — all four |
| Conversation state | Stateless: `store: false`, full history every call, no `previous_response_id` |
| Non-streaming replies | Aggregated from the upstream SSE — the upstream has no non-streaming mode (see Ground truth) |
| Unmappable parameters | Split — semantic ones are a 400, cosmetic ones are dropped with a warning |
| `model` string | Passed through verbatim; the gateway keeps no allowlist |
| Compression | Enabled, running **before** translation, reusing the existing OpenAI rewriter |
| Where the code lives | Tracked source, not the gitignored `codex_impl.rs` |
| Structure | Pure translation functions plus one branch in `proxy()` |

### Why the path prefix

Path alone cannot distinguish "this `/chat/completions` wants Codex" from one that wants real
OpenAI. A prefix decides it without parsing the body, leaves `/v1/chat/completions` byte-for-byte
untouched, and confines every stage-2 risk to a path that did not exist before. MUR agents configure
`http://<proxy>/codex/v1` as the base URL for Codex models and `http://<proxy>/v1` for the rest.

### Why translation is tracked, not hidden

Stage 1 hid the Codex client headers and OAuth constants in `src/codex/codex_impl.rs` because they
were read out of the installed Codex CLI. Neither API's request shape is a secret — both are public
documentation — and hiding the translator would make it untestable in a clean checkout.

## Approach

### Routing

`detect_provider` gains a Codex-with-translation case. Because the new path shares stage 1's upstream
and credentials and differs only in whether bodies are rewritten, translation is **not** a new
`Provider` variant: routing returns `(Provider::Codex, translate: bool)`. `Provider` answers "which
upstream", and adding a variant would force meaningless duplicate arms into `upstream_for` and
`token_source_for`.

- `/codex/v1/chat/completions*` → `(Provider::Codex, translate = true)`, upstream path fixed at `/responses`
- `/v1/responses*` → `(Provider::Codex, translate = false)`, unchanged from stage 1

The `*` matches the exact path, a `/` suffix, or a `?` query, the same three-way form every other
matcher in `detect_provider` uses.

When `translate` is true the upstream path is `/responses` outright, not the string surgery
`codex_target_path()` performs.

### Request flow

```
inbound /codex/v1/chat/completions
  → compress            (rewrite_tool_results_openai — chat shape, already supported)
  → chat_to_responses() (pure; always sets stream:true)
  → codex credentials + apply_codex_headers   (stage 1, unchanged)
  → POST <codex_base>/responses
  → aggregate SSE → responses_to_chat   (client asked stream:false)
    or SseTranslator → chunk frames     (client asked stream:true)
```

Compression runs first so it only ever sees the Chat Completions shape it already understands. This
retires stage 1's "compression for the Responses body shape" item without anyone having to model the
Responses shape at all. `should_compress` returns true for `Provider::Codex` only when `translate`
is set; the raw `/v1/responses` path stays uncompressed as stage 1 decided.

### `chat_to_responses`

| Chat Completions | Responses |
|---|---|
| `messages[]` with role `system` / `developer` | `instructions`, multiple joined by newline |
| `messages[]` with role `user` | `input[] {type:"message", role:"user", content:[{type:"input_text"}]}` |
| `messages[]` with role `assistant`, text only | `input[] {type:"message", role:"assistant", content:[{type:"output_text"}]}` |
| `messages[].tool_calls[]` | `input[] {type:"function_call", call_id, name, arguments}` |
| `messages[]` with role `tool` | `input[] {type:"function_call_output", call_id, output}` |
| `tools[] {type:"function", function:{…}}` | `tools[] {type:"function", name, description, parameters}` — one level flatter |
| `max_tokens` / `max_completion_tokens` | `max_output_tokens` |
| `temperature`, `top_p`, `tool_choice` | same name, passed through |
| `stream` (either value) | `stream: true`, always — the upstream rejects anything else |
| — | `store: false`, always written |

Rejected with 400 before any upstream call: `n > 1`, `seed`, `logprobs`, `top_logprobs`,
`response_format`. Each changes what the caller is entitled to assume about the result, so failing
loudly beats returning something subtly different from what was asked for.

Dropped with a warning: `presence_penalty`, `frequency_penalty`, `logit_bias`, `user`, `stop`. These
tune output rather than define it, and rejecting them would break clients that send defaults.

### `responses_to_chat`

Its input is a Responses object. Because the upstream never sends one, that object is always the
aggregate described above — `output[]` from the `response.output_item.done` items, envelope from
`response.completed`. The function itself does not know or care where the object came from.

| Responses | Chat Completions |
|---|---|
| `output[] {type:"message"}` `output_text` segments | `choices[0].message.content`, concatenated |
| `output[] {type:"function_call"}` | `choices[0].message.tool_calls[]`, `id` from `call_id` |
| `output[] {type:"reasoning"}` | dropped — the chat shape has nowhere to put it |
| `usage.input_tokens` / `output_tokens` / `total_tokens` | `prompt_tokens` / `completion_tokens` / `total_tokens` |
| `incomplete_details.reason == "max_output_tokens"` | `finish_reason: "length"` |
| any `function_call` item present | `finish_reason: "tool_calls"` |
| otherwise | `finish_reason: "stop"` |

The envelope adds `object: "chat.completion"`, `id`, `created`, and `model`.

Two cases the table leaves open, resolved explicitly:

- **`finish_reason` precedence.** A truncated response that also carries a function call is
  `"length"`. Truncation is checked first, because a tool call cut off mid-arguments is not a tool
  call the caller can act on.
- **An assistant message carrying both `content` and `tool_calls`** becomes two `input` items, the
  message first, then the function call — the order the model produced them.

### Streaming

`SseTranslator` consumes one Responses event and emits zero or more `chat.completion.chunk` frames.
Its entire state is three values: whether the role chunk has been sent, a tool-call index counter,
and an `item_id → index` map.

| Responses event | Emitted chunk |
|---|---|
| first event of the stream | `delta:{role:"assistant"}` |
| `response.output_text.delta` | `delta:{content: <delta>}` |
| `response.output_item.added` for a function call | `delta:{tool_calls:[{index, id:call_id, type:"function", function:{name, arguments:""}}]}` |
| `response.function_call_arguments.delta` | `delta:{tool_calls:[{index, function:{arguments: <delta>}}]}` |
| `response.completed` | `delta:{}` with `finish_reason` |
| end of stream | `data: [DONE]` |
| `response.failed` / `error` | an OpenAI-shaped error frame, then close |

Ignored, because the chunk shape has nowhere to put them: `response.created`,
`response.in_progress`, `response.content_part.added`, `response.content_part.done`,
`response.output_text.done`, `response.function_call_arguments.done`, `response.output_item.done`.
The `.done` events repeat content already sent as deltas — emitting them would duplicate the reply.

### Ground truth, captured

The tables and event names below were verified against live Codex traffic on 2026-08-11, using
stage 1's `/v1/responses` route. Fixtures live in `tests/fixtures/codex/`. Four findings changed the
design:

1. **The upstream is streaming-only.** A request without `stream: true` is rejected with
   `400 {"detail":"Stream must be set to true"}`. There is no non-streaming Responses reply to
   translate. `chat_to_responses` therefore always writes `stream: true`, and a non-streaming Chat
   Completions request is served by **aggregating** the SSE stream into one `chat.completion`. See
   "Serving a non-streaming request" below.
2. **`response.completed` carries `usage` and `status` but an empty `output` array.** The assembled
   items arrive one at a time on `response.output_item.done`, each a complete Responses output item.
   Aggregation is therefore: collect every `response.output_item.done` item into `output[]`, take
   the envelope from `response.completed`, and hand the result to `responses_to_chat` unchanged.
3. **Model names are account-tier dependent.** `gpt-5-codex`, `gpt-5`, `gpt-5.1-codex`, and
   `codex-mini-latest` are all rejected with "not supported when using Codex with a ChatGPT
   account"; `gpt-5.4` and `gpt-5.5` are accepted. This validates the verbatim-passthrough decision:
   any allowlist the gateway kept would be wrong for some account.
4. **Every guessed event and field name was correct.** `response.output_text.delta`,
   `response.output_item.added`, `response.function_call_arguments.delta`, and `response.completed`
   all exist as assumed, and function-call items carry `call_id`, `name`, and `arguments` with
   `item_id` on the deltas. Arguments genuinely arrive split — the captured fixture splits
   `{"city":"Taipei"}` across six deltas.

### Serving a non-streaming request

The client's `stream` flag, not the upstream's content type, decides the response shape. The
upstream is always streamed.

```
client stream:false → upstream stream:true → aggregate SSE → responses_to_chat → chat.completion
client stream:true  → upstream stream:true → SseTranslator → chat.completion.chunk frames
```

Aggregation buffers the whole reply, which the non-streaming contract requires anyway.


## Error handling

| Condition | Behaviour |
|---|---|
| Rejected parameter present | 400 in OpenAI error shape, naming the parameter, before any upstream call |
| Inbound body is not a JSON object | 400, no translation attempted |
| Upstream 401 | Stage 1's path unchanged: refresh once, retry once |
| Upstream non-2xx otherwise | Upstream error re-shaped as an OpenAI error; status code passed through |
| Upstream body will not translate | 502, worded to say translation failed rather than the upstream did — conflating the two makes this class of bug very hard to trace |
| Upstream stream ends without `response.completed` | Aggregate what arrived and return it with the `finish_reason` the items imply; a truncated stream is not a gateway fault |
| Failure mid-stream | Headers are already sent and the status cannot change; emit an error chunk, then `[DONE]` |

The 401 retry needs one thing stage 1 did not. Stage 1 forwarded borrowed bytes and could simply
resend them; the translated body is ours, so it must be held in memory until the retry window
closes. The request is fully buffered anyway — both compression and translation require it — so this
adds no cost, but it is easy to miss when writing the retry.

## Verification

Every test reads the captured fixtures; none touches the network.

- `chat_to_responses`: each role, a tool-call round trip, parameter mapping, `stream: true` forced
  regardless of what the client sent, 400 for each rejected parameter, a warning for each dropped one
- `responses_to_chat`: all three `finish_reason` values, usage arithmetic, reasoning items discarded
- Aggregation: the captured SSE fixtures rebuild into the Responses objects the fixtures'
  `.json` counterparts hold
- `SseTranslator`: recorded event sequences in, asserted chunk sequences out, including tool-call
  arguments arriving split across several deltas (the captured fixture splits them six ways)
- Routing: `/codex/v1/chat/completions` → `translate = true`; `/v1/responses` → `translate = false`
- Regression: `/v1/chat/completions` still forwards untouched to real OpenAI, and `/v1/responses`
  behaves exactly as it did in stage 1
- Compression: on the new path it runs before translation, and a compressed body still translates

## Deliberately out of scope

- **`X-Upstream` header override.** Its own spec, if a caller ever needs one endpoint.
- **API-key mode** (`auth_mode: "apikey"`), still resolving to passthrough as in stage 1.
- **Responses-native compression.** Compressing pre-translation removes the need; the raw
  `/v1/responses` path stays uncompressed.
