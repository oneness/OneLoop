# OneLoop architecture

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
- skill (registered only when skill files exist)

Web search and page fetching are deliberately not built-in tools. On
OpenRouter, requests that carry tools also enable the server-side
`openrouter:web_search` and `openrouter:web_fetch` tools: the model decides
when to use them, OpenRouter executes them, and the results arrive inside the
assistant message — no client-side handling, no HTML sanitization to
maintain. Metered per use; `ONELOOP_WEB_TOOLS=false` turns them off. Plain
completion calls (synthesis, compaction, memory extraction) never include
them, so background work cannot trigger paid searches.

## Providers

Currently supported:

- OpenRouter via API key (default — access to any model on the OpenRouter catalogue)
- OpenAI via API key
- Anthropic via API key

Default selection order (when `ONELOOP_PROVIDER` is not set):

1. OpenRouter
2. OpenAI
3. Anthropic

Override with `ONELOOP_PROVIDER` if needed. Route per-prompt with `#!provider` directives.
Use `#!consensus` or `#!debate` to ask multiple providers and synthesize a final answer.
Use `model:` in a single-provider directive to override the model for that prompt.

## Multi-model orchestration

`#!consensus` and `#!debate` ask several providers the same question and have a
judge synthesize the answers. Orchestrated providers never get direct tool
access — they see a single `request_evidence` tool and ask the main agent,
which executes, caches, and shares results across all providers in the run.

The evidence tools (`read`, `shell`) are defined in one table
(`EVIDENCE_TOOLS` in `evidence.rs`): the allowlist, the `request_evidence`
schema, execution dispatch, display formatting, and directive validation are
all derived from it. Adding or renaming an evidence tool is one entry there.
`shell` is backed by the `bash` tool behind a read-only command guardrail —
a seatbelt against state-changing commands, not a security boundary.
Orchestrated providers reached via OpenRouter also get the server-side web
tools, since their requests carry the `request_evidence` tool.

## Skills

Skill files are markdown files that contain task-specific instructions the agent loads on demand. They are not in the system prompt at startup — instead, the `skill` tool lists them by name and description so the model can pull one in when relevant.

Scan order (project-local shadows global for the same name):
1. `~/.oneloop/skills/*.md` — global, shared across all projects
2. `.oneloop/skills/*.md` — project-local

The first non-empty, non-heading line of each file is used as the skill's description in the tool listing. The full file content is returned as the tool result when the model calls `skill("name")`.

If no skill files are found at startup, the `skill` tool is not registered.

## Memory

`.oneloop/memory.md` is a plain markdown file of bullet-point facts the agent accumulates across sessions. It is loaded at startup and appended to the system prompt under a `## Memory` heading, alongside `AGENTS.md`.

Memory is updated automatically at compaction time via a second, cheap LLM call that receives only the compaction summary (not the full context) and extracts durable facts — user preferences, project decisions, recurring constraints. The response is appended to `memory.md`; the file is trimmed to 200 lines oldest-first if it grows past that.

The file is human-readable and hand-editable. Delete lines to forget things, add lines to seed memory before the first compaction.

## Sessions

Sessions are linear append-only JSONL files stored at:

```
.oneloop/sessions/YYYY-MM-DD.jsonl
```

`/clear` rotates to a new file (`YYYY-MM-DD-001.jsonl`, `YYYY-MM-DD-002.jsonl`, etc.).
Old sessions are preserved on disk — never deleted.
On restart, the latest session file for today is opened automatically.

On open (and after a Ctrl+C-cancelled run), any tool call left without a matching
result is closed out with a synthetic error result — providers reject conversations
containing dangling tool calls, so an unrepaired session would break every later request.

## Auth

Credentials are resolved from environment variables first, then `~/.oneloop/auth.json` —
an explicitly set env var always wins (blank values are ignored). Supported variables:

- `OPENROUTER_API_KEY`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`

`auth.json` is written with owner-only (0600) permissions.
Anthropic API-key auth is supported, but not `claude.ai` subscription login.

## Source layout

```
src/
  main.rs           CLI entry point, login command
  agent.rs          Agent struct, run_once_with, execute_tool_calls, session repair
  agent/
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    orchestration.rs Consensus, debate, per-provider evidence loops
    messages.rs     Message types (User, Assistant, ToolCall, ToolResult)
    session.rs      Session persistence, rotation, dangling-tool-call repair
    compaction.rs   Auto-compaction, token estimation, memory extraction
    evidence.rs     Evidence-tool table (single source of truth), cache, shell guardrail
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec, compaction)
  app.rs            Interactive REPL (rustyline), directive dispatch, Ctrl+C handling
  auth.rs           API key resolution (env over ~/.oneloop/auth.json) and storage
  config.rs         System prompt assembly (tool preamble + AGENTS.md + memory), env_or
  output.rs         Output truncation utilities, ANSI style constants
  providers.rs      Provider trait, request/response types, shared HTTP send/read
  providers/
    anthropic.rs    Anthropic Claude provider
    openai.rs       OpenAI GPT provider (Responses API)
    openrouter.rs   OpenRouter provider (Chat Completions API, server-side web tools)
    registry.rs     Provider discovery, selection, retry with interactive fallback
  tools.rs          Tool trait, ToolRegistry (Arc<dyn Tool>), ToolDefinition
  tools/
    bash.rs         Shell command execution
    read.rs         File reading
    write.rs        File writing
    edit.rs         Find-and-replace file editing
    skill.rs        On-demand skill loader (scans .oneloop/skills/ and ~/.oneloop/skills/)
docs/
  architecture.md   This file
  index.html        Executive presentation (GitHub Pages, space-bar nav)
  style-guide.md    Coding conventions and lint config
```
