# oneloop

A tiny, extensible coding agent.

## Philosophy

- tiny functional core
- one clear agent loop
- a few durable primitives
- everything else built on top
- terminal first
- easy to shape to a workflow

## Initial scope

oneloop starts small:

- one provider
- four tools: read, write, edit, bash
- linear session model
- AGENTS.md context loading
- interactive CLI first

Everything else is a later layer:

- RPC mode
- prompt templates
- skills
- plugins
- session branching
- compaction

## Development

```bash
nix develop
cargo check
```

## One-loop iteration

Use the helper script:

```bash
./loop "your prompt here"
```

`./loop` starts from a fresh session each time by deleting `.oneloop/` before running.
That keeps the iteration loop tight and avoids stale session state while developing the agent.

`./loop` is purely agent-driven: you talk to the agent in natural language, and the model decides whether to use `read`, `write`, `edit`, or `bash`.

## Current behavior

- prompts are persisted to `.oneloop/session.jsonl`
- assistant responses are persisted to `.oneloop/session.jsonl`
- tool calls and tool results are persisted to `.oneloop/session.jsonl`
- `read`, `write`, `edit`, and `bash` are available as built-in tools the model can choose to use
- `read` and `bash` truncate large output before it goes back into the model context
- for normal prompts, the provider can return tool calls and oneloop will execute them in a loop
- `AGENTS.md` in the current project directory is loaded as the system prompt
- `oneloop login zai` stores a Z.AI API key in `~/.oneloop/auth.json`
- `oneloop login anthropic` stores an Anthropic API key in `~/.oneloop/auth.json`
- if Z.AI credentials are available, oneloop prefers Z.AI
- otherwise if Anthropic credentials are available, oneloop uses Anthropic
- otherwise it falls back to the mock provider
- you can override provider selection with `ONELOOP_PROVIDER=zai|anthropic|mock`
- Z.AI defaults to the coding endpoint: `https://api.z.ai/api/coding/paas/v4`

## Important note on Anthropic login

oneloop does **not** implement `claude.ai` subscription login.
Anthropic's official docs state that third-party developers are not allowed to offer `claude.ai` login for their own products unless specially approved. So oneloop currently supports Anthropic API-key auth only.
