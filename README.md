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
./ol login openrouter      # paste an API key
./ol login openai          # sign in to a ChatGPT Plus/Pro subscription
```

Credentials are stored in `~/.oneloop/auth.json`. Only needed to reach hosted
models — the default `qwen` model runs through the credential-free `local`
provider.

`openai` opens a browser, signs in to ChatGPT, and stores the grant that
comes back; the `codex` model then runs against the subscription rather than a
metered API key — the same account and quota the Codex CLI uses. The access
token is renewed automatically as it expires, so this is a one-time step.

`./ol` is a thin wrapper that runs OneLoop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, or `bash`.

Two things are reachable but are not tools. `skill` is on-demand prompt engineering — it returns a markdown playbook from `.oneloop/skills/` for the model to follow, and does nothing to the machine; it is registered only when such files exist. Web search and fetching are OpenRouter's, executed server-side and returned inside the assistant message (metered per use; disable with `ONELOOP_WEB_TOOLS=false`).

## Providers and models

A **provider** is a place to send requests — a base URL, protocol, and shared
credentials when it needs them. A **model** is one thing that place will run.
OpenRouter is a single provider serving hundreds of models, so its URL and API
key are stated once and the models listed under them. ChatGPT's Codex backend
instead shares a renewable OAuth grant across its models.

Each model belongs to its provider and is sent through it: one URL, one
credential, one connection pool, however many models are listed under it.
`/model` shows them grouped that way.

Every model has a short **alias**, which is the name used everywhere else:
`/model flash` rather than the wire id it resolves to. Aliases are unique
across all providers, so naming one never has to say which provider it meant.

Config is `~/.oneloop/config.json`, written from a template on first run —
shown here with a second OpenRouter model added:

```json
{
  "default": "qwen",
  "providers": {
    "local": {
      "base_url": "http://localhost:8080/v1",
      "models": {
        "qwen":     { "id": "qwen" },
        "glimmer":  { "id": "glimmer" }
      }
    },
    "openai": {
      "base_url": "https://chatgpt.com/backend-api",
      "api": "codex",
      "models": {
        "codex": { "id": "gpt-5.5", "reasoning_effort": "medium" }
      }
    },
    "openrouter": {
      "base_url": "https://openrouter.ai/api/v1",
      "api_key_env": "OPENROUTER_API_KEY",
      "web_tools": true,
      "models": {
        "flash":  { "id": "~deepseek/deepseek-v4-flash-latest" },
        "pinned": { "id": "deepseek/deepseek-v4-flash-0731" }
      }
    }
  }
}
```

Adding a model is a few lines under its provider — no repeated
URL, no repeated key. Provider keys: `base_url`, `api` (`chat` by default,
or `codex`), `api_key_env` (omit for a server that needs none), `web_tools`,
`models`. Model keys: `id` (what goes on the wire), `max_tokens`,
`temperature`, `web_tools`, `reasoning_effort`; model settings override the
provider's.

`api` names the protocol the provider speaks, and with it how it is
authorized. `chat` is OpenAI Chat Completions, which is what everything
except ChatGPT speaks. `codex` is ChatGPT's Codex backend over the Responses
API, reached with the subscription grant `./ol login openai` stores —
so that provider names no `api_key_env`: there is no key to name. Change the
model `id` there to whichever Codex model your plan offers.

There is no `context_window` to declare. The server is the authority on what
fits, and it says so by refusing the request — see [When a thread gets too
long](#when-a-thread-gets-too-long).

`max_tokens` caps output per response and is omitted unless you set it, so a
hosted provider's own default applies. The bundled `local` model leaves it
unset too: `nix run .#serve` starts llama-server with `-n 32768`, which is
the same ceiling in one place instead of two.

`default` names the alias used when nothing else is asked for. It is `qwen`
out of the box, served by the credential-free `local` provider, so an
unconfigured checkout cannot accidentally bill a hosted model.

`/model` switches the active model for the rest of a session and leaves the
file alone; `default` is what the next run starts on, and changing that stays
an edit you make on purpose.

**This file holds no secrets.** A provider names the environment variable
its key lives in; the key itself — or, for a subscription, the OAuth grant —
is written by `oneloop login` into `~/.oneloop/auth.json` (0600). That keeps the config shareable —
committable to dotfiles, diffable, pasteable — which it could not be if a
key were in it.

Override for a single run:

- `ONELOOP_MODEL=<alias>` — use a different model
- `ONELOOP_WEB_TOOLS` — server-side web search/fetch on the active model

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

Provider calls are bounded rather than allowed to stall forever. Every HTTP
client gets 10 seconds to connect. Chat Completions gets 15 minutes for the
whole response; Codex streaming has no overall deadline while it is making
progress, but must begin responding and then produce another chunk within 90
seconds. ChatGPT sign-in waits up to 5 minutes for the browser callback, and
token exchange or renewal gets 30 seconds overall.

A provider names the environment variable holding its key (`api_key_env`);
that variable is read first, then `~/.oneloop/auth.json` — an explicitly set
env var always wins. The default `local` provider names none, so the default
`qwen` model needs no credentials anywhere.

## When a thread gets too long

Nothing is summarized, nothing is dropped, and no context window is
configured anywhere. When a conversation no longer fits, the server refuses
the request and OneLoop tells you so, naming the fix: `/clear` to start a
fresh session, or a model with a larger window.

The server is the only thing that reliably knows what fits: a llama-server
started with `-c 8192` and a hosted model with a 200k window are the same
code path, because both say so in the same place. A declared window is a
number that goes stale, guesses wrong for local servers, and has to be
maintained per model.

Summarizing a thread to keep it alive trades accuracy for length, silently
and on your behalf. `/clear` is the honest version of the same move: it is
one keystroke, it happens when you decide it should, and what you lose is
what you chose to lose.

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
