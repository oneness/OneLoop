# TODO

## Near term

- [ ] **Lua plugin system** — REPL-driven plugin system using `mlua`. Plugins are `.lua` files in `.oneloop/plugins/`. They subscribe to lifecycle hooks and can inject context, block/confirm tool calls, and persist state across sessions. Core agent code does not change — plugins are loaded dynamically at startup. See [`docs/plugins.md`](docs/plugins.md) for the full design.

- [ ] **Memory plugin** — Persistent `.oneloop/memory.md` that survives compaction and new sessions. Extracts facts from conversations during compaction, injects memory into every API call's system prompt. Compounds over time — after a week of use, the agent knows your project, preferences, and decisions without being told. Example plugin included in [`docs/plugins.md`](docs/plugins.md).

- [ ] **Safety plugin** — Intercepts tool calls before execution. Blocks dangerous bash commands (`rm -rf /`, fork bombs), warns on writes outside the project directory, requires confirmation on destructive operations (`DROP TABLE`, `git push --force`), blocks reads of sensitive files (SSH keys, `.env`). Example plugin included in [`docs/plugins.md`](docs/plugins.md).

- [ ] **Skills loader** — Walk `.oneloop/skills/*.md` and `~/.oneloop/skills/*.md`, concatenate, append to system prompt. Makes oneloop extensible without Rust changes. Users drop markdown files to teach the agent new capabilities through natural language.


## Done

- [x] **Built-in tools** — read, write, edit, bash
- [x] **Web search tool** — SearXNG-backed `web_search` tool, configurable via `ONELOOP_SEARX_URL`
- [x] **Multiple providers** — Z.AI, OpenAI, Anthropic
- [x] **Ctrl+C to stop** — Agent loop checks a shared stop flag, Ctrl+C breaks out cleanly
- [x] **Interactive mode** — REPL with animated spinner
- [x] **Session persistence** — JSONL at `.oneloop/sessions/YYYY-MM-DD.jsonl`
- [x] **AGENTS.md context** — Loaded as system prompt from project directory
- [x] **Auth** — API keys stored in `~/.oneloop/auth.json`
- [x] **Session rotation** — `/clear` rotates to a new session file (`YYYY-MM-DD-001.jsonl`, etc.). Old sessions preserved on disk. Latest session auto-opened on restart.
- [x] **Codebase refactor** — Reorganized to use `name.rs` + `name/` module pattern. Providers split into individual files. Dead code removed.
- [x] **Retry with fallback** — `complete_with_retry` retries up to 3 times with linear backoff, then interactively prompts for alternative provider. Configurable via `ONELOOP_MAX_RETRIES`.
- [x] **Auto-compaction** — Codex-style compaction at 85% context window. Structured handoff summary, recent user messages preserved verbatim. Compaction warning to user.
- [x] **Project style guide** — `docs/style-guide.md` with coding conventions. Strict clippy config in `Cargo.toml`. All violations fixed.
- [x] **Parallel tool execution** — Multiple tool calls from a single API response execute concurrently via `tokio::spawn` + `join_all`. Results collected in order for sequential session logging.
- [x] **Per-session metrics** — `.oneloop/metrics/<session>.jsonl` with `api_call`, `tool_exec`, `compaction` events. One file per session, rotates with session. Non-fatal error handling.
