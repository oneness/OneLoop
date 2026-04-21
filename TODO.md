# TODO

## Near term

- [ ] **Skills loader** — Walk `.oneloop/skills/*.md` and `~/.oneloop/skills/*.md`, concatenate, append to system prompt. Makes oneloop extensible without Rust changes. Users drop markdown files to teach the agent new capabilities through natural language.

- [ ] **Config-driven tools** — A `.oneloop/tools/*.toml` directory where users declaratively define tools (name, description, JSON schema, exec command). The registry reads these at startup, generates `ToolDefinition` entries, and interpolates arguments into the exec command via bash. First-class tool visibility without writing Rust code.

- [ ] **Context compaction** — Auto-compact when approaching context window limit. None of the LLM providers (Anthropic, OpenAI, Z.AI) do this at the API level — they just error out. The agent must manage it. See compaction plan below.

- [ ] **Qwen provider** — Add `@qwen` as a first-class provider. Qwen is OpenAI-compatible (same `/chat/completions` endpoint with tool calling support). Base URL: `https://dashscope.aliyuncs.com/compatible-mode/v1`. Models: `qwen-coder-plus`, `qwen3.6-plus`, `qwen3.6-flash`. Dramatically cheaper than Anthropic/OpenAI (~$0.17-0.97/M tokens vs $3-15/M). Extend `OpenAIProvider` with Qwen defaults, add `oneloop login qwen`, register in `ProviderRegistry`.

## Later

- [ ] **WASM plugins** — For when skills and config-driven tools aren't enough. A `.wasm` file in `~/.oneloop/plugins/` implements a simple ABI (`name()`, `description()`, `schema()`, `execute(json) -> json`). Language-agnostic, sandboxed. Only needed for actual computation logic that can't be expressed as a shell command or prompt recipe.

- [ ] **RPC mode** — Expose the agent loop over a socket/HTTP so editors and other tools can drive it programmatically.

- [ ] **Prompt templates** — Named reusable prompt patterns (e.g. "refactor", "debug", "explain") stored in `.oneloop/templates/`.

- [ ] **Session branching** — Fork a session at an arbitrary point to explore alternative paths without losing the original conversation.

- [ ] **TUI polish** — Richer terminal UI with colors, markdown rendering, progress bars.

## Context Compaction Plan

### Problem

Long coding sessions accumulate messages until the context window overflows. All LLM providers return errors when you exceed their token limit — they do **not** auto-compact. Oneloop currently just crashes with a provider error.

### How others do it

All compaction is **client-side** (the agent, not the API):

- **OpenAI Codex CLI** — `/compact` command + auto at token threshold. Preserves last ~20K tokens of recent messages alongside summary.
- **Claude Code** — `/compact` command + auto at ~95% capacity. Generates handoff summary, replaces history.
- **OpenCode** — `/compact` + auto overflow check. Separate "prune" mechanism for large tool outputs (>40K token protection window).
- **Amp** — Manual "handoff" only. Philosophy: keep conversations short.

### Proposed implementation

#### New file: `src/agent/compact.rs`

1. **Token estimation** — heuristic ~4 chars/token (good enough for threshold checks). Optionally `tiktoken-rs` for OpenAI models.
2. **Compaction prompt** — following Codex CLI's approach:

```
You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary
for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly
continue the work.
```

3. **Auto-compaction flow** — after each provider response:
   - Estimate total tokens in `session.messages()`
   - If over threshold (e.g. 85% of model's context window, configurable via `ONELOOP_COMPACT_THRESHOLD`):
     - Show `⏳ compacting context...`
     - Send entire conversation + compaction prompt to the model
     - Collect the summary
     - Replace message history with: `[System prompt] + [Summary prefix] + [Compacted summary] + [Last ~20K tokens of recent messages]`
     - Continue the loop
4. **Summary prefix** (prepended to summaries so the model knows it's a continuation):

```
Another language model started to solve this problem and produced a summary
of its thinking process. Use this to build on the work that has already been
done and avoid duplicating work. Here is the summary:
```

#### Changes to existing files

- **`session.rs`** — add `replace_messages()` method for in-memory replacement. Persist compacted session as a new JSONL (or overwrite).
- **`agent/mod.rs`** — after each provider response, call `check_compact()`. Print a message when compaction happens so the user knows.
- **`app.rs`** — support `/compact` command in interactive mode for manual compaction with optional custom instructions.

#### Configuration

- `ONELOOP_COMPACT_THRESHOLD` — percentage of context window to trigger auto-compaction (default: 85)
- `ONELOOP_COMPACT_PRESERVE_TOKENS` — how many tokens of recent messages to preserve (default: 20000)
- `ONELOOP_NO_AUTOCOMPACT` — disable auto-compaction, only manual `/compact`

#### Token counting approach

Simple heuristic first (4 chars/token). If needed later, add `tiktoken-rs` for exact OpenAI token counting and rough estimates for other providers. The threshold is a safety margin anyway — exact counting isn't critical.

#### What gets preserved

- System prompt (AGENTS.md)
- Compaction summary
- Last ~20K tokens of recent messages (current train of thought)
- All future messages after compaction

#### What gets lost

- Detailed tool outputs from early in the session
- Verbose assistant responses from earlier turns
- These are summarized into the compaction summary

#### Risks / considerations

- Multiple compactions cause cumulative information loss — quality degrades over time
- Compaction mid-task (e.g. in the middle of a multi-step refactor) can cause the model to "go off the rails"
- Compaction costs one extra API call (the summarization request)
- Should warn user: "Long conversations and multiple compactions can cause the model to be less accurate"

## Done

- [x] **Built-in tools** — read, write, edit, bash
- [x] **Web search tool** — SearXNG-backed `web_search` tool, configurable via `ONELOOP_SEARX_URL`
- [x] **Multiple providers** — Z.AI, OpenAI, Anthropic, mock fallback
- [x] **Ctrl+C to stop** — Agent loop checks a shared stop flag, Ctrl+C breaks out cleanly
- [x] **Interactive mode** — REPL with animated spinner
- [x] **Session persistence** — JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- [x] **AGENTS.md context** — Loaded as system prompt from project directory
- [x] **Auth** — API keys stored in `~/.oneloop/auth.json`
