# llmconduit

LLM API gateway for local and OpenAI-compatible chat-completions backends.

It accepts OpenAI Responses, OpenAI Chat Completions, and Anthropic Messages
requests, normalizes them, and forwards them to an upstream
`/v1/chat/completions` server. It can also run server-side tools such as Brave
Search.

## Build

```bash
cargo build --release
```

## Configure

```bash
./target/release/llmconduit configure
```

The default config path is:

```text
~/.config/llmconduit/config.yaml
```

Minimal config:

```yaml
bind_addr: "127.0.0.1:4000"
upstream_base_url: "http://127.0.0.1:8000/v1"
upstream_model: "Qwen3.5"
```

Multi-upstream model routing:

```yaml
upstreams:
  - name: "local"
    upstream_base_url: "http://127.0.0.1:8000/v1"
  - name: "openrouter"
    upstream_base_url: "https://openrouter.ai/api/v1"
    upstream_api_key: "..."
```

When `upstreams` is configured, llmconduit exposes the ordered union of the
primary upstream model catalogs. If a request omits `model`, passes a blank
model, or requests a model that is not currently available, llmconduit uses the
first model from the first upstream with a catalog entry. Requested model names
are normalized against the catalogs, so aliases such as different case or
punctuation route to the exact model id exposed by the backend. If multiple
upstreams expose the same model id, the first upstream wins.

Optional nested fallback providers:

```yaml
upstreams:
  - name: "local"
    upstream_base_url: "http://127.0.0.1:8000/v1"
    fallback_upstreams:
      - name: "backup"
        upstream_base_url: "https://openrouter.ai/api/v1"
        upstream_api_key: "..."
        upstream_model: "openai/gpt-4.1-mini"
        exposed_model: "GPT-4.1-mini"
        upstream_chat_kwargs:
          provider:
            order:
              - z-ai
            allow_fallbacks: true
```

If a selected upstream fails before producing the first chat chunk, only that
upstream's nested `fallback_upstreams` are tried. llmconduit does not treat the
next model-routing upstream as a failure fallback. Fallback models are not shown
in `/v1/models` unless `exposed_model` is set. A fallback `upstream_model` is
optional; when set, fallback requests use that model, otherwise they keep the
routed primary model id. `exposed_model` advertises a fallback model under a
client-facing alias and routes requests for that alias to the declaring fallback
provider.
Fallback `upstream_chat_kwargs` are merged only when that fallback is selected,
with per-model kwargs and explicit request values taking precedence.

The legacy top-level `upstream_*` and `fallback_upstreams` settings still work
when `upstreams` is not configured.

Global and per-model request defaults:

```yaml
system_prompt_prefix: |
  Shared instructions prepended to every request.

upstream_chat_kwargs:
  stream_reasoning: true

model_profile_templates:
  thinking:
    separate_reasoning: true
    chat_template_kwargs:
      enable_thinking: true

model_profiles:
  GLM-5.1:
    extends:
      - thinking
    chat_template_kwargs:
      clear_thinking: false

  Kimi-K2.6:
    extends:
      - thinking
    system_prompt_prefix: |
      Extra Kimi-specific instructions.
    chat_template_kwargs:
      preserve_thinking: true
```

`system_prompt_prefix` is prepended to all Responses, Chat Completions, and
Anthropic Messages requests. A profile-specific prefix is appended after the
global prefix. `upstream_chat_kwargs` merge in this order: top-level defaults,
matched model profile templates, matched model profile, then explicit request
values. In model profiles and templates, extra profile-level keys are shorthand
for upstream chat kwargs; the explicit `upstream_chat_kwargs` wrapper still
works and overrides the shorthand when both set the same key. When a profile
`extends` multiple templates, the `extends` list is applied in declaration
order: later entries override earlier ones, and the profile's own fields
override all templates.

### Reserved `*` profile

A profile keyed `*` is a pure fallback for per-model settings. When a request
names a model that no specific `model_profiles` entry matches, the `*` profile
stands in as that model's profile: its `upstream_chat_kwargs` and
`system_prompt_prefix` apply. When a specific profile DOES match, the `*`
profile is not consulted at all - an explicit match never inherits unset fields
from `*`. The `*` profile can itself `extend` templates, so extending a shared
template is the way to give `*` and explicit profiles common defaults. Use
`model_profile_templates` (`extends`) to share fields between explicit
profiles, not `*`.

Per-model profile matching precedence, highest to lowest:

1. The request model - matched by name (case-insensitive) against `model_profiles`.
2. The resolved/upstream model (after `upstream_model` rewriting) - matched by name.
3. The reserved `*` profile - used only when neither 1 nor 2 matches.

Top-level config is the base below all profiles: `upstream_chat_kwargs` is the
deep-merge base, and `system_prompt_prefix` is always prepended. Client request
values still override profile settings, as described above.

```yaml
model_profiles:
  # Fallback for any model without an explicit profile.
  "*":
    upstream_chat_kwargs:
      chat_template_kwargs:
        enable_thinking: true

  GLM-5.2:
    upstream_chat_kwargs:
      chat_template_kwargs:
        enable_thinking: false
```

With this config, a request for `GLM-5.2` uses only the `GLM-5.2` profile
(`enable_thinking: false`); the `*` profile contributes nothing. A request for
any other model (e.g. `Qwen-3`) falls back to `*` (`enable_thinking: true`).

### Reasoning effort

A profile's `reasoning_effort` block shapes the upstream `reasoning_effort` field
(the value Claude Code sends as `output_config.effort`, and the value OpenAI
clients send as `reasoning_effort`) and controls the thinking template kwarg the
gateway injects on the Anthropic route. Effort shaping applies on every
converting route (`/v1/messages`, `/v1/responses`, `/v1/chat/completions`, and
`/v1/messages/count_tokens`); the thinking-template-kwarg injection applies only
on the Anthropic routes (`/v1/messages` and `/v1/messages/count_tokens`).

```yaml
model_profiles:
  "*":
    reasoning_effort:
      default: high
      map:
        low: high
        xhigh: max
        "*": high
      thinking_param_name: enable_thinking
      thinking_param_value_on: true
      thinking_param_value_off: false
```

- `map` translates a client effort level to an upstream effort string. Keys match
  case-insensitively. A level that is not listed passes through verbatim, unless
  the reserved `*` entry is set, which rewrites every otherwise-unlisted level. An
  explicit level always wins over `*`.
- `default` is the effort emitted when the client sends no effort string. `default:
  null` (or omitting it) sends no `reasoning_effort` field. `*` does not apply to
  this case.
- Anthropic clients expect thinking to be **off** unless the request explicitly
  enables it, but some upstreams treat an absent `enable_thinking` kwarg as thinking
  *on*. So on the Anthropic route the gateway always injects a
  thinking template kwarg into `chat_template_kwargs`, stating on/off explicitly
  rather than leaving it to the upstream default or inferring it from the effort
  value. `thinking_param_name` is the kwarg name (default `enable_thinking`);
  `thinking_param_value_on` / `_off` are the values for thinking-on and thinking-off
  (defaults `true` / `false`, but any JSON value is allowed). The injected value
  overrides any static `chat_template_kwargs` default for that key, and a profile
  with no `reasoning_effort` block still injects the built-in `enable_thinking:
  true`/`false`.
- A resolved effort of `none` also forces the off-value on the Anthropic route, even
  when the request enabled thinking. This is what makes a `map` that clamps low levels
  to `none` (e.g. z.ai's `minimal`/`none` -> `none`) actually skip thinking.
- Chat Completions and native Responses clients control the thinking kwarg
  themselves via `chat_template_kwargs` in the request; the gateway never injects
  one for them.
- A profile with no `reasoning_effort` block applies no effort shaping: the client
  effort is forwarded if present, otherwise omitted (no clamp).

The reserved `*` profile shapes models with no matching profile. Resolution
is strict: the highest-precedence `reasoning_effort` block wins wholesale (no
field-merging across `extends` or the request/configured/resolved chain), and a
matched profile without a block is not back-filled from `*`.

### Example: GLM-5.2 on vLLM

On vLLM, GLM's `reasoning_effort` is a top-level field and `enable_thinking` is a
chat-template kwarg. GLM treats an absent `enable_thinking` as thinking *on*, which
contradicts Anthropic's default-off semantics when a client omits thinking, so the
gateway injects `enable_thinking: true`/`false` on the Anthropic route to state it
explicitly (the defaults already match GLM, so no `thinking_param_*` keys are
needed). The `map` reproduces the z.ai clamp for the effort string, and a level that
clamps to `none` also forces `enable_thinking: false` so the clamp actually skips
thinking.

GLM's allowed levels are `max` (default, deep reasoning), `xhigh`, `high`,
`medium`, `low`, `minimal`, `none`. z.ai clamps them: `none`/`minimal` skip
thinking, `low`/`medium` map to `high`, and `xhigh` maps to `max`. `high` and
`max` are native levels, so they pass through unchanged and need no map entry.

```yaml
model_profiles:
  GLM-5.2:
    reasoning_effort:
      map:
        none: none
        minimal: none
        low: high
        medium: high
        xhigh: max
```
Optional Brave Search:

```yaml
brave_api_key: "..."
```

## Run

```bash
./target/release/llmconduit start
```

Useful flags:

```bash
./target/release/llmconduit start --raw
./target/release/llmconduit start --with-debug-ui
```

The gateway listens on `http://127.0.0.1:4000` by default.

## Codex

```toml
[model_providers.llmconduit]
name = "llmconduit"
base_url = "http://127.0.0.1:4000/v1"
wire_api = "responses"
requires_openai_auth = false

[profiles.llmconduit]
model_provider = "llmconduit"
model = "Qwen3.5"
```

```bash
codex -p llmconduit "what files are in this directory?"
```

## Docker

```bash
docker build -t llmconduit .
docker run --rm -p 4000:4000 \
  --add-host=host.docker.internal:host-gateway \
  -e LLMCONDUIT_UPSTREAM_BASE_URL=http://host.docker.internal:8000/v1 \
  llmconduit
```

## Endpoints

| Endpoint | Description |
|-|-|
| `POST /v1/responses` | OpenAI Responses API |
| `POST /v1/chat/completions` | OpenAI Chat Completions API |
| `POST /v1/messages` | Anthropic Messages API |
| `GET /v1/models` | Proxied model list |
| `GET /healthz` | Health check |
| `GET /debug` | Debug UI when started with `--with-debug-ui` |

## Environment

Common overrides:

```text
LLMCONDUIT_BIND_ADDR
LLMCONDUIT_UPSTREAM_BASE_URL
LLMCONDUIT_UPSTREAM_API_KEY
LLMCONDUIT_UPSTREAM_MODEL
LLMCONDUIT_SYSTEM_PROMPT_PREFIX
LLMCONDUIT_UPSTREAM_CHAT_KWARGS_JSON
LLMCONDUIT_UPSTREAM_FAILURE_COOLDOWN_SECS
LLMCONDUIT_BRAVE_MAX_RESULTS
LLMCONDUIT_REQUEST_TIMEOUT_SECS
LLMCONDUIT_CONNECT_TIMEOUT_SECS
LLMCONDUIT_MAX_WEB_SEARCH_ROUNDS
LLMCONDUIT_MAX_REPLAY_ENTRIES
LLMCONDUIT_FLATTEN_CONTENT
BRAVE_SEARCH_API_KEY
OPENAI_API_KEY
```

`OPENAI_API_KEY` is used as a fallback upstream API key.

## Request Logs

Set this in config to write upstream chat requests as JSONL:

```yaml
upstream_request_log_path: "/tmp/llmconduit-upstream.jsonl"
```

Then inspect prefix stability:

```bash
llmconduit analyze-log
```

## Test

```bash
cargo test
```

## License

MIT
