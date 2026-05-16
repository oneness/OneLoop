# Agent Instructions

## Principles

Read [`PRINCIPLES.md`](PRINCIPLES.md) first. Every decision should align with those principles. When in doubt, simplicity wins.

## Environment

- I use NixOS.
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
cargo clippy -- -D warnings
```

Do not silence warnings with `#[allow(...)]`. Fix them. If truly necessary, use `#[expect(clippy::lint_name)]` with a comment.

## Module Organization

- One concern per module. Split when files exceed ~300 lines.
- Private modules with explicit `pub use` re-exports.
- Environment variables follow `ONELOOP_` prefix.

## Testing

- Descriptive test names: `compact_preserves_recent_user_messages()`.
- One assertion per test when practical.
- `#[cfg(test)] mod tests {}` in the same file.
