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

## Code Search

Use `semble search` to find code by describing what it does or naming a symbol/identifier, instead of grep:

​```bash
semble search "authentication flow" ./my-project
semble search "save_pretrained" ./my-project
semble search "save model to disk" ./my-project --top-k 10
​```

Use `semble find-related` to discover code similar to a known location (pass `file_path` and `line` from a prior search result):

​```bash
semble find-related src/auth.py 42 ./my-project
​```

`path` defaults to the current directory when omitted; git URLs are accepted.

If `semble` is not on `$PATH`, use `uvx --from "semble[mcp]" semble` in its place.

## Workflow

1. Start with `semble search` to find relevant chunks.
2. Inspect full files only when the returned chunk is not enough context.
3. Optionally use `semble find-related` with a promising result's `file_path` and `line` to discover related implementations.
4. Use grep only when you need exhaustive literal matches or quick confirmation of an exact string.
