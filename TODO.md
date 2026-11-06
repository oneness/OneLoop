# TODO

- [x] **Feature freeze** — DONE
- [x] **Release 1.0** — Ship OneLoop v1.0
- [ ] **[Refactor agent.rs](docs/refactor-agent.md)** — split the 1249-line
      `src/agent.rs`: move `messages` to the crate root, give `tools` its
      `AgentContext` and batching, then break up the loop into `agent/turn.rs`.
      Plan, steps and judgment calls are in the linked file.
