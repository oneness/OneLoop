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

## Docs

A change is not finished until the docs say the same thing.

- [`docs/index.html`](docs/index.html) — the product deck: slides plus a codebase snapshot. Update the affected slide and the snapshot numbers in the same change.
- [`docs/architecture.md`](docs/architecture.md) — source layout and mechanisms.

When behavior, a tool, the module layout, or the shape of the loop moves, one
of these is wrong until you fix it.
