# OneLoop

A local-first coding agent. It runs against a model on your own machine by
default — no API key, no account, nothing leaving the box — and reaches a
hosted model only when you ask it to. One loop, four tools, zero config.

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
- `/model` — list the configured models and switch to one
- `/model <alias>` — switch straight to that model
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
models — the default `local` model uses no credentials.

`./ol` is a thin wrapper that runs OneLoop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, or `bash`.

Two things are reachable but are not tools. `skill` is on-demand prompt engineering — it returns a markdown playbook from `.oneloop/skills/` for the model to follow, and does nothing to the machine; it is registered only when such files exist. Web search and fetching are OpenRouter's, executed server-side and returned inside the assistant message (metered per use; disable with `ONELOOP_WEB_TOOLS=false`).

## Directives

Directives use `#!directive words#!` followed by the user message:

- `#!flash#! explain this file` — route to the `flash` model
- `#!flash model:deepseek/deepseek-v3-0324#! refactor this` — one-off wire id
- `#!model:deepseek/deepseek-v4-flash-0731#! hard problem` — one-off id, default model
- `#!local flash#! should we do X?` — consensus (2+ models defaults to consensus)
- `#!consensus local flash judge:flash#! question` — explicit consensus with judge
- `#!debate local flash rounds:2 judge:flash#! question` — debate with 2 rounds
- `#!local format:md#! summarize` — single model with markdown output

Tokens between `#!...#!` are space-separated: model aliases, mode keywords
(`consensus`, `debate`), and key:value modifiers (`model:provider/name`,
`judge:flash`, `rounds:2`, `tools:none`, `format:md`, `format:html`). No `#!`
at all means plain prompt with the default model. `model:` is only valid in
single-model mode; `judge:`, `rounds:`, and `tools:` require consensus or
debate mode.

## Providers and models

A **provider** is a place to send requests — a base URL and, if hosted, the
environment variable holding its key. A **model** is one thing that place
will run. OpenRouter is a single provider serving hundreds of models, so the
URL and key are stated once and the models listed under them.

Each model belongs to its provider and is sent through it: one URL, one key,
one connection, however many models are listed under it. `/model` shows them
grouped that way.

Every model has a short **alias**, which is the name used everywhere else:
`#!consensus flash pinned#!` rather than the wire ids those resolve to.
Aliases are unique across all providers, so a directive never has to say
which provider it meant.

Config is `~/.oneloop/config.json`, written from a template on first run —
shown here with a second OpenRouter model added:

```json
{
  "default": "local",
  "providers": {
    "local": {
      "base_url": "http://localhost:8080/v1",
      "models": {
        "local": { "id": "local", "max_tokens": 4096, "context_window": 128000 }
      }
    },
    "openrouter": {
      "base_url": "https://openrouter.ai/api/v1",
      "api_key_env": "OPENROUTER_API_KEY",
      "web_tools": true,
      "models": {
        "flash":  { "id": "~deepseek/deepseek-v4-flash-latest", "context_window": 128000 },
        "pinned": { "id": "deepseek/deepseek-v4-flash-0731", "context_window": 128000 }
      }
    }
  }
}
```

Adding a model for consensus is a few lines under its provider — no repeated
URL, no repeated key. Provider keys: `base_url`, `api_key_env` (omit for a
server that needs none), `web_tools`, `models`. Model keys: `id` (what goes
on the wire), `context_window`, `max_tokens`, `temperature`, `web_tools`;
model settings override the provider's.

`default` names the alias used when nothing else is asked for. It is `local`
out of the box, which needs no credentials, so an unconfigured checkout
cannot accidentally bill a hosted model.

`/model` switches the active model for the rest of a session and leaves the
file alone; `default` is what the next run starts on, and changing that stays
an edit you make on purpose.

**This file holds no secrets.** A provider names the environment variable
its key lives in; the key itself is written by `oneloop login openrouter`
into `~/.oneloop/auth.json` (0600). That keeps the config shareable —
committable to dotfiles, diffable, pasteable — which it could not be if a
key were in it.

Override for a single run:

- `ONELOOP_MODEL=<alias>` — use a different model
- `ONELOOP_CONTEXT_WINDOW_TOKENS` — override the active model's window
- `ONELOOP_WEB_TOOLS` — server-side web search/fetch on the active model

`ONELOOP_PROVIDER` is accepted as the old name for `ONELOOP_MODEL`.

### Running the local server

The `local` provider expects an OpenAI-compatible server on port 8080. This
flake builds and runs one:

```bash
nix run .#serve
```

With no model present it offers to download one (~20 GB, into `~/models/`)
and starts the server once it lands. The download resumes if interrupted,
and lands as `.part` until complete — an aborted transfer never looks like a
usable model. To use different weights:

```bash
nix run .#serve -- /path/to/other.gguf
# or: ONELOOP_LOCAL_MODEL=/path/to/other.gguf nix run .#serve
```

The offer is only made for the default, and only with a terminal attached:
a script or CI run gets the `curl` command printed instead of a surprise
20 GB transfer.

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
- `ONELOOP_MAX_RETRIES` — attempts before offering another model (default: `3`)
- `ONELOOP_COMPACTION_THRESHOLD` — % of context window that triggers auto-compaction (default: `85`)
- `ONELOOP_COMPACT_USER_MSG_TOKENS` — recent user-message tokens preserved across compaction (default: `20000`)

A provider names the environment variable holding its key (`api_key_env`);
that variable is read first, then `~/.oneloop/auth.json` — an explicitly set
env var always wins. The default `local` provider names none, so a default
run needs no credentials anywhere.

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
