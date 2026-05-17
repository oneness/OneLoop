# TODO

- [ ] **Skills** — Dedicated `skill` tool. Scan `.oneloop/skills/*.md` and `~/.oneloop/skills/*.md` at startup. Embed skill names + descriptions in the tool's description so the model can pick the right one. Model calls `skill("name")` → agent reads the file → returns content as tool result. No directives, no user action needed. The model decides when a skill is relevant.

- [ ] **Memory** — Persistent `.oneloop/memory.md` that survives compaction and new sessions. Extracts facts from conversations, injects into system prompt. Compounds over time.
