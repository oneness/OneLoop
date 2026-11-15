# OneLoop — Rust Style Guide

This guide defines the coding conventions for the OneLoop project. The goal is
clean, idiomatic, functional-leaning Rust that is consistent across the codebase.

OneLoop is a binary, not a library. API-design guidance here is a strong
recommendation, not a contract: favor simplicity and directness over broad
reusability, avoid premature generalization, and solve the problem in front of
you. (After the [Google Rust style guide](https://google.github.io/styleguide/rustguide.html).)

## References

- [PRINCIPLES.md](../PRINCIPLES.md) — project soul and engineering direction
- [Apollo Rust Best Practices](https://github.com/apollographql/rust-best-practices)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- [Rust Analyzer Style Guide](https://rust-analyzer.github.io/book/contributing/style.html)
- [Rust Design Patterns](https://rust-unofficial.github.io/patterns/)

---

## Imports

Order: `std` → external crates → `crate`/`super`. One module per `use`
statement: no `::` inside the `{...}` block (`use foo::{a, b}`, never
`use foo::{bar::Baz}`). (rustfmt's `imports_granularity = "Module"` would
enforce this, but it is nightly-only and this project pins stable, so it
stays a review convention.)

```rust
use std::io::{self, Write as IoWrite};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::HeaderMap;

use crate::messages::ToolCall;
use crate::providers::{ProviderRequest, ProviderResponse};
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
- Name the file and its leaf module after the primary item inside
  (`providers/chat.rs` holds the Chat Completions provider, not a grab bag).
- Prefer private modules with explicit `pub use` re-exports.
- Module file structure:
  ```
  src/
    providers.rs      ← provider type, shared request/response types, error helpers
    providers/
      chat.rs         ← the Chat Completions provider
    models.rs         ← model registration and selection
    models/
      retry.rs        ← retry, then fall back to another model
  ```

### Dependencies form a tree, not a graph

Each module depends only on things *below* it. Nothing depends back up, and no
two modules at the same level depend on each other through a third.

Concretely: `app` depends on `agent`, `claude`, `antigravity`, `models`,
`config`, `output`, and `tools` — everything below it, never the reverse. The
leaves (`messages`, `path`, and `output`) import nothing internal at all. The
shape is a DAG, and it should stay one — a cycle means two modules cannot be
understood, or tested, without the other.

Two rules keep it that way:

- **A child never reaches up for its parent's internals.** `use super::X` is
  fine and normal — the parent of a nested module is also the barrel that
  declares it, so `super` means "the namespace my siblings share," not "the
  layer above me." A tool taking `Tool` and `ToolResult` from `super` is
  reading its siblings' shared vocabulary, not calling upward. What is *not*
  fine is a module reaching past its siblings to a shared dependency to name
  a type they own: if `app` needs a type that only `claude` and `antigravity`
  use, it takes it from one of *them*, which is why both re-export `Request`
  and `DEFAULT_INSTRUCTION`.
- **A shared helper goes in the lowest module that owns the concept.** Two
  modules that need the same logic take it from their common dependency rather
  than duplicating it, and the dependency does not learn about either of them
  back — `review.rs` holds the second-opinion mechanism and imports only
  `messages` and `status`, never `app`.

A cheap check when adding a module: name what it may import, then confirm
nothing in that list imports *it*. If something does, one of the two is in the
wrong place.

## Environment Variables

All env-based config follows the `ONELOOP_` prefix convention:

```
ONELOOP_MODEL              model alias for this run
ONELOOP_MAX_RETRIES        retry cap (default: 10)
ONELOOP_WEB_TOOLS          OpenRouter server-side web search/fetch (default: false
                           unless a provider or model sets `web_tools`)
ONELOOP_ORIGINAL_DIR       the directory the user was in, when a wrapper changed
                           into the checkout before running (`ol`, `ols`)
```

One more is read outside the binary: `ONELOOP_BIN`, which is where `install.sh`
puts it (`~/.local/bin` by default). It is an installation path, not a run-time
setting.

## Clippy

Run before every commit:

```sh
cargo clippy --all-targets -- -D warnings
```

The project's `[lints.clippy]` config in `Cargo.toml` enforces key lints.
Fix warnings, don't silence them. If you must suppress, use
`#[expect(clippy::lint_name)]` with a comment explaining why.

`unwrap_used` is a restriction lint, so it fires in tests too unless told
otherwise. `clippy.toml` sets `allow-unwrap-in-tests = true`, and that file is
the only place it can go — a `config` table beside the lint in `Cargo.toml` is
an unused manifest key, silently ignored. Tests unwrap freely: a helper that
hits an unexpected error should abort rather than pass an `Err` along and
report a misleading failure. Production code returns a `Result`; `expect()` is
denied by the same lint. Delete `clippy.toml` and the test suite goes back to
failing lint, so it belongs with the code.

`--all-targets` is not optional. Without it clippy never compiles the test
modules, so the tests holding the suite together go unchecked.

## Testing

- Test names describe behavior: `repair_closes_tool_calls_without_results()`.
- One assertion per test when practical.
- `assert_eq!(got, expected)` — expected on the right.
- Use `#[test]` unit tests in the same file via `#[cfg(test)] mod tests {}`.

## Naming

- `snake_case` for functions, variables, modules, files.
- `PascalCase` for types, traits, enums.
- `SCREAMING_SNAKE_CASE` for constants and statics.
- Booleans read as assertions: `is_empty`, `has_system`, `is_context_overflow`.
- Builder / constructor methods: `new`, `open`, `from_spec`, `with_builtin_tools`.
