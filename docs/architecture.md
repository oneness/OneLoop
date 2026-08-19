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

There is no summarization, no memory store, and no retrieval layer. A
session is what was said, in order, until it is cleared.

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
- elisp — evaluates Emacs Lisp through `emacsclient` in the running Emacs server

`elisp` is for editor-only state that the filesystem tools cannot see: open or
unsaved buffers, visible windows, cursor positions, diagnostics, and process
output. It requires `emacsclient` and a running Emacs server. The bundled
`emacs` skill must be loaded before use; it keeps expressions bounded and
non-interactive and directs ordinary disk access back to `read`, `write`, and
`edit`. A tool timeout stops `emacsclient`, but cannot stop Lisp already
executing inside Emacs.

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
maintain. Metered per use; `ONELOOP_WEB_TOOLS=false` turns them off. A
request carrying no tools never includes them, so a background completion
cannot trigger a paid search.

## Providers and models

There are two protocols. OpenAI Chat Completions is what a local
llama-server and every hosted provider speak — they differ only by URL,
model id, and whether a key is required, so `providers/chat.rs` serves all
of them. `providers/codex.rs` speaks the Responses API to ChatGPT's Codex
backend, which is the one way into a ChatGPT subscription. A provider's
`api` field names which, and `Model::complete` is a two-arm `match` on it —
no client trait, because two protocols do not need a plug-in point either.

The second one is not the first renamed: the conversation is a flat list of
typed items rather than roles with attachments, tools are functions with
their fields inline, the system prompt is a field, and the endpoint only
streams. Nothing renders as it streams — the rest of OneLoop expects a
finished turn — but the stream still has to be read event by event rather
than swallowed whole, for two reasons the endpoint does not advertise. The
turn's content arrives as `response.output_item.done`, one event per item;
the `response.completed` that closes it carries an empty `output`, so a
client that reads only the closing event gets an empty answer. And the
connection stays open after that event, so reading the body to its end waits
for a close that is unrelated to a turn that finished seconds earlier.
`read_turn` therefore accumulates items, stops on the event that closes the
turn, and drops the connection there. An endpoint that instead repeats every
item in the closing event is handled by preferring whatever that event
carries, so neither spelling doubles up.

Network waits are bounded according to the protocol. Every provider client
gets 10 seconds to connect. Chat Completions is a finite body and gets a
15-minute overall deadline. A Codex stream has no overall deadline while it
is progressing, but must begin responding within 90 seconds and each later
chunk resets the same 90-second idle deadline. That avoids killing a long,
active generation while still closing a connection that has stalled.

A provider is a place: base URL, what it is let in with, and one HTTP
client. A model is one thing that place will run, carrying a short alias
used everywhere else. Every `Model` holds an `Arc<Provider>`, so two models
from the same provider share one connection pool, one credential, one
URL — and a model can always say where it is sent, which is what `/model`
and the error messages show. Aliases must be unique across providers, or
naming one would be ambiguous; that is refused at load.

Config is `~/.oneloop/config.json`, written from `src/default-config.json`
on first run. It holds no secrets — a provider names an environment
variable, and keys live in `auth.json`. The default model is `qwen`, served
by the credential-free `local` provider, so an unconfigured checkout cannot
accidentally bill a hosted model.

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

## Context that no longer fits

Nothing is summarized and nothing is silently dropped. A conversation grows
until the server refuses it, and the refusal is reported — with the fix
named, because "provider error: 400" does not suggest `/clear`.

`is_context_overflow` in `providers.rs` classifies a 400/413 by the phrases
every server words differently ("context length", "prompt is too long",
"context size"), beside `is_retryable` and answering the same kind of
question about the same error. It decides what the message says, not what
the loop does: an overflow ends the turn like any other refusal.

No per-model `context_window` is declared, no threshold percentage is tuned,
and no character-per-token estimate appears in a branch. The server already
knows what fits and says so. `estimate_tokens` survives only to size the
`tokens_estimated` metric, where being approximate is harmless, which is why
it lives in `metrics.rs`.

Summarizing a thread to keep it alive trades accuracy for length without
being asked. `/clear` is the same move made deliberately: it costs one
keystroke, it happens when the user decides, and what is lost is what they
chose to lose. An unrecognised refusal phrasing costs nothing extra — it
stays an ordinary reported error.

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

`auth.json` is one entry per provider, filed under the name that provider
has in the config, and each entry says what kind it is:

```json
{
  "openrouter":   { "type": "api_key", "key": "..." },
  "openai":       { "type": "oauth", "access": "...", "refresh": "...",
                    "expires": 1760000000, "account_id": "..." }
}
```

The `type` decides how an entry is read, so a provider that changes how it
lets callers in is a different entry rather than a different field, and
nothing in the file's shape is specific to one vendor. API keys are resolved
from the environment first (`api_key_env`, blank values ignored) and only
then from here — setting a variable is the caller saying "use this".
`auth.json` is written with owner-only (0600) permissions and is the only
file holding secrets.

Entries are held as raw JSON and typed only on the way in and out. This
version is not the only thing that has ever written the file — an older
OneLoop wrote entries with no `type` at all — and the file is rewritten
whenever a token is refreshed, so an entry dropped on a read would be
deleted on the next write. One it cannot read is one it leaves alone.

A subscription has no key to name. `ol login openai` runs the
authorization-code flow with PKCE (`auth/codex.rs`): a challenge in the
authorize URL, a one-request web server on the port the redirect names, and
an exchange for tokens that expire — the Codex CLI's own client id and
endpoints, because the grant is issued to that client. The `state` parameter
is checked on the way back, or any page the browser can reach could hand
that server a code of its choosing. The callback wait ends after 5 minutes
(or on Ctrl+C), token exchange and renewal get 30 seconds overall, and their
HTTP connection gets 10 seconds to establish.

What comes back is stored as an ordinary entry, and renewed a minute before
it expires by whichever request notices first — behind a lock, because the
refresh token is single-use and two models sharing the provider would
otherwise spend it twice. A rotation that cannot be written back is a
warning, not a failure: the token in hand still works, and only the next run
pays for it. A provider configured but never signed in to is `Missing`
rather than an error at startup, so it refuses with the command that fixes
it instead of a 401 later — and models that need no sign-in still load.

## Output

`output.rs` owns both the ANSI escapes and the shape of a status line, so a
colour change, a `--no-color` flag, or `NO_COLOR` support is one edit rather
than thirty. Callers pick a meaning — `fail`, `warn`, `ok`, `tick`, `step`,
`note`, `head` — and the module decides the glyph, the colour, and the stream.

The streams are split by what the text *is*, not by how bad it is:

- **stdout** carries the model's answer, and nothing else.
- **stderr** carries everything OneLoop says about itself — the tool trace,
  the model banner, the picker, warnings, and errors.

So `ol "..." > answer.md` captures the answer alone while the trace stays on
the terminal, and a failing run writes nothing to stdout at all. Two shapes
stay outside the module by design: the spinner's cursor manipulation, and the
column-aligned model picker, which no general helper could serve without
becoming one caller's shape.

## Source layout

```
src/
  main.rs           CLI entry point, login command
  agent.rs          Agent struct, run_once, execute_tool_calls, session repair
  agent/
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    messages.rs     Message types (User, Assistant, ToolCall, ToolResult)
    session.rs      Session persistence, rotation, dangling-tool-call repair
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec), token estimation
  app.rs            Interactive REPL (rustyline), /commands, Ctrl+C handling
  auth.rs           Credential resolution (env over ~/.oneloop/auth.json) and storage
  auth/
    codex.rs        ChatGPT sign-in: PKCE, the callback server, token refresh
  catalog.rs        ~/.oneloop/config.json: providers and their models, validation, active model
  config.rs         System prompt assembly (tool preamble + AGENTS.md), env_or
  models.rs         Model (alias + settings + its provider), registry, active-model switching
  models/
    retry.rs        Retry a request, then offer another model when one won't answer
  output.rs         Status lines (glyph, colour, stream), output truncation, ANSI constants
  providers.rs      Provider (endpoint: URL, credentials, one HTTP client), request/response types
  providers/
    chat.rs         The Chat Completions wire format
    codex.rs        The Responses wire format, as ChatGPT's Codex backend speaks it
  tools.rs          Tool trait, ToolRegistry (Arc<dyn Tool>), ToolDefinition
  tools/
    bash.rs         Shell command execution
    elisp.rs        Emacs Lisp evaluation through a running Emacs server
    read.rs         File reading
    write.rs        File writing
    edit.rs         Find-and-replace file editing
    skill.rs        On-demand skill loader (scans .oneloop/skills/ and ~/.oneloop/skills/)
docs/
  architecture.md   This file
  index.html        Executive presentation (GitHub Pages, space-bar nav)
  style-guide.md    Coding conventions and lint config
```
