# TODO

- [ ] **Skills** — Dedicated `skill` tool. Scan `.oneloop/skills/*.md` and `~/.oneloop/skills/*.md` at startup. Embed skill names + descriptions in the tool's description so the model can pick the right one. Model calls `skill("name")` → agent reads the file → returns content as tool result. No directives, no user action needed. The model decides when a skill is relevant.

- [x] **Memory** — `.oneloop/memory.md`, a single markdown file the agent reads and writes. Loaded into system prompt at startup (alongside AGENTS.md). During compaction, the model extracts worth-keeping facts and appends them. Capped at ~200 lines — oldest entries trimmed. No vector DB, no embeddings. The model decides what to remember.
