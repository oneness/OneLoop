# OneLoop

A tiny coding agent. One loop, multiple models, five tools, zero config.

## Quick links

- **[Overview](https://www.birkey.co/oneloop/)** — executive presentation (space-bar to navigate)
- **[Architecture](docs/architecture.md)** — how the agent loop, providers, tools, and sessions work
- **[Style guide](docs/style-guide.md)** — coding conventions and lint config

## Usage

### Interactive mode

```bash
./ol
```

Starts an interactive REPL. Type your message and press Enter.

Commands:
- `/clear` — wipe context and start a fresh session
- `Ctrl+C` — stop a running request
- `Ctrl+D` — exit

### One-shot mode

```bash
./ol "your prompt here"
```

Runs a single prompt and exits.

### Piped input

```bash
git diff | ./ol "summarise these changes"
cat error.log | ./ol "what is causing this?"
```

When stdin is a pipe, its content is prepended to the prompt and the agent runs non-interactively.

### Login

```bash
./ol login openrouter
```

Stores the API key in `~/.oneloop/auth.json`. Only needed to reach hosted
models — the default `local` endpoint uses no credentials.

`./ol` is a thin wrapper that runs OneLoop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, `bash`, or `skill` (when skill files exist under `.oneloop/skills/`). Web search and page fetching are not OneLoop tools: on OpenRouter, the agent enables the server-side `openrouter:web_search` and `openrouter:web_fetch` tools, which the model invokes when it needs the web and OpenRouter executes itself (metered per use; disable with `ONELOOP_WEB_TOOLS=false`).

## Directives

Directives use `#!directive words#!` followed by the user message:

- `#!openrouter#! explain this file` — route to the `openrouter` endpoint
- `#!openrouter model:deepseek/deepseek-v3-0324#! refactor this` — specific model
- `#!model:anthropic/claude-opus-4#! hard problem` — model override, default endpoint
- `#!local openrouter#! should we do X?` — consensus (2+ endpoints defaults to consensus)
- `#!consensus local openrouter judge:openrouter#! question` — explicit consensus with judge
- `#!debate local openrouter rounds:2 judge:openrouter#! question` — debate with 2 rounds
- `#!local format:md#! summarize` — single endpoint with markdown output

Tokens between `#!...#!` are space-separated: endpoint names, mode keywords
(`consensus`, `debate`), and key:value modifiers (`model:provider/name`,
`judge:openrouter`, `rounds:2`, `tools:none`, `format:md`, `format:html`). No
`#!` at all means plain prompt with the default endpoint. `model:` is only
valid in single-endpoint mode; `judge:`, `rounds:`, and `tools:` require
consensus or debate mode.

## Endpoints

Every endpoint speaks OpenAI Chat Completions, so a local llama-server and a
hosted model differ only by URL, model name, and whether a key is needed.
They are configured in `~/.oneloop/endpoints.json`:

```json
{
  "default": "local",
  "endpoints": {
    "local":      { "base_url": "http://localhost:8080/v1",
                    "model": "local",
                    "max_tokens": 4096 },
    "openrouter": { "base_url": "https://openrouter.ai/api/v1",
                    "model": "deepseek/deepseek-v4-flash",
                    "api_key_env": "OPENROUTER_API_KEY",
                    "web_tools": true }
  }
}
```

Without that file those two endpoints are the built-in defaults, and `local`
is the default — an unconfigured checkout cannot accidentally bill a hosted
model. Add as many endpoints as you like; the names are what `#!consensus
local openrouter#!` refers to.

Per-endpoint keys: `base_url`, `model`, `api_key_env` (omit for a server that
needs no key), `web_tools`, `max_tokens`, `temperature`.

Override with environment variables:

- `ONELOOP_PROVIDER=<endpoint>` — use a different endpoint for this run
- `ONELOOP_OPENROUTER_MODEL` — override the default endpoint's model
- `ONELOOP_OPENROUTER_BASE_URL` — override the default endpoint's URL
- `ONELOOP_OPENROUTER_MAX_TOKENS` — override the default endpoint's output cap
- `ONELOOP_OPENROUTER_TEMPERATURE` — override the default endpoint's temperature
- `ONELOOP_WEB_TOOLS` — server-side web search/fetch on the default endpoint

Only OpenRouter needs credentials (`oneloop login openrouter`). Earlier
versions also supported direct OpenAI and Anthropic providers; any
`openai`/`anthropic` entries left in `~/.oneloop/auth.json` are ignored.

### Running the local server

The `local` endpoint expects an OpenAI-compatible server on port 8080. This
flake builds and runs one:

```bash
nix run .#serve -- ~/models/Qwen3.6-35B-A3B-Q4_K_M.gguf
# or: ONELOOP_LOCAL_MODEL=... nix run .#serve
```

It wraps llama.cpp's Vulkan build with flags measured against
Qwen3.6-35B-A3B — see the comments in `flake.nix` for what each one is worth.
`ONELOOP_LOCAL_PORT` moves it off 8080.

llama.cpp is tracked at upstream master, which ships several builds a day and
where fixes that matter here land quickly — the Qwen3 chat parser
([PR #26252](https://github.com/ggml-org/llama.cpp/pull/26252)) is the
difference between the agent working and silently doing nothing. To take
today's build:

```bash
nix flake update llama-cpp     # ~5 min cold, ~3 min after
```

`flake.lock` pins the revision, so a bad upstream day is
`git checkout HEAD~1 -- flake.lock`. Pin deliberately by changing the input
to a tag (`github:ggml-org/llama.cpp/b10229`).

Building an inference engine has no business gating `cargo check`, so this is
a separate output rather than part of the dev shell — `nix develop` does not
pull it in.

Tuning (all optional):

- `ONELOOP_MAX_ITERATIONS` — cap on agent-loop iterations per prompt (default: `50`)
- `ONELOOP_MAX_RETRIES` — provider retry attempts before offering a fallback (default: `3`)
- `ONELOOP_COMPACTION_THRESHOLD` — % of context window that triggers auto-compaction (default: `85`)
- `ONELOOP_CONTEXT_WINDOW_TOKENS` — assumed context window size (default: `128000`)
- `ONELOOP_COMPACT_USER_MSG_TOKENS` — recent user-message tokens preserved across compaction (default: `20000`)

Credentials are resolved from environment variables first (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`), then from `~/.oneloop/auth.json` — an explicitly set env var always wins.

## Development

```bash
nix develop
cargo check
```

## Contributing

This project is personal software that I maintain for my own use. I do not accept pull requests.

If it's useful to you: fork it, copy the code, adapt it freely. The only ask is that you keep the copyright notice intact (MIT license).

## License

MIT — see [LICENSE](LICENSE).
