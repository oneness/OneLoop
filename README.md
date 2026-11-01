# What

My personal coding assistant, built to solve the following problems in
commercial and open source coding agents:

1. Abstractions, features, and UI/UX that stand between coding agents and
   LLMs.
2. Black box prompt engineering and vendor lock-in.
3. Keeping my judgement of an LLM's reckoning grounded, by having full source
   control over how I interact with coding models.

# How

Just one simple REPL-style main loop with a minimal set of tools.

# Want to try?

1. Make sure you have a Rust toolchain or Nix installed.
2. Run:

   ```bash
   ./ol login opencode-go  # recommended
   ./ol
   ```

# Development

```bash
nix develop
cargo check
```

**Deeper:**
[Overview](https://www.birkey.co/oneloop/) ·
[Architecture](docs/architecture.md) ·
[Style guide](docs/style-guide.md)

# Contributing

Pull requests not accepted. I recommend you build your own coding agent —
[here is why I use mine](https://www.birkey.co/2026-10-24-why-i-use-my-own-coding-agent.html).

# License

MIT — see [LICENSE](LICENSE).
