# What

My personal coding assistant, built to solve the following problems in
commercial and open source coding agents:

1. Abstractions, features, and UI/UX that stand between coding agents and
   LLMs.
2. Black box prompt engineering and vendor lock-in.
3. Keeping judgement of an LLM's reckoning grounded, by having full source
   control over how I interact with coding models.

I recommend you build your own coding agent — [here is why I use
mine](https://www.birkey.co/2026-10-24-why-i-use-my-own-coding-agent.html).

**Curious?**
[Overview](https://www.birkey.co/oneloop/) ·
[Architecture](docs/architecture.md)

**Want to try?**
```bash
curl --fail --silent --show-error --location \
  https://raw.githubusercontent.com/oneness/OneLoop/main/install.sh | sh
```

That installs `oneloop` and its `ol` alias into `~/.local/bin`, and says so if
that directory is not on `PATH`. From a checkout, `./ol` builds and runs the
working tree, and `./ols` starts the pinned local model server.

# Contributing

Sorry. No pull requests.

# License

MIT — see [LICENSE](LICENSE).
