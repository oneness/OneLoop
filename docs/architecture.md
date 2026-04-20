# oneloop architecture

## Core

The initial core is intentionally small:

- agent loop
- session/messages
- provider abstraction
- tool abstraction
- config loading
- auth loading

Built-in tools use the same core tool abstraction as future non-built-in tools.
That keeps the core honest without forcing a full plugin runtime too early.

## Layers on top

Not part of the initial core:

- TUI polish
- RPC mode
- prompt templates
- skills
- plugin system
- branching
- compaction

## The loop

1. accept user input
2. build request from system prompt + session + input
3. call provider
4. store assistant output
5. if tool calls are returned, persist them
6. execute tools
7. store tool results
8. continue until the provider stops returning tool calls

## First built-in tools

- read
- write
- edit
- bash

All four core built-in tools are now implemented.
The main `./loop` workflow is agent-driven: the model decides when to use them.

## Providers

Currently supported:

- Z.AI via API key
- OpenAI via API key
- Anthropic via API key
- mock fallback

Default selection order:

1. Z.AI
2. OpenAI
3. Anthropic
4. mock

Override with `ONELOOP_PROVIDER` if needed.

## Sessions

The first version uses a linear append-only JSONL session file at:

```text
.oneloop/session.jsonl
```

This is intentionally simpler than tree sessions or compaction.

## Auth

Credentials are resolved from `~/.oneloop/auth.json` first, then environment variables.
Currently supported environment variables:

- `ZAI_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`

Anthropic API-key auth is supported, but not `claude.ai` subscription login.
