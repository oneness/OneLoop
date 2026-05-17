# OneLoop

A tiny coding agent. One loop, multiple models, five tools, zero config.

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
./ol login zai
./ol login openai
./ol login anthropic
```

Stores API keys in `~/.oneloop/auth.json`.

`./ol` is a thin wrapper that runs OneLoop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, `bash`, or `web_search`.

## Directives

Directives use `#!directive words#!` followed by the user message:

- `#!anthropic#! explain this file` — route to Anthropic
- `#!anthropic openai#! should we do X?` — consensus (2+ providers defaults to consensus)
- `#!consensus anthropic openai zai judge:openai#! question` — explicit consensus with judge
- `#!debate anthropic openai rounds:2 judge:anthropic#! question` — debate with 2 rounds
- `#!anthropic format:md#! summarize` — single provider with markdown output

Tokens between `#!...#!` are space-separated: provider names, mode keywords
(`consensus`, `debate`), and key:value modifiers (`judge:openai`, `rounds:2`,
`tools:none`, `format:md`, `format:html`). No `#!` at all means plain prompt
with default provider.

## Provider selection

Default order:

1. Z.AI (if credentials available)
2. OpenAI (if credentials available)
3. Anthropic (if credentials available)

Override with environment variables:

- `ONELOOP_PROVIDER=zai|openai|anthropic` — force a specific provider
- `ONELOOP_ANTHROPIC_MODEL` — Anthropic model override
- `ONELOOP_OPENAI_MODEL` — OpenAI model override
- `ONELOOP_OPENAI_BASE_URL` — OpenAI base URL override
- `ONELOOP_OPENAI_REASONING_EFFORT` — reasoning effort for o-series models
- `ONELOOP_ZAI_MODEL` — Z.AI model override
- `ONELOOP_ZAI_BASE_URL` — Z.AI base URL override

Credentials are resolved from `~/.oneloop/auth.json` first, then from environment variables (`ZAI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

## Development

```bash
nix develop
cargo check
```
