# OneLoop

Most coding agents are built around a vendor. OneLoop is built around your subscriptions.

One agent for your Claude, ChatGPT, Antigravity, OpenRouter, and OpenCode subscriptions — plus local when you want private. One loop, six tools, no framework. Models are chosen per run and switched mid-conversation, and everything said lands in one append-only session file you can grep.

**Deeper:** [Overview](https://www.birkey.co/oneloop/) ·
[Architecture](docs/architecture.md) ·
[Style guide](docs/style-guide.md)

## Why it is different

- **One session, many models.** `/model` switches mid-conversation. `/cc`,
  `/ca`, and `/ag` ask Claude Code or Antigravity about the conversation
  without switching your model at all. Plan with the expensive one, implement
  with the cheap one, take a second opinion from a third.
- **Subscriptions you already pay for.** Claude Code and Antigravity are driven
  through their own CLI binaries, so their answers spend quota you have already
  bought rather than metered tokens. ChatGPT Plus/Pro is signed in to directly
  over OAuth and then behaves as an ordinary provider.
- **Any endpoint your subscriptions already cover.** A provider is one URL, one credential, one
  connection pool; the models listed under it get short aliases. The same code
  serves llama-server, OpenRouter, OpenCode Go over Chat Completions, and
  ChatGPT's Codex backend plus OpenCode Go Responses models over the Responses
  API.
- **Nothing is compacted.** No summarization, no silent truncation. Every
  message, tool call, and result is appended to a JSONL file, so the session is
  what was said, in order — and it is greppable on disk. The one substitution is
  a `read` whose bytes the conversation already holds: it is recorded as a
  pointer to the earlier copy, and the trace says so when it happens.
- **Small enough to read.** Six tools, about eight thousand lines. No plugins,
  no hooks, no middleware — the whole agent reads in a sitting.

Tools: `read` (file contents), `write` (create or overwrite), `edit`
(exact-match replace), `bash` (run a command), `elisp` (live Emacs state through
`emacsclient`), and `skill` (load a markdown playbook on demand).

Web search is not a tool, because it does not need to be one. A hosted provider
that sets `"web_tools": true` runs the search and the page fetch server-side and
bills per use. Otherwise the model searches the way you would: a terminal client
like [`ddgr`](https://github.com/jarun/ddgr) — DuckDuckGo from the command line —
run through `bash`, with `curl` for the full page. Put the commands you like in
a skill, and the model loads them when the task calls for it.

## Quick start

```bash
./ols        # start a local model — no key, no account, nothing leaves the box
./ol         # interactive REPL
```

Reach a hosted model only when you want one:

```bash
./ol login openrouter     # API key
./ol login opencode-go    # OpenCode Go API key
./ol login openai         # ChatGPT Plus/Pro, in a browser
```

One-shot and piped runs are stateless — no session file, no `AGENTS.md`:

```bash
./ol "explain this codebase"
git diff | ./ol "summarise these changes"
```

## Commands

| Command | Effect |
| --- | --- |
| `/help` | current model, models, tools, session file, message count, loaded instructions |
| `/model` | list configured models and pick one |
| `/model <alias>` | switch straight to that alias |
| `/reload` | reload `~/.oneloop/config.json` without restarting |
| `/clear` | wipe context and start a fresh session file |
| `/cc [focus]` | ask Claude Code to critique the discussion |
| `/ca <question>` | ask Claude Code about the discussion, or to implement something |
| `/cn <prompt>` | ask Claude Code with no discussion history |
| `/ag`, `/aga`, `/agn` | the same three for Antigravity |

A command can carry a prompt across the `;` boundary, which is what makes the
multi-model workflow quick:

```text
(nvidia)> /model luna; plan how to split the parser into modules
(luna)> /model nvidia; implement step 1 of that plan
(nvidia)> /ca is the plan still consistent with what we implemented?
```

`/cc` and `/ca` send the transcript, including tool arguments and results, as
untrusted reference material; `/cn` sends only your prompt. Nothing is resumed
on the other side — each request is a fresh review — and the answer comes back
as a reference message in *this* session, so the current model can reason about
it. `/help` lists the Claude Code and Antigravity commands only when their
binaries are on `PATH`. `Ctrl+C` stops a running request, or discards the draft
at the prompt. An unknown `/word` is sent as an ordinary prompt — a message can
legitimately start with a path.

## Providers and models

A **provider** is a place to send requests — a base URL, a protocol, and shared
credentials when it needs them. A **model** is one thing that place will run.
OpenRouter is a single provider serving hundreds of models, so its URL and API
key are stated once and the models listed under it. ChatGPT's Codex backend
instead shares a renewable OAuth grant across its models.

Three API families are spoken: Chat Completions for local models and most
hosted providers, and the Responses API for the Codex backend — which is the
only way into a ChatGPT subscription — and for Responses models on providers
like OpenCode Go. A provider's `api` field names the default, and a model can
override it.

Every model has a short **alias**, which is the name used everywhere else:
`/model nvidia` rather than the wire id it resolves to. Aliases are unique across
all providers, so naming one never has to say which provider it meant.

Config is `~/.oneloop/config.json`, written from a template on first run. This
is that template:

```json
{
  "default": "qwen",
  "providers": {
    "local": {
      "base_url": "http://localhost:8080/v1",
      "models": {
        "qwen":    { "id": "Qwen3.6-35B-A3B-Q4_K_M.gguf" },
        "glimmer": { "id": "Muse-Glimmer-30B-Q4_K_M.gguf" }
      }
    },
    "openai": {
      "base_url": "https://chatgpt.com/backend-api",
      "api": "codex",
      "models": {
        "luna": { "id": "gpt-6-luna", "reasoning_effort": "medium" }
      }
    },
    "openrouter": {
      "base_url": "https://openrouter.ai/api/v1",
      "api_key_env": "OPENROUTER_API_KEY",
      "web_tools": false,
      "models": {
        "nvidia": { "id": "nvidia/nemotron-3-ultra-550b-a55b:free", "context_window": 1000000 },
        "laguna": { "id": "poolside/laguna-s-2.1:free",              "context_window": 262144 },
        "ling":   { "id": "inclusionai/ling-3.1-flash",              "context_window": 262144 }
      }
    },
    "opencode-go": {
      "base_url": "https://opencode.ai/zen/go/v1",
      "api_key_env": "OPENCODE_GO_API_KEY",
      "models": {
        "deepseek-flash": { "id": "deepseek-v4.1-flash", "context_window": 1000000 },
        "meta-muse":      { "id": "muse-spark-1.3-contributor",
                            "context_window": 1048576, "api": "responses" },
        "mimo-flash":     { "id": "mimo-v2.6-flash", "context_window": 1050000 }
      }
    }
  }
}
```

Credentials live in `~/.oneloop/auth.json` (mode `0600`), never in the config.
An API key is read from its environment variable first and from `auth.json`
second, so exporting a variable is the caller saying "use this". A subscription
has no key to name: `ol login openai` runs an OAuth authorization-code flow with
PKCE and stores the grant, which renews itself in the background afterwards —
one-time step, same account and quota the Codex CLI uses.

Setting a model, narrowest scope first: `/model` affects the session,
`ONELOOP_MODEL` a single run, and `default` in the config every run. Only the
last is persistent.

### Running the local server

The `local` provider expects an OpenAI-compatible server on port 8080. This
flake builds and runs one:

```bash
./ols
```

With no model present it offers to download one (~20 GB, into `~/models/`) and
starts the server once it lands. The download resumes if interrupted, and lands
as `.part` until complete — an aborted transfer never looks like a usable model.
To use different weights:

```bash
./ols -- /path/to/other.gguf
# or: ONELOOP_LOCAL_MODEL=/path/to/other.gguf ./ols
```

The offer is only made for the default, and only with a terminal attached: a
script or CI run gets the `curl` command printed instead of a surprise 20 GB
transfer. It wraps llama.cpp's Vulkan build with flags measured against
Qwen3.6-35B-A3B — see the comments in `ols` for what each one is worth.
`ONELOOP_LOCAL_PORT` moves it off 8080.

## Sessions and skills

Sessions are linear append-only files at `.oneloop/sessions/YYYY-MM-DD.jsonl`;
`/clear` rotates to `-001`, `-002`, and so on, and nothing is ever deleted. On
restart the latest session for today opens automatically. A tool call left
without a result — after a `Ctrl+C` — is closed out on open, because providers
reject a conversation containing dangling calls.

Nothing is summarized and nothing is dropped behind your back. A conversation
grows until the server refuses it, and the refusal names the fix: `/clear`, or a
model with a larger window. A model may declare a `context_window`, in which
case the prompt shows roughly how much is still free.

Skills are markdown playbooks under `~/.oneloop/skills/` and
`.oneloop/skills/` (project wins on a name collision). They are not in the
system prompt — the model sees a name and description for each and calls
`skill("name")` when the task matches. Anything that is a command plus
instructions — a search client, a formatter, a house checklist — belongs here
rather than in the core. If no skill files exist, the tool is not registered at
all.

## Agent status (OSC 7501)

OneLoop reports the active model and turn state via **OSC 7501** (Program Status
Protocol) when running under Emacs `comint` (or any terminal that sets
`INSIDE_EMACS=...,comint`). This lets any client — editor, terminal, tooling —
read the current model and whether a turn is running, without scraping the
prompt.

```
ESC ] 7501 ; state=working|idle : app=oneloop : title=<base64 model> ESC \
```

- `state` — `working` while a turn runs, `idle` between turns. OSC 7501 defines
  further states (`done`, `blocked`, `error`, `clear`); OneLoop emits only these
  two.
- `title` — the model alias (base64-encoded UTF-8), so the client always knows
  the current model even in idle.

## Tuning

All optional:

- `ONELOOP_MODEL` — run this one model instead of the config's `default`
- `ONELOOP_MAX_ITERATIONS` — cap on agent-loop iterations per prompt (default: `100`)
- `ONELOOP_MAX_RETRIES` — attempts before offering another model (default: `10`)
- `ONELOOP_WEB_TOOLS` — override a provider's `web_tools` for this run

## Development

```bash
nix develop
cargo check
```

## Contributing

This project is personal software that I maintain for my own use. I do not
accept pull requests.

If it's useful to you: fork it, copy the code, adapt it freely. The only ask is
that you keep the copyright notice intact (MIT license).

## License

MIT — see [LICENSE](LICENSE).
