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

nix develop --command bash -c 'cargo clippy -- -D warnings'
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

## Code Search

Use the OneSearch skill linked at `.oneloop/skills/onesearch.md` for repository search. It documents invoking the local binary at `~/repos/OneSearch/target/debug/onesearch`, selecting `code`, `docs`, `all`, or `session` filters, and following result chunks. Prefer OneSearch over grep for semantic questions and symbol lookups; use literal grep only for exhaustive exact-string checks or when OneSearch is unavailable.

## Workflow

1. Start with a OneSearch query that describes behavior or names the symbol.
2. Choose `code`, `docs`, `all`, or `session` based on the material needed.
3. Inspect full files only when returned chunks lack sufficient context.
4. Use grep only for exhaustive literal matches or when OneSearch is unavailable.
