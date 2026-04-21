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

### Key insight

If the context window is filling up, that's a sign the session is doing too many things. Rather than lossy in-place compression (which degrades quality with each compaction), the better approach is: **wrap up the session gracefully, write a handoff document, start fresh.**

### How others do it (and why it's problematic)

- **OpenAI Codex CLI** — `/compact` replaces history with a summary + recent messages. Cumulative information loss over multiple compactions.
- **Claude Code** — Same pattern, auto at ~95%. Users report model "goes off the rails" after multiple compactions.
- **Amp** — Manual "handoff" only. Philosophy: keep conversations short and focused.

### Proposed approach: Session Handoff (not compression)

Instead of compressing the context in-place, we:

1. **Detect** when context is nearing the limit (e.g. 85% threshold)
2. **Wrap up** — ask the model to produce a structured handoff document:
   - What was accomplished
   - Current work in progress
   - Files involved and their current state
   - What remains to be done
   - Key decisions, constraints, user preferences
3. **Persist** the handoff to disk (`.oneloop/handoff.md`)
4. **Start a new session** with the handoff as initial context
5. The old session stays intact on disk (never deleted, always recoverable)

This is "compaction without compaction" — no information loss, no quality degradation. The model gets a clean context window with everything it needs to continue.

### Implementation

#### New file: `src/agent/compact.rs`

1. **Token estimation** — heuristic ~4 chars/token for threshold checks.
2. **Handoff prompt**:

```
We're approaching the context window limit for this session. Before we
start a new session, create a detailed handoff document that captures
everything needed to continue this work seamlessly.

Include:
- What was accomplished in this session
- Current work in progress (be specific about files, functions, state)
- What remains to be done (clear next steps)
- Key decisions made and why
- Important context, constraints, or user preferences
- Any critical data, examples, or references

Be thorough — this is the only context the next session will have
from our conversation.
```

3. **Auto-handoff flow** — after each provider response:
   - Estimate total tokens in `session.messages()`
   - If over threshold (configurable via `ONELOOP_HANDOFF_THRESHOLD`, default 85):
     - Print: `⚠ context window nearly full — wrapping up session...`
     - Send conversation + handoff prompt to the model
     - Save handoff to `.oneloop/handoff.md`
     - Print the handoff so the user can review
     - Automatically start a new session with the handoff as the first user message
     - User continues seamlessly

4. **Manual `/wrap` command** — in interactive mode, user can trigger this anytime:
   - `/wrap` — generate handoff, start new session
   - `/wrap <instructions>` — same but with custom handoff instructions

#### Changes to existing files

- **`session.rs`** — add `token_estimate()` method. Add ability to start a new session file (e.g. `2026-04-20-b.jsonl` when `2026-04-20.jsonl` gets too long).
- **`agent/mod.rs`** — after each provider response, check token estimate. If over threshold, trigger handoff flow.
- **`app.rs`** — support `/wrap` command in interactive mode. After handoff, restart the agent loop with the new session.
- **`config.rs`** — load `.oneloop/handoff.md` as part of the system prompt if it exists (so new sessions pick it up automatically).

#### Configuration

- `ONELOOP_HANDOFF_THRESHOLD` — percentage of context window to trigger auto-handoff (default: 85)
- `ONELOOP_NO_AUTOHANDOFF` — disable auto-handoff, only manual `/wrap`

#### What this avoids (vs. traditional compaction)

- ❌ No cumulative information loss — each session starts fresh with a full handoff
- ❌ No quality degradation — the model never works from a compressed summary mid-task
- ❌ No risk of "going off the rails" mid-compaction
- ❌ No information destruction — old sessions stay on disk untouched

#### What the user sees

```
  ✓ edit: src/app.rs (12 lines, 340 bytes)

⚠ context window nearly full (87%) — wrapping up session...

  ⏳ generating handoff...

Session handoff saved to .oneloop/handoff.md
Starting new session with handoff context...

> (continue working — the new session knows everything from before)
```

## Done

- [x] **Built-in tools** — read, write, edit, bash
- [x] **Web search tool** — SearXNG-backed `web_search` tool, configurable via `ONELOOP_SEARX_URL`
- [x] **Multiple providers** — Z.AI, OpenAI, Anthropic, mock fallback
- [x] **Ctrl+C to stop** — Agent loop checks a shared stop flag, Ctrl+C breaks out cleanly
- [x] **Interactive mode** — REPL with animated spinner
- [x] **Session persistence** — JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- [x] **AGENTS.md context** — Loaded as system prompt from project directory
- [x] **Auth** — API keys stored in `~/.oneloop/auth.json`
