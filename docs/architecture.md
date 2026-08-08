# OneLoop architecture

OneLoop is local-first: the default model runs on the same machine, needs no
credentials, and sends nothing off the box. Hosted models are reachable, but
only when named.

## Core

The initial core is intentionally small:

- agent loop
- session/messages
- providers and the models they host
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

`skill` is registered alongside them when `.oneloop/skills/*.md` files
exist, but is not a tool in the same sense: it touches nothing and returns a
markdown playbook for the model to follow. It is on-demand prompt
engineering wearing a tool call, which is the only way to let the model
decide when it needs one.

Web search and page fetching are deliberately not built-in tools. On
OpenRouter, requests that carry tools also enable the server-side
`openrouter:web_search` and `openrouter:web_fetch` tools: the model decides
when to use them, OpenRouter executes them, and the results arrive inside the
assistant message — no client-side handling, no HTML sanitization to
maintain. Metered per use; `ONELOOP_WEB_TOOLS=false` turns them off. Plain
completion calls (compaction, memory extraction) never include them, so
background work cannot trigger paid searches.

## Providers and models

There is one protocol: OpenAI Chat Completions. A local llama-server and a
hosted model differ only by URL, model id, and whether a key is required, so
`providers/chat.rs` serves both. A second protocol would be a second module
beside it and a `match` in `Model::complete` — there is no client trait to
implement, because one protocol does not need a plug-in point.

A provider is a place: base URL, the environment variable naming its key,
and one HTTP client. A model is one thing that place will run, carrying a
short alias used everywhere else. Every `Model` holds an `Arc<Provider>`, so
two models from the same provider share one connection pool, one key, one
URL — and a model can always say where it is sent, which is what `/model`
and the error messages show. Aliases must be unique across providers, or
naming one would be ambiguous; that is refused at load.

Config is `~/.oneloop/config.json`, written from `src/default-config.json`
on first run. It holds no secrets — a provider names an environment
variable, and keys live in `auth.json`. The default model is `local`, which
needs no credentials, so an unconfigured checkout cannot accidentally bill a
hosted model.

Which model is active is decided once, in `catalog.rs`: the file's `default`
unless `ONELOOP_MODEL` names another. The per-run environment overrides then
apply to whatever that resolved to. `catalog.rs` keeps the config's nesting
rather than flattening it — providers, each with the models it hosts — and
`models.rs` makes that live, then flattens only the lookup: everything
downstream asks for a model by alias. It owns the one thing that moves:
which model a request goes to when nothing names another. That index is
atomic because the registry is shared with the retry path, which can fall
back to another model, while `/model` can change it.

Setting a model, narrowest scope first: `/model` switches the session;
`ONELOOP_MODEL` sets a run; `default` in the config sets every run. Only the
last is persistent, and only it is edited by hand — a session-scoped command
that rewrote the config would make the next run's model a side effect.

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

## Compaction

Compaction is never scheduled and never predicted. It runs when the user types `/compact`, or when a provider rejects a request for not fitting — `is_context_overflow` in `providers.rs` classifies a 400/413 by the phrases every server words differently ("context length", "prompt is too long", "context size"), beside `is_retryable` and answering the same kind of question about the same error.

The agent loop owns the reaction, because the session is what has to change: `run_once` catches the refusal, compacts, and retries the same iteration. Once per prompt — a second refusal means the summary did not help, and summarizing a summary is how a loop that never terminates begins.

Deciding at the point of refusal rather than ahead of it is what removes the machinery: no per-model `context_window` to declare, no threshold percentage to tune, no character-per-token estimate in a branch. The server already knows what fits and says so. `estimate_tokens` survives only to size the `tokens_estimated` metric, where being approximate is harmless.

The consequence is one wasted request per overflow. That is cheap — a rejected request is not billed by hosted providers, and a local server refuses after tokenizing rather than after generating.

Compacting summarizes the thread through a plain completion call (tool outputs stripped to short notes first, so the summary request is not itself oversized), extracts memory, rotates to a fresh session file, then replays recent user messages verbatim followed by the summary. An unrecognised refusal phrasing costs nothing new: it stays an ordinary reported error, which is what it was before.

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

A provider names the variable holding its key via `api_key_env`, or omits it
for a server that needs none. `auth.json` is written with owner-only (0600)
permissions and is the only file holding secrets.

## Source layout

```
src/
  main.rs           CLI entry point, login command
  agent.rs          Agent struct, run_once, execute_tool_calls, session repair
  agent/
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    messages.rs     Message types (User, Assistant, ToolCall, ToolResult)
    session.rs      Session persistence, rotation, dangling-tool-call repair
    compaction.rs   Summarize-and-reseed, token estimation, memory extraction
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec, compaction)
  app.rs            Interactive REPL (rustyline), /commands, Ctrl+C handling
  auth.rs           API key resolution (env over ~/.oneloop/auth.json) and storage
  catalog.rs        ~/.oneloop/config.json: providers and their models, validation, active model
  config.rs         System prompt assembly (tool preamble + AGENTS.md + memory), env_or
  models.rs         Model (alias + settings + its provider), registry, active-model switching
  models/
    retry.rs        Retry a request, then offer another model when one won't answer
  output.rs         Output truncation utilities, ANSI style constants
  providers.rs      Provider (endpoint: URL, key, one HTTP client), request/response types
  providers/
    chat.rs         The Chat Completions wire format — the one protocol
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
