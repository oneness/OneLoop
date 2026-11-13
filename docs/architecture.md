# OneLoop architecture

OneLoop is subscription-first: one agent drives the Claude, ChatGPT, Antigravity,
OpenRouter, and OpenCode subscriptions you already pay for. The default model is
still local — it runs on the same machine, needs no credentials, and sends
nothing off the box — and hosted models are reachable whenever named.

## Core

The initial core is intentionally small:

- agent loop
- session/messages
- providers and the models they host
- tool abstraction
- config loading
- auth loading

There is no memory store and no retrieval layer. A session is what was said, in
order, until it is cleared or compacted — and compaction writes a shorter
transcript, not a second one beside it.

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

Both modes run this same loop. They differ only in what is set up before it
starts: the interactive REPL opens the session file and loads project
instructions, while a one-shot loads neither.

The loop lives in `agent/turn.rs`, and it makes two decisions that are worth
naming because they are the whole of what it decides:

- `record(&ProviderResponse) -> Record` — what a response *is*, as far as
  writing it down goes. `AsIs` when it has content (keep it with its reasoning,
  print the content), `ReasoningOnly` when it has only reasoning (keep it, print
  nothing), `ToolCallsOnly` when it has only tool calls (**keep nothing** — an
  empty assistant message between a call and its results would change the
  transcript the provider reads back), and `Fallback` when it has nothing at all
  (keep and print the fallback sentence).
- `continues(&ProviderResponse) -> bool` — `!tool_calls.is_empty()`, and nothing
  else. A response with words but no calls ends the turn.

`request(&mut self, active: &mut Option<String>)` is everything between those
decisions: assembly, retry and fallback, the spinner, the `api_call` metric and
`set_active`. It returns an `Attempt` — `Answered`, `Refused`, or `Overflow` —
because the loop, not the request, decides whether a refusal is curable: a
refusal ends the turn without ending the run, and an overflow is the one the
loop can answer with a compaction.

Where the spinner is silent — comint, a pipe, anything `output::plain()` — the
trace has to speak instead, because a wait with nothing on screen cannot be told
from a hang. It speaks on one rule, applied to both a turn and a tool call: **a
start line is printed only for work that outlasts a second** (`SAY_AFTER` in
`turn.rs`). A wait that ends sooner was never in doubt, and the completion line
already names it — so printing `▸ what is running` as well left the same text on
screen twice for every quick call. The completion line, `✓ what ran · how long`,
is the one record that a call happened at all, and it is always printed.

A turn names itself once, not once per request: a turn with three tool round
trips is three requests, and `▸ computing... · ~Nk tokens in current context`
repeated per request reads as a countdown that never ends. `when_slow` is the
one helper both sites use, so the rule is stated in one place. Where the spinner
does run there is nothing to print: it is a single line rewritten in place, and
the `✓` on the way out is what names the call.

Neither `record` nor `continues` touches the terminal or the session, which is
what makes them the first part of this loop a test can reach.

There is no iteration cap. A turn ends when the model stops calling tools, or
when the provider refuses, and both of those have already said their piece. A
model that never converges cannot grind on for ever either: the transcript is
append-only and never compacted, so the conversation outgrows the window and the
server refuses with an error that says so, and a run in the REPL can always be
stopped with Ctrl+C. What has no bound is an unattended run — `ol "…"` in a
script or an editor command — which is what the wall-clock budget in `TODO.md`
is for.

## Built-in tools

- read
- write
- edit
- bash
- elisp — evaluates Emacs Lisp through `emacsclient` in the running Emacs server

`read` returns a window of a file rather than the whole thing: `offset` and
`limit` choose it, and a window that stops short ends by naming the offset that
continues it, so a long file is read in the pieces the model asks for instead of
one message the conversation pays for on every remaining turn. A read whose
bytes the conversation already holds is answered with a pointer to the earlier
copy. That check walks the transcript on every batch, so `/clear` and a resumed
session need no cache to invalidate, and a file that changed underneath cannot
produce a matching digest.

`elisp` is for editor-only state that the filesystem tools cannot see: open or
unsaved buffers, visible windows, cursor positions, diagnostics, and process
output. It requires `emacsclient` and a running Emacs server. The bundled
`emacs` skill must be loaded before use; it keeps expressions bounded and
non-interactive and directs ordinary disk access back to `read`, `write`, and
`edit`. A tool timeout stops `emacsclient`, but cannot stop Lisp already
executing inside Emacs.

`skill` is registered alongside them when a skill file exists in either
directory, but it is not a tool in the same sense: it touches nothing and
returns a markdown playbook for the model to follow. It is on-demand prompt
engineering wearing a tool call, which is the only way to let the model
decide when it needs one.

Web search and page fetching are deliberately not built-in tools. A provider,
or one model under it, opts into OpenRouter's server-side
`openrouter:web_search` and `openrouter:web_fetch` with `web_tools: true`; the
model then decides when to use them, OpenRouter executes them, and the results
arrive inside the assistant message — no client-side handling, no HTML
sanitization to maintain. It is metered per use, which is why it stays off
until something asks for it; `ONELOOP_WEB_TOOLS` overrides the active model's
setting for one run. A request carrying no tools never includes them, so a
background completion cannot trigger a paid search.

## Providers and models

There are three API families. OpenAI Chat Completions is what a local
llama-server and hosted providers speak by default; `providers/chat.rs` serves
those models. `providers/responses.rs` serves both ChatGPT's Codex backend and
other compatible Responses endpoints. A per-model API override can select
Responses for one model on an otherwise Chat provider. The Codex backend uses
OAuth and its own endpoint headers; other Responses providers use API keys as
configured. `Model::complete` dispatches on the resolved API, without a client
trait or plug-in framework.

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

Every refusal is recorded in the `errors.log` of the conversation it
interrupted, one JSON object per line: the model alias, the status, and the body
as it arrived. It sits beside the session and the metrics it explains, in the
conversation's root — `.oneloop/` — rather than in the home directory, because a
refusal belongs to the work it interrupted and one global
file merged every project into a single history. A one-shot records nowhere: it
persists nothing, so a refusal there is reported on stderr and written to no
file.
The line printed to the console is a summary,
and the reason is usually only in the body — OpenRouter answers a gated model
with a 403 whose `failed_routing_step` names the routing gate, and nothing
else surfaces that. All three shapes go through `log_provider_failure`: a
non-2xx status, a 200 carrying an `error` object instead of `choices`, and a
body that parses as neither. A log that cannot be written is a warning, never
a replacement for the error it was meant to explain.

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
atomic because both `/model` and the retry path can change it. A fallback
becomes active for the rest of the session only after it successfully answers;
if it fails, the current model stays selected. The config file is untouched.

Setting a model, narrowest scope first: `/model` switches the session;
`ONELOOP_MODEL` sets a run; `default` in the config sets every run. Only the
last is persistent, and only it is edited by hand — a session-scoped command
that rewrote the config would make the next run's model a side effect.

## When a model won't answer

`models/retry.rs` gives one model ten attempts by default (`ONELOOP_MAX_RETRIES`),
waiting 500 ms times the attempt number in between. It sits beside the registry
rather than in `providers` because the cure is a different model, which only the
registry knows about.

`is_retryable` decides whether a second attempt can plausibly differ: a 5xx, a
429, a timeout, or a transport error can, while a 400 or a 401 is the same
answer every time — a refused request fails at once. That refusal keeps its
type, so the caller can still tell an overflow from any other rejection — the
one refusal a second attempt cannot cure.

When the attempts run out, an interactive run offers the other models. A pipe
or a script gets the error instead, because nobody there can answer the picker.
The picked model gets one attempt: it answers first and is switched to second,
so it becomes the session's active model only once its response arrives, and a
fallback that fails leaves the selection where it was. The config file is never
written, and the next run starts on its `default`.

## Skills

A skill is a directory holding a `SKILL.md` — the [Agent Skills](https://agentskills.io/specification) layout, which every other coding agent reads too. That file is markdown with YAML frontmatter, and anything the skill needs — a script, a reference — sits beside it in the same directory. Skills are not in the system prompt at startup; the `skill` tool lists them by name and description instead, so the model can pull one in when relevant.

Scan order, which is also the order the model's menu lists them in:
1. `.oneloop/skills/<name>/SKILL.md` — project-local
2. `~/.oneloop/skills/<name>/SKILL.md` — global, shared across all projects, shadowed

The directory names the skill, so the menu entry, the file named in the trace and the frontmatter's own `name` cannot drift apart. The frontmatter's `description` is the line the menu shows, because that is what decides whether the model loads the skill at all; a file with no `description` falls back to its first non-empty, non-heading line so a half-written skill still says something. A flat `<name>.md` in either directory is not a skill. The full file content is returned as the tool result when the model calls `skill("name")`.

If no skill directories are found at startup, the `skill` tool is not registered.

## Context that no longer fits

Nothing is silently dropped. A conversation that outgrows its window is either
summarized (see [Compaction](#compaction)) or handed back with the fix named —
never quietly trimmed.

`is_context_overflow` in `providers.rs` classifies a 400/413, beside
`is_retryable` and answering the same kind of question about the same error. It
reads two signals, in order of how much they can be trusted: the `error.code`
the server puts on the refusal (OpenAI's `context_length_exceeded`, and the
hyphenated spellings gateways use), and — where a server sends no usable code,
which OpenRouter never does and a proxying gateway sometimes drops — the
message, against a short list of the phrases every server words differently
("context length", "prompt is too long", "context size"). A code is a closed
vocabulary, so an unrecognised one stays an ordinary refusal instead of falling
through to the prose. It classifies; it does not decide. The turn loop reads the
answer as `Attempt::Overflow` and spends its one compaction on it, because that
is the one refusal the loop can cure.

Both classifiers read that one function, and `ProviderHttpError` is the one type
a refusal arrives as however it was delivered — a non-2xx status, an `error`
object inside a 200 (OpenRouter's habit), or a `failed` event closing a stream
whose headers were already sent. The last two keep the status the response
actually carried rather than inventing a 400, so the refusal is still logged as
what happened; `is_retryable` gives them the attempts a status-carried refusal
would get, except when the classification says the body will never fit.

A model *may* declare a `context_window` in the config; the interactive
prompt then shows roughly how much of it is still free — measured from the
prompt-token count the server reported for the last turn, and from
`estimate_tokens`, the same estimate that sizes the `tokens_estimated` metric,
until a turn has run.

That estimate decides two things, and neither of them drops a message: it sizes
the summarizer's chunk budget, and after a failed compaction it says whether a
retry is worth spending. The second one is why that note says "does not appear
to fit" rather than "does not fit" — a gauge is not the server. Everything else
waits for the server, which already knows what fits and says so.

`/clear` remains the deliberate version of the same move: it costs one
keystroke, it happens when the user decides, and what is lost is what they
chose to lose. An unrecognised refusal phrasing costs nothing extra — it
stays an ordinary reported error.

## Interactive help

The startup banner shows the session summary and a short `/help` hint rather
than a full command list. `/help` is handled locally without a model request —
as is a `!` command, which runs bash for the user and records the output:
it displays the current summary (model, available models, tools, session path
and message count, and loaded instruction sources), then commands and keyboard
shortcuts. The summary is generated when requested, so it reflects session
resets and model switches.

The context line reports startup instruction sources, currently `AGENTS.md`
when successfully loaded. It is not an inventory of the prompt: the built-in
sections — tool preamble, engineering tenets, environment — are always present
and never listed. Nor is it an inventory of files read through tools; `MEMORY.md`
and other Markdown files are not automatically loaded. `/reload` reloads model
configuration, not instruction files. Ctrl+C stops a running request or discards
the draft at the terminal prompt. Editor frontends may intercept keys before
they reach OneLoop.

Those frontends can go further than intercepting. rustyline treats `TERM` of
`dumb`, `cons25`, or `emacs` as unsupported and reads whole lines instead of
editing them (`readline_direct`), which is the state comint runs in: there the
prompt has no line editing at all. Everything that needs only the finished line
still works, so `!` commands run normally.

## Bash commands at the prompt

A `!` line is the user's own bash command, not the model's: it runs in the working
directory through `tools/bash.rs::run`, the same function the `bash` tool calls,
so the transcript the terminal shows — `command`, `exit_code`, `stdout`,
`stderr` — is exactly what enters the session. That output is printed
uncoloured and unprefixed to stderr, where the rest of what OneLoop says about
itself goes, between two glyph-prefixed status lines.

Recording reuses the reference path: `Agent::record_reference` pushes a user
message without starting a turn, the same move `/cc` makes, so the model reads
the output when the user next writes to it. Nothing about it is hidden — the
message is attributed to the user in the session file and says the output is
reference material. `!` takes the rest of the line verbatim, which is why there
is no `!cmd; prompt` form: a `;` there belongs to the shell.

`!` and the `bash` tool differ in one thing worth knowing: the command runs with
no tty and no stdin, so interactive programs need a non-interactive flag
(`git --no-pager log`), and it is interruptible with `Ctrl+C` under a 300-second
cap rather than the tool's 30.

## Sessions

Sessions are linear append-only JSONL files stored at:

```
.oneloop/sessions/YYYY-MM-DD.jsonl
```

`/clear` rotates to a new file (`YYYY-MM-DD-001.jsonl`, `YYYY-MM-DD-002.jsonl`, etc.).
Old sessions are preserved on disk — never deleted.
On restart, the latest session file for today is opened automatically.

`metrics/` mirrors that naming, one file per session and one JSON line per
`api_call` or `tool_exec` event, so a session and its costs are read together.
An `api_call` says what it was for in `purpose` — `turn` for a request the
loop made, `summarize` for a compaction's — so both can be summed as one cost
and still told apart. A `tool_exec` carries one `duration_ms` for the batch and
a `calls` array with each call's own span: the batch number is the distance from
the first call's start to the last call's end, which cannot be checked without
the per-call spans it was computed from.
A turn the user stops with Ctrl+C writes a `turn_cancelled` event with its
duration and what it was waiting on: the request and the tool batch are both
inside the future the interrupt drops, so this is the only line that survives,
and without it a cancelled turn leaves no trace at all.
It is filed in the conversation's root — `.oneloop/metrics/` beside the
project's own sessions — and each of the three files a conversation leaves (its
session, its ledger, its refusal log) derives from that one root rather than
knowing the layout for itself.

An agent keeps its conversation at `.oneloop/sessions/`, with its ledger in
`metrics/`, its refusals in `errors.log` and its lock beside it.

A one-shot — `ol "…"`, or a prompt piped in — is stateless: it opens no
session file, reads no `AGENTS.md`, and records no metrics or error log, so
it neither inherits nor disturbs the conversation on disk.

On open (and after a Ctrl+C-cancelled run), any tool call left without a matching
result is closed out with a synthetic error result — providers reject conversations
containing dangling tool calls, so an unrepaired session would break every later request.

That repair assumes a single writer, so a project is claimed by one process at a
time: `.oneloop/session.lock` is held (an advisory lock, released by the kernel
when the process dies) for as long as a session is open, and a second `ol` in
the same directory refuses to start rather than answering a tool call the first
one is still running. A one-shot takes no lock, because it writes nothing.

## Agents from specs

An agent is a value. The loop, the session machinery, the message vocabulary and
the `Tool` trait are shared; an `AgentSpec` decides *what* is shared — where the
agent works, which tools it may call, which instructions it reads, and whose
conversation it keeps. `Agent::from_spec` is the only constructor, and the
default `ol` is just the first spec it is handed:

```rust
pub struct AgentSpec {
    pub cwd: PathBuf,
    pub instructions: Vec<Instruction>,
    pub model: Option<String>,
    pub tools: ToolRegistry,
    pub session: SessionPolicy,
}
```

That is the whole mechanism, and it is deliberately no more than that: no plugin
registry, no trait objects over agents, no config file. A preset is a function
returning a spec, and there is one — `AgentSpec::coding`, which is `ol`. A second
agent is a second function here, not a second branch in the loop.

`SessionPolicy` is where the conversation lives:

| policy | sessions | lock | id |
|---|---|---|---|
| `Project` | `.oneloop/sessions/` | `.oneloop/session.lock` | `{project}-{stem}` |
| `Ephemeral` | nothing on disk | none | `{project}-{stem}` |

The root is the directory holding `sessions/`, `metrics/`, `errors.log` and
`session.lock`, so a policy that names no root takes no lock and leaves no
trace.

The run mode is applied *after* the spec is built, and it always wins on one
field: `ol "…"` forces `Ephemeral`, so it writes no session file exactly as `ol`
with no prompt writes one. A spec says what the agent is; the run mode says
whether the conversation persists.

A `model` is the model the agent *works as*: `from_spec` makes it active in the
registry once, and `None` leaves whatever the config made active, which is what
the default agent always gets. It is deliberately not a second opinion held
beside the registry — a per-turn preference would disagree with `/model` the
moment the user typed it, and the mode line, the gauge and the next request must
all read one answer. A spec that names a model the config does not have fails
where it is built, naming the alias.

One boundary to name, because it decides when this holds: the registry is
process-wide, and `Arc` makes it shared. Two agents built in one process from the
same registry would share this answer — whichever was built last would decide for
both. That is why a second agent in one process is a deliberate step (the model
moving onto the agent) and not something a preset already gives you.

Everything a spec decides is resolved once, in `from_spec`: the system prompt is
computed there from the spec's own tools and instructions, and the metrics and
refusal log follow the conversation's root — a run that writes nothing records
nothing.

## Compaction

When the server refuses a request because the conversation no longer fits,
OneLoop replaces that conversation with a briefing and retries the request that
ran into the limit. A written transcript stays on disk and the briefing names
it, so nothing is lost — only moved out of the window. A one-shot keeps no
transcript, and its briefing says so instead of naming one.

The summarizer is a function, not an agent: text in, summary out, no tools, no
session, one request through the same retry and fallback every other request
gets. That shape is what makes the rest decision-shaped rather than
judgement-shaped, and `agent/compact.rs` is the whole of it:

1. `render` turns the transcript into text, naming a tool result longer than
   2 KB instead of quoting it, and cutting a tool call's arguments to the same
   size with a marker that says so. A tool result that *failed* is the one
   exception: it keeps both its ends, because a build or a test run puts the
   command first and the verdict last, and those are what the next
   conversation needs. This is where most of the compression happens, before
   any model is called.
2. `chunk` cuts that text at a budget of half the declared window, between
   lines so no line is split — a message can still straddle the cut, which is
   why each part is prefixed with which one it is. A rendered session usually
   fits in one piece; when it does not, each piece is summarized and the
   summaries are summarized, up to three rounds.
3. `clear_session` rotates to a new file — the old one stays.
4. `briefing` writes the summary and the path to the old transcript as the new
   session's first message. In a turn, the prompt that ran into the limit
   follows it, so the turn can answer. A one-shot has no transcript to name, and
   its briefing says so rather than pointing at the project's own session.

That request is an `api_call` in the ledger like any other, carrying
`purpose=summarize` rather than `purpose=turn`: it is billed the same way, and
a compaction can be a session's most expensive request, so it is counted
beside the turns it replaces rather than left out of the total.

A compaction is a request, so it says when it starts — `compacting...` — and
when it is done: `compacting done`, naming the new session's file unless the
conversation was never written. Without the pair, a long compaction cannot be
told from a hang, and where the spinner is silent there is nothing else on
screen to tell it.

The same pair reaches Emacs's mode line through the status report: `working`
with `msg=compacting` while the summarizer runs, and the state the guard found
when it began, once the summary is in hand. A comint buffer, where the spinner
is silent, then says `compacting...` where it would otherwise say
`computing...` — and an overflow, which compacts inside a turn, is back to
`computing...` when the turn resumes.

The refusals that are not an overflow change nothing: `Attempt` in `turn.rs` is
what separates "the provider said no" from "it no longer fits", and only the
second one has a cure. A turn gets one compaction. A second overflow is a
conversation whose own shortest form does not fit — checked against the gauge
before the retry is spent, so the answer names the message rather than the
window. That exit names `/clear` and a larger window and stops there: `/compact`
is the thing that just did not help, and a summary of a summary is not smaller,
so offering it again would be advice that cannot work.

`/compact` runs the same thing on demand, and `/compact; <prompt>` summarizes
and then sends the prompt.

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

`output.rs` owns both the ANSI escapes — CSI and OSC, the latter as `osc`,
which is framing only — and the shape of a status line, so a colour change, a
`--no-color` flag, or `NO_COLOR` support is one edit rather than thirty.
Callers pick a meaning — `fail`, `warn`, `stopped`, `ok`, `tick`, `running`,
`step`, `note`, `head` — and the module decides the glyph, the colour, and the
stream.

When a tool call returns it says how long it took: `✓ read src/main.rs · 0.3s`,
and a failure says it in the same place, before the reason — `✗ bash: cargo test
· 12.4s (failed: …)`. `elapsed` is the one spelling of a duration — a tenth below
ten seconds, whole seconds above, minutes as minutes — so a number reads the same
way wherever it appears. The span itself is measured once, in `batch.rs`, where
the call actually runs: the trace line and the `tool_exec` ledger entry both read
that measurement rather than a second clock beside it.

The streams are split by what the text *is*, not by how bad it is:

- **stdout** carries the model's answer, and nothing else.
- **stderr** carries everything OneLoop says about itself — the tool trace,
  the model banner, the picker, warnings, and errors.

So `ol "..." > answer.md` captures the answer alone while the trace stays on
the terminal, and a failing run writes nothing to stdout at all. Two shapes
stay outside the module by design: the spinner's cursor manipulation, and the
column-aligned model picker, which no general helper could serve without
becoming one caller's shape.

The trace line for a tool call is `agent/trace.rs::format_tool_call`, and it is
always one line: `name: argument`. Only the prefix is the agent's — the
argument is the tool's own answer, `Tool::trace_label`, because only the tool
knows how its argument should read: a bash command and an elisp expression are
flattened from their line breaks and cut at `TRACE_WIDTH` (240) characters
before `...` closes them, a path is resolved and spelled the way every other
tool spells it, and a skill name is looked up on disk to name the file that
would load. A command the model wrote as a whole here-document, or an
expression written as a whole program, would otherwise scroll the trace away,
and the line's job is to say that one ran — an opening `(progn` alone says
nothing about what the `progn` did. Those two shapes — `elide` and
`first_line` — live in this module so a tool reaching for them does not reach
into the agent; `first_line` is left to the path tools, whose arguments are one
line by nature.

Not every terminal can take everything the module emits. `plain()` reports
terminals that cannot take cursor addressing — comint runs its children with
`TERM=dumb` and marks them in `INSIDE_EMACS` — and there the spinner stays
silent: its per-frame rewrites would land in `comint-output-filter` on
Emacs's single thread, a firehose that grinds even when the buffer is not
displayed. `comint()` reports comint specifically, the one place with a
consumer for the **OSC 7501** (Program Status Protocol) reports that
`crate::status` emits instead: one report when a turn starts (`state=working`),
one when it ends (`state=idle`), carrying the model alias in a base64 `title`
field.

A report says what a busy process is busy *with* in a base64 `msg`, because a
mode line has no trace to read: the spec's five states are all the vocabulary
there is, so the word `compacting` rides beside `state=working` rather than
inventing a sixth. Only a stretch of work worth naming sends one — a turn's
requests do not — and a report replaces its record whole, so the report that
takes the name back is the one that ends the work, whether it finished or
failed. A compaction that runs inside a turn belongs to a `Compacting` guard
that remembers whose busy state it borrowed: an overflow leaves the turn
working where `/compact` at the prompt leaves the process idle.

The record's fields are `crate::status`'s to decide and the framing around them
is `output::osc`'s — `ESC ] 7501 ;` and the ST terminator are terminal syntax,
the same kind of fact as the CSI escapes in that module, and what a sequence
number means belongs to whoever speaks it. A second OSC feature would reach
for `output::osc` and need nothing from `status`.

## Releases

`version` in `Cargo.toml` is the release version; nothing else stores one by
hand. `Cargo.lock` carries a copy while the lockfile is committed, and the
release builds with `--locked`, so a bump left unsynced fails the build step
rather than shipping — the next plain `cargo build` rewrites that one line.
Bumping it, committing, and running `./release` tags `v<version>` and pushes the
tag — which is the only thing that publishes. The script refuses a dirty tree
and an existing tag for the same reason the workflow re-checks the tag against
`Cargo.toml`: a release is a claim about one specific commit.

Two workflows, and the checks have one definition between them. `ci.yml` runs
`cargo fmt --check`, clippy at `-D warnings`, and the tests on every branch push
and pull request. `release.yml` calls that same workflow through
`workflow_call`, so a release cannot skip a check a push would have caught, then
builds `cargo build --release --locked` inside `nix develop`, strips the binary,
and uploads `oneloop-v<version>-x86_64-linux.tar.gz` to the tag's release.

The asset is built with the flake's toolchain rather than a runner-provided
Rust, so the released binary and the development shell cannot drift apart. Only
`x86_64-linux` is built, though the flake supports `aarch64-linux` too.

`install.sh` is the user-facing half: it resolves the latest release through the
GitHub API, downloads the tarball for the running platform, installs the binary
into `${ONELOOP_BIN:-~/.local/bin}` and symlinks `ol` beside it. It writes
nothing under `~/.oneloop`, so a fresh install starts on the local default
model.

## Source layout

```
src/
  main.rs           CLI entry point, login command, --version
  messages.rs       Message types (User, Assistant, ToolCall, ToolResult) and ToolDefinition
  agent.rs          Agent struct, from_spec, accessors, session repair, context gauge,
                    log_cancelled_turn
  agent/
    spec.rs         AgentSpec, SessionPolicy, the coding preset
    compact.rs      render, chunk, budget, the summarizer request and the briefing
                    that replaces a conversation with a summary of itself
    turn.rs         run_once*, request, Attempt, record, continues,
                    execute_tool_calls, log_api_call
    trace.rs        format_tool_call: the name, a colon, and the tool's own label
    banner.rs       the startup summary: summary, packed, the column it wraps on
    batch.rs        One batch of tool calls, the per-file locks that order them, and
                    the per-call timing the trace and the ledger both read
    duplicate_reads.rs
                    answering a re-read with a pointer: read_payloads_in_context,
                    payload_digest, suppress_duplicate_reads, duplicate_read_notice
    spinner.rs      SpinnerGuard (AbortHandle-based RAII spinner)
    session.rs      Session persistence, root, rotation, dangling-tool-call repair, the
                    per-conversation lock
    metrics.rs      Per-session JSONL metrics (api_call, tool_exec, turn_cancelled),
                    token estimation
  antigravity/
    mod.rs          Public entry points for Antigravity requests
    client.rs       Stateless Antigravity CLI execution, discussion prompts, reference responses
  app.rs            Interactive REPL (rustyline), `/` and `!` commands, Ctrl+C handling
  auth.rs           Credential resolution (env over ~/.oneloop/auth.json) and storage
  auth/
    codex.rs        ChatGPT sign-in: PKCE, the callback server, token refresh
  catalog.rs        ~/.oneloop/config.json: providers and their models, validation, active model
  claude/
    mod.rs          Public entry points for Claude requests
    client.rs       Stateless Claude CLI execution, discussion prompts, reference responses
  config.rs         env_or, the working directory
  models.rs         The registry: the list, which one is active, switching
  models/
    retry.rs        Retry a request, then offer another model when one won't answer
  output.rs         Status lines (glyph, colour, stream), output truncation, trace-line
                    rendering (elide, first_line, elapsed), ANSI constants and OSC framing
  path.rs           Path resolution (@, ~, absolute, relative) and how the trace spells one
  prompt.rs         System prompt assembly (tool preamble + engineering tenets + environment
                    + AGENTS.md), and the summarizer's own system prompt
  providers.rs      Provider (endpoint: URL, credentials, one HTTP client), Model (alias +
                    settings + its provider + how it completes), request/response types,
                    the refusal log's name inside a conversation root
  providers/
    chat.rs         The Chat Completions wire format
    responses.rs    The shared Responses API implementation
  tools.rs          Tool trait (incl. trace_label, lock_path), ToolRegistry (Arc<dyn Tool>),
                    AgentContext
  tools/
    bash.rs         Bash command execution
    elisp.rs        Emacs Lisp evaluation through a running Emacs server
    read.rs         File reading
    write.rs        File writing
    edit.rs         Find-and-replace file editing
    skill.rs        On-demand skill loader (scans .oneloop/skills/ and ~/.oneloop/skills/ for <name>/SKILL.md),
                    skill_file
  status.rs         Turn state, model title, and named work via OSC 7501 (Program Status
                    Protocol)
docs/
  architecture.md   This file
  index.html        Executive presentation (GitHub Pages, space-bar nav)
  style-guide.md    Coding conventions and lint config
```
