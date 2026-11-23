# OneLoop — mental model

OneLoop is a loop around one model. `app` reads a line, `agent::turn` sends it,
tools run, their results go back, and the loop ends when the model asks for
nothing more. This page names contracts and direction, not implementation.
<!-- module: app --> <!-- module: agent::turn -->

## The flow

```text
  app ──> agent ──> turn ──> providers ──> model
           │          │            │
        session    batch ──> tools └── answer returns to turn
           │          │
        metrics   per-file locks
  overflow ──> compact ──> turn retries once
```
<!-- edge: app>agent --> <!-- edge: agent>agent::session --> <!-- edge: agent>agent::metrics --> <!-- edge: agent::turn>agent::batch -->
<!-- edge: agent::batch>tools --> <!-- edge: agent::turn>providers --> <!-- edge: agent::compact>agent::turn --> <!-- edge: models>models::retry -->

## Contracts by layer
Each row is the contract a module offers the rest of the tree.

**Foundation**

| Module | Contract |
|---|---|
| `main` | Declares the module tree and starts the app. <!-- module: main --> |
| `config` | Working directory, `ONELOOP_*` settings, retry default. <!-- module: config --> <!-- fact: config::DEFAULT_MAX_RETRIES = 10 --> |
| `messages` | One wire-neutral conversation type set, shared by session, tools and providers. <!-- module: messages --> |
| `prompt` | The fixed instructions loaded before the conversation. <!-- module: prompt --> |
| `output` | Stdout carries the answer only; stderr carries the trace. <!-- module: output --> |
| `path` | Resolve a model-sent file path and spell it back for the trace. <!-- module: path --> |
| `auth` | API-key credentials and where they are stored. <!-- module: auth --> |
| `claude` | Descriptor for the Claude Code second-opinion CLI. <!-- module: claude --> |
| `antigravity` | Descriptor for the Antigravity second-opinion CLI. <!-- module: antigravity --> |
| `tools::edit::matcher` | Find an edit's anchor and splice its replacement. <!-- module: tools::edit::matcher --> |
| `tools::edit::refusal` | Explain a failed edit in the words the model reads. <!-- module: tools::edit::refusal --> |

**Mechanisms**

| Module | Contract |
|---|---|
| `catalog` | `~/.oneloop/config.json`: providers, their models, globally unique aliases. <!-- module: catalog --> |
| `status` | Busy, idle, title and current work on one OSC 7501 line. <!-- module: status --> <!-- fact: status::STATUS_SEQUENCE = 7501 --> |
| `agent::spinner` | RAII heartbeat for work long enough to name, silent where a spinner cannot run. <!-- module: agent::spinner --> |
| `auth::codex` | ChatGPT subscription login and grant refresh. <!-- module: auth::codex --> |
| `tools` | The `Tool` trait and registry: name, schema, execution, trace label. <!-- module: tools --> |
| `tools::edit::show` | Render the line ranges an edit reports: hints, candidates, changes. <!-- module: tools::edit::show --> |

**Transport and tools**

| Module | Contract |
|---|---|
| `providers` | Endpoint, credentials and pool belong to the place; wire data to the model. <!-- module: providers --> |
| `providers::chat` | Chat Completions transport for local and API-key providers. <!-- module: providers::chat --> |
| `providers::responses` | Responses transport, including the Codex backend. <!-- module: providers::responses --> |
| `models` | Flat alias-to-model registry; models of one provider share its connection. <!-- module: models --> |
| `models::retry` | Retry one model, then offer another. <!-- module: models::retry --> |
| `agent::spec` | What an agent is: cwd, instructions, model, tools, session policy. <!-- module: agent::spec --> |
| `agent::trace` | One label format for every tool call on screen. <!-- module: agent::trace --> |
| `agent::batch` | Run one answer's calls at once, with a lock per file. <!-- module: agent::batch --> |
| `tools::bash` | Run a shell command with a timeout and elided output. <!-- module: tools::bash --> |
| `tools::read` | Read file contents within the output caps. <!-- module: tools::read --> |
| `tools::write` | Replace a whole file atomically. <!-- module: tools::write --> |
| `tools::elisp` | Evaluate Emacs Lisp in the running server. <!-- module: tools::elisp --> |
| `tools::skill` | Load a named skill from disk and report which file it found. <!-- module: tools::skill --> |
| `tools::edit` | Ordered search-and-replace edits to one file, in one write. <!-- module: tools::edit --> |
| `tools::edit::steps` | Read, locate, write and receipt for one edit. <!-- module: tools::edit::steps --> |

**State and second opinions**

| Module | Contract |
|---|---|
| `agent::session` | Append-only conversation; repairs a call left without its result. <!-- module: agent::session --> |
| `agent::metrics` | One JSON line per request, tool batch and compaction, beside the session. <!-- module: agent::metrics --> |
| `review` | One subprocess mechanism for both second-opinion CLIs. <!-- module: review --> |
| `claude::client` | Run `claude` with the session as untrusted reference material. <!-- module: claude::client --> |
| `antigravity::client` | Run `agy` with the same contract. <!-- module: antigravity::client --> |

**Orchestration**

| Module | Contract |
|---|---|
| `agent` | Assembles the agent: spec, session, metrics, models, prompt, providers. <!-- module: agent --> |
| `agent::turn` | One request at a time; sort the reply, write it, then run calls or end. <!-- module: agent::turn --> |
| `agent::banner` | The first-screen summary of what this agent is. <!-- module: agent::banner --> |
| `agent::compact` | Render, chunk, summarize and replace the session; at most 3 rounds. <!-- module: agent::compact --> <!-- fact: agent::compact::MAX_ROUNDS = 3 --> |
| `app` | REPL input, slash commands, `!` lines, one-shot mode. <!-- module: app --> |

## Boundaries

The conversation and its cost ledger are files under `.oneloop/`; errors go to
`errors.log`; the answer goes to stdout and every trace line to stderr.
<!-- module: agent::session --> <!-- module: agent::metrics --> <!-- module: providers --> <!-- module: output -->

## Where to start reading

`src/main.rs` → `app.rs` → `agent.rs` → `agent/turn.rs`, then follow the one
arrow your question is about. <!-- module: main --> <!-- module: app --> <!-- module: agent --> <!-- module: agent::turn -->
