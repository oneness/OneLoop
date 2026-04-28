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

- multiple providers (Z.AI, OpenAI, Anthropic)
- five tools: read, write, edit, bash, web_search
- linear append-only session model with `/clear` to rotate
- AGENTS.md context loading
- interactive CLI with animated spinner
- date-based session persistence
- auto-compaction when context nears limit
- retry with interactive fallback to alternative providers
- parallel tool execution
- per-session metrics logging

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

`./ol` is a thin wrapper that runs oneloop via `nix develop`. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, `bash`, or `web_search`.

## Current behavior

- sessions are persisted as JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- `/clear` rotates to a new session file (e.g. `YYYY-MM-DD-001.jsonl`, `YYYY-MM-DD-002.jsonl`); old sessions are preserved on disk
- on restart, the latest session file for today is opened automatically
- an animated braille spinner shows progress while thinking and during tool execution
- tool results show ✓/✗ with line and byte counts
- `read` and `bash` truncate large output before it goes back into the model context
- `AGENTS.md` in the current project directory is loaded as the system prompt
- `@provider` prefix routes to a specific provider (e.g. `@anthropic explain this code`)
- `oneloop login <provider>` stores API keys in `~/.oneloop/auth.json`
- multiple tool calls from a single response execute in parallel
- auto-compaction triggers at 85% of context window with a structured handoff summary
- on provider failure, retries up to 3 times then prompts to pick an alternative provider
- per-session metrics logged to `.oneloop/metrics/<session>.jsonl`

## Provider selection

Default order:

1. Z.AI (if credentials available)
2. OpenAI (if credentials available)
3. Anthropic (if credentials available)

Override with environment variables:

- `ONELOOP_PROVIDER=zai|openai|anthropic` — force a specific provider
- `ONELOOP_ANTHROPIC_MODEL` — Anthropic model override (defaults to `claude-sonnet-4-6`)
- `ONELOOP_OPENAI_MODEL` — OpenAI model override (defaults to `gpt-5.4`)
- `ONELOOP_OPENAI_BASE_URL` — OpenAI base URL override (defaults to `https://api.openai.com/v1`)
- `ONELOOP_OPENAI_REASONING_EFFORT` — reasoning effort for o-series models (`low`, `medium`, `high`; defaults to `medium`)
- `ONELOOP_ZAI_MODEL` — Z.AI model override (defaults to `glm-5.1`)
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
