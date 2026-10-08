# OneLoop

A local-first coding agent. It runs against a model on your own machine by
default — no API key, no account, nothing leaving the box — and reaches a
hosted model only when you ask it to. One loop, few tools, zero config.

## Quick links

- **[Overview](https://www.birkey.co/oneloop/)** — executive presentation (space-bar to navigate)
- **[Architecture](docs/architecture.md)** — how the agent loop, providers, tools, and sessions work
- **[Style guide](docs/style-guide.md)** — coding conventions and lint config

## Usage

### Interactive mode

```bash
./ol
```

Starts an interactive REPL. Type your message and press Enter. The startup
banner shows a short `/help` hint instead of the full command list.
`/help` shows the current model, available models, tools, session file and
message count, and loaded instruction sources, followed by commands and
keyboard shortcuts. The context line currently lists `AGENTS.md` when loaded;
it does not list every file read during the conversation.

Commands (also available with `/help`):
- `/help` — show current session information, commands, and keyboard shortcuts
- `/model` — list the configured models and switch to one
- `/model <alias>` — switch straight to that model
- `/reload` — reload `~/.oneloop/config.json` without restarting
- `/clear` — wipe context and start a fresh session
- `/cc [focus]` — critique the discussion, optionally focusing on an aspect
- `/ca <question>` — ask about the discussion, including brainstorming
- `/cn <prompt>` — send only that prompt, with no discussion history
- `Ctrl+C` — stop a running request, or discard the draft at the terminal prompt

The prompt shows the model and, when the model declares a `context_window`
in the config, roughly how much of it is still free — `(qwen ~ 80%)> `. The
percentage is an estimate; the server still refuses the request when the
conversation truly no longer fits.

### Ask Claude about the discussion

Ask Claude to review, brainstorm, answer questions, or implement changes,
without switching your current model:

```text
/cc
/cc the retry logic
/ca Brainstorm simpler alternatives to the retry logic.
/cn Explain the tradeoffs between SQLite and PostgreSQL.
```

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
comes back; the `chatgpt` model then runs against the subscription rather than a
metered API key — the same account and quota the Codex CLI uses. The access
token is renewed automatically as it expires, so this is a one-time step.

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
        "chatgpt": { "id": "gpt-5.6-sol", "reasoning_effort": "medium" }
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

### Running the local server

The `local` provider expects an OpenAI-compatible server on port 8080. This
flake builds and runs one:

```bash
./ols
```

With no model present it offers to download one (~20 GB, into `~/models/`)
and starts the server once it lands. The download resumes if interrupted,
and lands as `.part` until complete — an aborted transfer never looks like a
usable model. To use different weights:

```bash
./ols -- /path/to/other.gguf
# or: ONELOOP_LOCAL_MODEL=/path/to/other.gguf ./ols
```

The offer is only made for the default, and only with a terminal attached:
a script or CI run gets the `curl` command printed instead of a surprise
20 GB transfer.

It wraps llama.cpp's Vulkan build with flags measured against
Qwen3.6-35B-A3B — see the comments in `ols` for what each one is worth.
`ONELOOP_LOCAL_PORT` moves it off 8080.


Tuning (all optional):

- `ONELOOP_MAX_ITERATIONS` — cap on agent-loop iterations per prompt (default: `50`)
- `ONELOOP_MAX_RETRIES` — attempts before offering another model (default: `3`)


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
