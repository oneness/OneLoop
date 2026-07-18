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
./ol login openai
./ol login anthropic
```

Stores API keys in `~/.oneloop/auth.json`.

`./ol` is a thin wrapper that runs OneLoop via `nix develop` and pins the per-provider models. The agent is purely model-driven: you talk to it in natural language, and the model decides whether to use `read`, `write`, `edit`, `bash`, or `skill` (when skill files exist under `.oneloop/skills/`). Web search and page fetching are not OneLoop tools: on OpenRouter, the agent enables the server-side `openrouter:web_search` and `openrouter:web_fetch` tools, which the model invokes when it needs the web and OpenRouter executes itself (metered per use; disable with `ONELOOP_WEB_TOOLS=false`).

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
single-provider mode; `judge:`, `rounds:`, and `tools:` require consensus or
debate mode.

## Provider selection

Default order:

1. OpenRouter (if credentials available)
2. OpenAI (if credentials available)
3. Anthropic (if credentials available)

Override with environment variables:

- `ONELOOP_PROVIDER=openrouter|openai|anthropic` — force a specific provider
- `ONELOOP_OPENROUTER_MODEL` — OpenRouter model (default: `deepseek/deepseek-v4-flash`)
- `ONELOOP_OPENROUTER_BASE_URL` — OpenRouter base URL override
- `ONELOOP_WEB_TOOLS` — OpenRouter server-side web search/fetch tools (default: `true`)
- `ONELOOP_ANTHROPIC_MODEL` — Anthropic model override
- `ONELOOP_ANTHROPIC_MAX_TOKENS` — Anthropic output-token cap per response (default: `16000`)
- `ONELOOP_OPENAI_MODEL` — OpenAI model override
- `ONELOOP_OPENAI_BASE_URL` — OpenAI base URL override
- `ONELOOP_OPENAI_REASONING_EFFORT` — reasoning effort (default: `medium`; `none` omits the parameter for models that reject it)

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
