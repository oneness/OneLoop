# OneLoop

A tiny coding agent. One loop, multiple models, six tools, zero config.

## Quick links

- **[Overview](docs/overview.html)** — executive presentation (open in browser, space-bar to navigate)
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

### Login

```bash
./ol login openrouter
./ol login openai
./ol login anthropic
```

Stores API keys in `~/.oneloop/auth.json`.

`./ol` is a thin wrapper that runs OneLoop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, `bash`, or `web_search`.

## Directives

Directives use `#!directive words#!` followed by the user message:

- `#!openrouter#! explain this file` — route to OpenRouter
- `#!openrouter model:deepseek/deepseek-v3-0324#! refactor this` — specific model
- `#!model:anthropic/claude-opus-4#! hard problem` — model override, default provider
- `#!anthropic openai#! should we do X?` — consensus (2+ providers defaults to consensus)
- `#!consensus anthropic openai judge:openai#! question` — explicit consensus with judge
- `#!debate anthropic openai rounds:2 judge:anthropic#! question` — debate with 2 rounds
- `#!anthropic format:md#! summarize` — single provider with markdown output

Tokens between `#!...#!` are space-separated: provider names, mode keywords
(`consensus`, `debate`), and key:value modifiers (`model:provider/name`,
`judge:openai`, `rounds:2`, `tools:none`, `format:md`, `format:html`). No `#!`
at all means plain prompt with default provider. `model:` is only valid in
single-provider mode.

## Provider selection

Default order:

1. OpenRouter (if credentials available)
2. OpenAI (if credentials available)
3. Anthropic (if credentials available)

Override with environment variables:

- `ONELOOP_PROVIDER=openrouter|openai|anthropic` — force a specific provider
- `ONELOOP_OPENROUTER_MODEL` — OpenRouter model (default: `anthropic/claude-sonnet-4-5`)
- `ONELOOP_OPENROUTER_BASE_URL` — OpenRouter base URL override
- `ONELOOP_ANTHROPIC_MODEL` — Anthropic model override
- `ONELOOP_OPENAI_MODEL` — OpenAI model override
- `ONELOOP_OPENAI_BASE_URL` — OpenAI base URL override
- `ONELOOP_OPENAI_REASONING_EFFORT` — reasoning effort for o-series models

Credentials are resolved from `~/.oneloop/auth.json` first, then from environment variables (`OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

## Development

```bash
nix develop
cargo check
```
