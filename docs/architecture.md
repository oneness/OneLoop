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
completion calls (synthesis, compaction, memory extraction) never include
them, so background work cannot trigger paid searches.

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
and the error messages show. Aliases must be unique across providers, or a
directive naming one would be ambiguous; that is refused at load.

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
atomic because the registry is shared with orchestration tasks while
`/model` can change it.

Setting a model, narrowest scope first: `model:` sends a one-off wire id for
one prompt; `#!alias` routes one prompt; `/model` switches the session;
`ONELOOP_MODEL` sets a run; `default` in the config sets every run. Only the
last is persistent, and only it is edited by hand — a session-scoped command
that rewrote the config would make the next run's model a side effect.

## Multi-model orchestration

`#!consensus` and `#!debate` ask several models the same question and have a
judge synthesize the answers — `#!consensus local flash#!` puts a local model
against a hosted one, and several models from one provider cost nothing extra
to configure. Orchestrated models never get direct tool access: they see a
single `request_evidence` tool and ask the main agent, which executes,
caches, and shares results across all of them in the run.

The evidence tools (`read`, `shell`) are defined in one table
(`EVIDENCE_TOOLS` in `evidence.rs`): the allowlist, the `request_evidence`
schema, execution dispatch, display formatting, and directive validation are
all derived from it. Adding or renaming an evidence tool is one entry there.
`shell` is backed by the `bash` tool behind a read-only command guardrail —
a seatbelt against state-changing commands, not a security boundary.
Orchestrated models reached via OpenRouter also get the server-side web
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

A provider names the variable holding its key via `api_key_env`, or omits it
for a server that needs none. `auth.json` is written with owner-only (0600)
permissions and is the only file holding secrets.

## Source layout

```
src/
  main.rs           CLI entry point, login command
  agent.rs          Agent struct, run_once_with, execute_tool_calls, session repair
  agent/
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    orchestration.rs Consensus, debate, per-model evidence loops
    messages.rs     Message types (User, Assistant, ToolCall, ToolResult)
    session.rs      Session persistence, rotation, dangling-tool-call repair
    compaction.rs   Auto-compaction, token estimation, memory extraction
    evidence.rs     Evidence-tool table (single source of truth), cache, shell guardrail
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec, compaction)
  app.rs            Interactive REPL (rustyline), /commands, directive dispatch, Ctrl+C handling
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
