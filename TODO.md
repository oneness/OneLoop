# TODO

## Near term

- [ ] **Skills loader** — Walk `.oneloop/skills/*.md` and `~/.oneloop/skills/*.md`, concatenate, append to system prompt. Makes oneloop extensible without Rust changes. Users drop markdown files to teach the agent new capabilities through natural language.

- [ ] **Config-driven tools** — A `.oneloop/tools/*.toml` directory where users declaratively define tools (name, description, JSON schema, exec command). The registry reads these at startup, generates `ToolDefinition` entries, and interpolates arguments into the exec command via bash. First-class tool visibility without writing Rust code.

## Later

- [ ] **WASM plugins** — For when skills and config-driven tools aren't enough. A `.wasm` file in `~/.oneloop/plugins/` implements a simple ABI (`name()`, `description()`, `schema()`, `execute(json) -> json`). Language-agnostic, sandboxed. Only needed for actual computation logic that can't be expressed as a shell command or prompt recipe.

- [ ] **RPC mode** — Expose the agent loop over a socket/HTTP so editors and other tools can drive it programmatically.

- [ ] **Prompt templates** — Named reusable prompt patterns (e.g. "refactor", "debug", "explain") stored in `.oneloop/templates/`.

- [ ] **Session branching** — Fork a session at an arbitrary point to explore alternative paths without losing the original conversation.

- [ ] **Session compaction** — Summarize old messages to keep the context window small while preserving intent.

- [ ] **TUI polish** — Richer terminal UI with colors, markdown rendering, progress bars.

## Done

- [x] **Built-in tools** — read, write, edit, bash
- [x] **Web search tool** — SearXNG-backed `web_search` tool, configurable via `ONELOOP_SEARX_URL`
- [x] **Multiple providers** — Z.AI, OpenAI, Anthropic, mock fallback
- [x] **Ctrl+C to stop** — Agent loop checks a shared stop flag, Ctrl+C breaks out cleanly
- [x] **Interactive mode** — REPL with animated spinner
- [x] **Session persistence** — JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- [x] **AGENTS.md context** — Loaded as system prompt from project directory
- [x] **Auth** — API keys stored in `~/.oneloop/auth.json`
