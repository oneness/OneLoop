# Agent Instructions

## Principles

Read [`PRINCIPLES.md`](PRINCIPLES.md) first. Every decision should align with those principles. When in doubt, simplicity wins.

## Environment

- Always use `nix-shell -p <package>` to install any tools you need. Never assume pip, apt, or other package managers are available.
- Example: `nix-shell -p python3Packages.python-docx --run "python3 script.py"`
- Rust toolchain is available via the project flake (`nix develop`).

## Style Guide

See [`docs/style-guide.md`](docs/style-guide.md) for the full coding conventions. Key points:

- **Functional style**: prefer iterator chains (`.map()`, `.filter()`, `.collect()`) over imperative loops, unless you need `break`/`return`.
- **Borrow over clone**: use `&T` in params, avoid `.clone()` unless ownership transfer is needed.
- **Inline format args**: `format!("{name}")` not `format!("{}", name)`.
- **No `unwrap()`** outside tests. Use `.context()?` or `let ... else { bail!() }`.
- **Method references** over closures: `.map(String::len)` not `.map(|s| s.len())`.
- **Collapse single-arm match**: use `if let Some("clear") = ...` instead of matching.

## Linting

The project has strict `[lints.clippy]` config in `Cargo.toml`. Run before committing:

```sh

nix develop --command bash -c 'cargo clippy --all-targets -- -D warnings'
```

Do not silence warnings with `#[allow(...)]`. Fix them. If truly necessary, use `#[expect(clippy::lint_name)]` with a comment.

`unwrap_used` is a restriction lint, so it would otherwise fire in test modules as well. `clippy.toml` sets `allow-unwrap-in-tests = true`, and that file is the only place it can go — a `config` table beside the lint in `Cargo.toml` is silently ignored. Unwrapping in a test is fine: a helper that hit an unexpected error should abort, not pass an `Err` through and report a confusing failure. Outside tests it stays denied, and `expect()` is denied by the same lint, so production code returns a `Result`.

## Module Organization

- One concern per module. Split when files exceed ~300 lines.
- Private modules with explicit `pub use` re-exports.
- Environment variables follow `ONELOOP_` prefix.

## Testing

- Descriptive test names: `repair_closes_tool_calls_without_results()`.
- One assertion per test when practical.
- `#[cfg(test)] mod tests {}` in the same file.

## Edit calls

Use `edit` for one file at a time. Read the file with `read` immediately
before editing it: the anchor has to come from the lines the read actually
showed, not from memory.

- **One replacement per call.** Send `path`, `old_str`, `new_str`, and
  optionally `replace_all`. Several changes to one file are several calls;
  calls on one file take turns, but their order is not guaranteed — if one
  change depends on what another wrote, wait for the first result before
  sending the next.
- **Never send `old_str` equal to `new_str`.** The tool refuses no-ops.
- **Prefer one `write` when the whole file is changing.** Many hunks are one
  call to `write`, not many calls to `edit`.

## Search

Every repository question starts with OneSearch, not grep. Read
`~/.oneloop/skills/onesearch/SKILL.md` once per session for the invocation
(`~/repos/OneSearch/target/debug/onesearch`).

1. Ask by behavior, or name the symbol.
2. Pick the filter that holds the answer: `code` (default), `docs`, `config`,
   `session`, or `all`.
3. Read the returned chunks; open whole files only when they fall short.

Grep is for exhaustive literal checks, and for when OneSearch cannot run.
Reaching for it first is a wasted step, not a shortcut.

## Documentation

A change is not finished until the docs say the same thing.

- [`docs/mental-model.md`](docs/mental-model.md) — the single page: the path a message takes, with a diagram, and every fact that sits on that path.
- [`docs/index.html`](docs/index.html) — the product deck: one slide per mechanism. Update the slide a change touches in the same change.
- [`docs/style-guide.md`](docs/style-guide.md) — coding conventions, lint config, and the `ONELOOP_` variables.

When behavior, a tool, the module layout, or the shape of the loop moves, one
of these is wrong until you fix it.

A change that moves a module, a public contract or a dependency direction also
regenerates the mental model:

1. Run `./project-map/generate` — writes the gitignored `.oneloop/project-map.json`.
2. Use the `mental-model` skill to rewrite `docs/mental-model.md` from that artifact.
3. Run `./project-map/generate --check` and fix every error before finishing.

The check validates citations, not truth: the model still owns the prose. Never
hand-edit the artifact.
