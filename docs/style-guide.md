# OneLoop — Rust Style Guide

This guide defines the coding conventions for the OneLoop project. The goal is
clean, idiomatic, functional-leaning Rust that is consistent across the codebase.

## References

- [PRINCIPLES.md](../PRINCIPLES.md) — project soul and engineering direction
- [Apollo Rust Best Practices](https://github.com/apollographql/rust-best-practices)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Rust Analyzer Style Guide](https://rust-analyzer.github.io/book/contributing/style.html)
- [Rust Design Patterns](https://rust-unofficial.github.io/patterns/)

---

## Imports

Order: `std` → external crates → `crate`/`super`.

```rust
use std::io::{self, Write as IoWrite};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::HeaderMap;

use crate::agent::messages::ToolCall;
use crate::config::Config;
```

## Ownership & Borrowing

- **Prefer `&T` over `.clone()`** unless ownership transfer is genuinely needed.
- Use `&str` / `&[T]` in function parameters, not `String` / `Vec<T>`.
- Small `Copy` types (≤24 bytes) are fine to pass by value.
- When you do need to clone, use `.cloned()` / `.copied()` on iterators
  instead of `.map(|x| x.clone())`.

## Error Handling

- **`anyhow`** is our error type (we're a binary, not a library).
- Use `.context("...")` to add human-readable context at the call site.
- Use `?` to propagate errors — avoid verbose `match` chains.
- `bail!("...")` for early returns with a formatted error.
- **Never** use `unwrap()` / `expect()` outside tests. Use
  `let Some(x) = ... else { bail!("...") }` or `.with_context(|| ...)?`.
- `todo!()` is acceptable for stubs; `unreachable!()` when you've proven
  a branch can't be hit.

## Iterators & Functional Style

We lean **functional**. Prefer iterator chains over imperative loops:

```rust
// ✅ Preferred
let names: Vec<&str> = items.iter().filter(|i| i.active).map(|i| i.name.as_str()).collect();

// ❌ Avoid
let mut names = Vec::new();
for item in &items {
    if item.active {
        names.push(item.name.as_str());
    }
}
```

When to use `for` loops instead:
- You need `break`, `continue`, or early `return`.
- The body is mostly side-effects (I/O, logging).
- The iterator chain would be harder to read than the loop.

## Comments

- Comments explain **why**, never **what**.
- Use `// SAFETY:`, `// PERF:`, `// CONTEXT:` prefixes for specific concerns.
- Keep comments short. If you need a paragraph, link to a doc or issue.
- `// TODO(#42): description` — always reference an issue.
- Replace long comments with well-named helper functions.

## Documentation

- `///` doc comments on all public items (functions, structs, enums, traits).
- Include `# Examples`, `# Errors`, `# Panics` sections where relevant.
- `//!` at the top of module files for module-level docs.

## Formatting

- `cargo fmt` is law. No discussion.
- Inline format args: `format!("{name}")` not `format!("{}", name)`.
- Collapse single-arm `match` into `if let` or `if` (clippy will catch this).

## Module Organization

- One concern per module. If a file exceeds ~300 lines, consider splitting.
- Prefer private modules with explicit `pub use` re-exports.
- Module file structure:
  ```
  src/
    providers/
      mod.rs          ← pub use re-exports
      anthropic.rs    ← one provider
      openai.rs
      zai.rs
      registry.rs     ← orchestration
  ```

## Environment Variables

All env-based config follows the `ONELOOP_` prefix convention:

```
ONELOOP_PROVIDER           default provider name
ONELOOP_MAX_ITERATIONS     agent loop cap (default: 50)
ONELOOP_MAX_RETRIES        retry cap (default: 3)
ONELOOP_CONTEXT_WINDOW_TOKENS  token budget (default: 128000)
ONELOOP_COMPACTION_THRESHOLD  auto-compact trigger % (default: 85)
ONELOOP_SEARX_URL          SearXNG instance URL for web_search
```

## Clippy

Run before every commit:

```sh
cargo clippy -- -D warnings
```

The project's `[lints.clippy]` config in `Cargo.toml` enforces key lints.
Fix warnings, don't silence them. If you must suppress, use
`#[expect(clippy::lint_name)]` with a comment explaining why.

## Testing

- Test names describe behavior: `compact_preserves_recent_user_messages()`.
- One assertion per test when practical.
- `assert_eq!(got, expected)` — expected on the right.
- Use `#[test]` unit tests in the same file via `#[cfg(test)] mod tests {}`.

## Naming

- `snake_case` for functions, variables, modules, files.
- `PascalCase` for types, traits, enums.
- `SCREAMING_SNAKE_CASE` for constants and statics.
- Booleans read as assertions: `is_empty`, `has_system`, `should_compact`.
- Builder / constructor methods: `new`, `open_or_create`, `with_builtin_tools`.
