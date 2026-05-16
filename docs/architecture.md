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

## The loop

1. accept user input
2. build request from system prompt + session + input
3. call provider
4. store assistant output
5. if tool calls are returned, persist them
6. execute tools
7. store tool results
8. continue until the provider stops returning tool calls

## Built-in tools

- read
- write
- edit
- bash
- web_search (SearXNG-backed)

All five core built-in tools are now implemented.

## Providers

Currently supported:

- Z.AI via API key
- OpenAI via API key
- Anthropic via API key

Default selection order (when `ONELOOP_PROVIDER` is not set):

1. Z.AI
2. OpenAI
3. Anthropic

Override with `ONELOOP_PROVIDER` if needed. Route per-prompt with `#!provider` directives.
Use `#!consensus` or `#!debate` to ask multiple providers and synthesize a final answer.

## Sessions

Sessions are linear append-only JSONL files stored at:

```
.oneloop/sessions/YYYY-MM-DD.jsonl
```

`/clear` rotates to a new file (`YYYY-MM-DD-001.jsonl`, `YYYY-MM-DD-002.jsonl`, etc.).
Old sessions are preserved on disk — never deleted.
On restart, the latest session file for today is opened automatically.

## Auth

Credentials are resolved from `~/.oneloop/auth.json` first, then environment variables.
Currently supported environment variables:

- `ZAI_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`

Anthropic API-key auth is supported, but not `claude.ai` subscription login.

## Source layout

```
src/
  main.rs           CLI entry point, login command
  agent.rs          Agent struct, run_once_with, auto_compact_if_needed
  agent/
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    orchestration.rs Consensus, debate, multi-provider evidence loops
    messages.rs     Message types (User, Assistant, ToolCall, ToolResult)
    session.rs      Session persistence, rotation, file discovery
    compaction.rs   Token estimation, tool output stripping, compaction
    evidence.rs     Evidence cache, safety checks, tool execution
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec, compaction)
  app.rs            Interactive REPL, command routing, Ctrl+C handling
  auth.rs           API key storage in ~/.oneloop/auth.json
  config.rs         System prompt loading from AGENTS.md
  output.rs         Output truncation utilities
  providers.rs      Provider trait, ProviderRequest/Response types
  providers/
    anthropic.rs    Anthropic Claude provider
    openai.rs       OpenAI GPT provider
    zai.rs          Z.AI GLM provider
    registry.rs     Provider discovery, selection, retry with fallback
  tools.rs          Tool trait, ToolRegistry (Arc<dyn Tool>), ToolDefinition
  tools/
    bash.rs         Shell command execution
    read.rs         File reading
    write.rs        File writing
    edit.rs         Find-and-replace file editing
    web_search.rs   SearXNG web search
docs/
  architecture.md   This file
  overview.html     Executive presentation (browser, space-bar nav)
  style-guide.md    Coding conventions and lint config
```
