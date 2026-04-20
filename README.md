# oneloop

A tiny, extensible coding agent.

## Philosophy

- tiny functional core
- one clear agent loop
- a few durable primitives
- everything else built on top
- terminal first
- easy to shape to a workflow

## Scope

oneloop starts small:

- multiple providers (Z.AI, OpenAI, Anthropic, mock fallback)
- four tools: read, write, edit, bash
- linear append-only session model
- AGENTS.md context loading
- interactive CLI with animated spinner
- date-based session persistence

Everything else is a later layer:

- RPC mode
- prompt templates
- skills
- plugins
- session branching
- compaction

## Usage

### Interactive mode

```bash
./ol
```

Starts an interactive REPL. Type your message and press Enter. Ctrl+D to exit.

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

`./ol` is a thin wrapper that runs oneloop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, or `bash`.

## Current behavior

- sessions are persisted as JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- an animated braille spinner shows progress while thinking and during tool execution
- tool results show ✓/✗ with line and byte counts
- `read` and `bash` truncate large output before it goes back into the model context
- `AGENTS.md` in the current project directory is loaded as the system prompt
- `oneloop login zai` stores a Z.AI API key in `~/.oneloop/auth.json`
- `oneloop login openai` stores an OpenAI API key in `~/.oneloop/auth.json`
- `oneloop login anthropic` stores an Anthropic API key in `~/.oneloop/auth.json`

## Provider selection

Default order:

1. Z.AI (if credentials available)
2. OpenAI (if credentials available)
3. Anthropic (if credentials available)
4. mock fallback

Override with environment variables:

- `ONELOOP_PROVIDER=zai|openai|anthropic|mock` — force a specific provider
- `ONELOOP_ANTHROPIC_MODEL` — Anthropic model override (defaults to `claude-sonnet-4-5`)
- `ONELOOP_OPENAI_MODEL` — OpenAI model override (defaults to `o3`)
- `ONELOOP_OPENAI_BASE_URL` — OpenAI base URL override (defaults to `https://api.openai.com/v1`)
- `ONELOOP_OPENAI_REASONING_EFFORT` — reasoning effort for o-series models (`low`, `medium`, `high`; defaults to `medium`)
- `ONELOOP_ZAI_BASE_URL` — Z.AI base URL override (defaults to `https://api.z.ai/api/coding/paas/v4`)

Credentials are resolved from `~/.oneloop/auth.json` first, then from environment variables (`ZAI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`).

## Important note on Anthropic login

oneloop does **not** implement `claude.ai` subscription login.
Anthropic's official docs state that third-party developers are not allowed to offer `claude.ai` login for their own products unless specially approved. So oneloop currently supports Anthropic API-key auth only.

## Development

```bash
nix develop
cargo check
```
