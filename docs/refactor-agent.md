# Refactoring `agent.rs`

Status: decided, not started (2026-11-06). Plan recorded here so the work can be
picked up without re-deriving it.

`src/agent.rs` is 1249 lines and holds six unrelated concerns. This is the plan
to split it. Nothing here changes behavior except step 7, which adds the tests
the loop has never had.

## Why

The file is the largest in the tree by a third, but length is the symptom. The
reason it grew is that `agent` doubles as the crate's shared-vocabulary module:
two dependency arrows point backwards into it, so everything that needed a
`Message` or an `AgentContext` imported it from the agent.

- `tools/*.rs` (7 files) import `crate::agent::AgentContext`, while `agent`
  imports `tools::ToolRegistry`.
- `providers*.rs`, `claude/client.rs`, `antigravity/client.rs` import
  `crate::agent::messages::*`, while `agent` imports `providers`.

Neither is a compiler problem inside one crate. Both make `agent.rs` the place
where unrelated things accumulate.

## What is in the file now

| lines | contents | depends on |
|---|---|---|
| 1–28 | imports, `pub mod` decls | — |
| 30–33 | `AgentContext { cwd }` | nothing |
| 34–130 | **trace rendering**: `salient_field`, `TRACE_WIDTH`, `ELISION`, `first_line`, `elide`, `format_tool_call`, `skill_file` | `path`, `tools::skill` |
| 131–208 | **tool batching**: `run_batch` (per-file locks), `file_key` | `tools`, `path` |
| 209–327 | **duplicate-read suppression**: `read_payloads_in_context`, `payload_digest`, `suppress_duplicate_reads`, `duplicate_read_notice` | `output`, `path`, `sha2` |
| 329–347 | `Stop`, `cap_notice` | — |
| 349–700 | `Agent`: constructors, accessors, `reload_models`, `clear_session`, the loop (`run_once_with_model`, 126 lines), `remaining_context`, `log_api_call`, `execute_tool_calls` | everything |
| 701–755 | **banner**: `summary`, `VALUE_COLUMN`, `SUMMARY_WIDTH`, `packed` | `models`, `path` |
| 756–1249 | tests | — |

## Three findings that shaped the plan

**Forty percent is tests, and they cluster on the seams.** 494 of the 1249 lines
are `#[cfg(test)]`, sitting above trace (175), duplicate reads (215), banner
(20) and batching (20). The real code is 755 lines. Each existing test group
travels with its subject, so most commits are contiguous moves.

**There is exactly one shared seam.** `file_key` has three callers that land in
two different destination files (`run_batch`, and both transcript scans in
duplicate reads), and it calls `salient_field`, which `format_tool_call` also
calls. A naive trace / batch / dedup cut orphans `salient_field` and `file_key`
in a fourth place. The honest split is: `salient_field` is registry knowledge
(which argument names a tool) and lives in `tools.rs`; `file_key` is the lock
key and lives with the lock in `tools/batch.rs`.

**The loop has no test at all.** No test double exists for `ModelRegistry`, so
`run_once_with_model` — the project's stated core — is the one untested stretch
in the file, and 126 lines of it mix the iteration cap, request assembly, retry
bookkeeping, session pushes, `println!`, metrics and cap reporting. That is why
step 7 exists and why it is the only step that is not a move.

## Decision

Cut along the dependency direction first (so `tools` and `messages` stop
importing `agent`), then break up the loop.

`app.rs` needs **zero edits**: `agent.summary()`, `agent.messages()` and
`agent.run_once*` keep their paths and signatures, because an `impl Agent` block
in a submodule defines methods on the same type.

## Target tree

```
src/
  messages.rs           ~46   Message, User/Assistant/ToolCall/ToolResultMessage  ← moved verbatim
  tools.rs             ~140   Tool trait, ToolRegistry, AgentContext, salient_field (the per-tool argument table)
  tools/
    batch.rs            ~80   run_batch, file_key  (+ the edit-lock test)
    read/write/edit/bash/elisp/skill.rs            (unchanged apart from import lines)
  agent/
    mod.rs             ~180   Agent: state, constructors, accessors, reload/repair/clear, context gauge
    turn.rs            ~280   run_once*, request, record, Stop, cap_notice, execute_tool_calls, log_api_call
    trace.rs           ~272   format_tool_call, elide, first_line, skill_file  (+173 test lines)
    duplicate_reads.rs ~334   suppress_duplicate_reads, digest, notice  (+216 test lines)
    banner.rs           ~75   impl Agent { summary } + packed  (+20 test lines)
    session.rs metrics.rs spinner.rs   (import lines only)
```

## Steps

One commit each. Every step compiles and passes tests on its own.

Steps 1–6 are pure moves, and "pure" is a constraint rather than a hope: the
functions are copied verbatim **with their doc comments**, no comment is
rewritten, trimmed or retitled on the way, and nothing but visibility changes. A
move that also edits logic cannot be reviewed as a move. Visibility is
`pub(super)` within `agent/` and `pub(crate)` where `agent` reaches into `tools`.

Each new file holds one concern and one mechanism: free functions, or a single
`impl Agent` block. No extension traits, no wrapper types.

Order matters in two places: step 3 precedes step 4 (`trace.rs` imports
`salient_field` from `tools.rs`) and step 6 (`duplicate_reads.rs` imports
`file_key` from `tools::batch`). Steps 4 and 5 have no dependants and can be
swapped.

A *concern* here is one cluster from the inventory table above — a single
responsibility that currently shares the file with five others.

**The milestone is the end of step 3**, the point where the arrows have turned.
After it, the only reference to `agent::` from outside `src/agent/` is `app.rs`'s
`use crate::agent::Agent`, which is the one consumer that should point that way.
Today there are 18.

```sh
grep -rn "agent::" --include=*.rs src/ | grep -v "^src/agent/" | grep -v "^src/app.rs"
```

Match `agent::`, not `crate::agent::`: four tool files import it nested, as
`use crate::{agent::AgentContext, path::resolve_path}`, so the longer pattern
reports three files where there are six. Steps 4–7 rest on this shape, so run
the check before starting step 4.

1. **`messages.rs` → `src/messages.rs`.** `git mv`, then exactly 13 import
   lines and two declarations: `mod messages;` in `main.rs`; drop
   `pub mod messages` from `agent/mod.rs`; `crate::agent::messages` →
   `crate::messages` in `providers.rs` (×2, one inline in `ProviderRequest`),
   `providers/chat.rs` (×2), `providers/responses.rs` (×2),
   `claude/client.rs` (×2), `antigravity/client.rs` (×2),
   `agent/metrics.rs` (×1); `super::messages` → `crate::messages` in
   `session.rs` and `metrics.rs`. First, so no later step edits an import twice.

   The on-disk session format does not move with it: a `Message` serializes by
   variant and field name, never by module path, so v1.0 session files load
   unchanged.

2. **`AgentContext` → `tools.rs`.** Defined beside the `Tool` trait that takes it
   as its `execute` parameter, in no module of its own; six tool files switch to
   `crate::tools::AgentContext`, and `agent/mod.rs` imports it from there. This
   is what turns the `tools → agent` arrow around.

3. **`run_batch` + `file_key` → `tools/batch.rs`; `salient_field` → `tools.rs`.**
   The edit-lock test travels with `run_batch`.

4. **`trace.rs`.** The rendering block and its 173 test lines.

5. **`banner.rs`.** `impl Agent { summary }`, `packed`, `VALUE_COLUMN`,
   `SUMMARY_WIDTH`, and the two wrapping tests.

6. **`duplicate_reads.rs`.** The four items and their 216 test lines; imports
   `file_key` from `tools::batch` — the same key the batch lock uses, which is
   what its doc comment already says.

7. **`turn.rs`.** Moves `run_once`, `run_once_with_model`, `execute_tool_calls`,
   `log_api_call`, `Stop`, `cap_notice`, and adds the two pure decisions the
   loop currently makes inline:

   - `record(&ProviderResponse) -> Record { AsIs, ReasoningOnly, Fallback }` —
     the empty-content / reasoning-only / give-up branch that has never been
     tested;
   - `continues(&ProviderResponse) -> bool` — true only when the response
     carries tool calls, which is invariant 5 below. `Stop` keeps its three
     variants unchanged; the loop sets `Stop::Resolved` when `continues` is
     false.

   `request()` becomes
   `async fn request(&mut self, active: &mut Option<String>) -> Result<Option<ProviderResponse>>`:
   request assembly, retry and fallback, spinner callbacks, the context-overflow
   note, the `api_call` metric, and `set_active`. `Ok(None)` means the provider
   refused and has already said so.

   `ProviderResponse` derives `Debug + Clone` with public fields, so `record` and
   `continues` are testable with no fake provider. That is the point of this
   step.

## Verification per commit

```
nix develop --command bash -c 'cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test'
```

The suite is **293 tests** today, 24 of them in `agent.rs`. Steps 1–6 move
tests without adding or removing any, so the total stays 293 and no test may be
deleted to make a move compile. Per-module filters shift as tests relocate — the
edit-lock test leaves `agent::tests` for `tools::batch::tests` in step 3 — so
`cargo test agent::` dropping by one is expected, the suite total is not. Step 7
adds roughly five.

## Manual smoke, after step 7

Tests cover units; none of them runs a provider, a session file or a terminal. So
one interactive turn and one one-shot, before and after, is the only check on
what a move can still quietly change:

1. `ol` in a project that already has a session on disk: the previous
   conversation loads, the startup summary renders, and a `read` still traces as
   `read: src/agent.rs`.
2. A turn that calls a tool: the trace line appears, the assistant text lands on
   **stdout** only, and the session file gains user → assistant → tool call →
   tool result in that order.
3. `ol "say hi"`: a one-shot writes no session file, no metrics line, and no
   `.oneloop/` where none existed.

Then `diff` one `api_call` line from `.oneloop/metrics/` before against after:
same keys, same shapes, `input_tokens` still the server's count or `null`.

## Behavior that must not change (step 7 only)

Steps 1–6 cannot regress anything: they move functions and widen visibility.
Step 7 rewrites the one untested stretch of the program, so these invariants are
the review checklist for that commit. Each is currently true, each is observable,
and none is covered by a test today.

1. **Fallback pinning.** `active_model` is set to the model that actually
   answered, and is passed as the pin to the *next* request. A run that fell back
   therefore stays on the fallback model for the rest of the prompt. Dropping
   this silently changes retry behavior mid-run.
2. **`api_call` metric fields.** On failure, `model` is the *requested* model and
   `input_tokens` is `null`; on success, `model` is the model that answered and
   `input_tokens` is the server's count (which may itself be `null`). `model_id`
   is `"?"` for an alias the registry cannot resolve. `tokens_estimated` is
   measured before the request and `duration_ms` spans it. These lines are
   written to `.oneloop/metrics/*.jsonl`, so they are persisted behavior, not
   console noise.
3. **`last_input_tokens`** is set only on a successful response, from
   `input_tokens`, and cleared by `/clear`; the context gauge prefers it over the
   estimate until then.
4. **The fallback sentence is byte-identical**: `I wasn't able to generate a
   response. Please try again or rephrase.` Rewording it is a behavior change.
5. **`continues` is `tool_calls.is_empty()` and nothing else.** An empty-content
   response carrying tool calls must still run the tools, so this decision must
   not be derived from `record`.
6. **`record` classification.** Push with reasoning when the content is
   non-empty *or* reasoning is present; print only non-empty content; the
   fallback branch requires content empty **and** reasoning absent **and** no
   tool calls.
7. **Provider failure path order.** Report the error, then the context-overflow
   note, then end the turn *without* propagating: the run returns `Ok`. A fatal
   error — `set_active` failing, a session write failing — still propagates.
8. **`execute_tool_calls` ordering.** Every tool call is pushed before any
   result; one `tool_exec` metric per batch; duplicate-read suppression rewrites
   results *before* they are pushed; one trace line per result, in call order.
9. **Session entry order** on disk is unchanged: user, assistant, tool calls,
   tool results.

## Doc edits (with step 6 or 7)

- [`architecture.md`](architecture.md) `:360` — `agent.rs::format_tool_call` →
  `agent/trace.rs::format_tool_call`
- [`architecture.md`](architecture.md) `:411-412` — the layout block changes the
  `agent.rs` line to `agent/mod.rs` and gains `messages.rs`, `agent/turn.rs`,
  `agent/trace.rs`, `agent/duplicate_reads.rs`, `agent/banner.rs` and
  `tools/batch.rs`, drops `agent/messages.rs`, and the `tools.rs` line learns
  about `AgentContext` and the argument table
- [`index.html`](index.html) — nothing; the deck never names the file

## Judgment calls

Recorded so they can be revisited, not re-argued:

1. **`salient_field` goes in `tools.rs`, not `tools/batch.rs`.** It is the table
   of which argument names a tool — registry knowledge with two consumers, the
   trace label and the lock key. `file_key` stays in `batch.rs` with the lock it
   keys.
2. **`project_dir` is duplicated into the `trace.rs` and `tools/batch.rs` test
   modules.** Every existing test module in the repo (`session`, `skill`,
   `prompt`, `read`, `edit`) already defines its own copy; following that beats
   inventing a shared `test_support` module for six lines.
3. **Step 7 moves the turn, not just helpers.** `mod.rs` otherwise lingers at
   ~420 and the split buys less than it costs.
4. **`request` returns `Result<Option<_>>`** rather than defining a new error
   type to separate a refusal from a fatal error. Settles when step 7 is
   written, not before.
5. **No traits, no `ToolExecutor`, no plugin seam.** Only files and
   `pub(crate)`.
6. **`AgentContext` lives in `tools.rs`, not a module of its own.** It is four
   lines including its derive, and it is the `Tool::execute` parameter, so it
   reads beside the trait rather than in a file that exists only to hold it.
   Settled during review: `tools.rs` ends at ~140 lines folded, against ~129 plus
   a 25-line companion file split out. One file wins. The same question for
   `banner.rs` (75 lines) settles at step 5, when `mod.rs` can be read without
   the summary block in it.

## Not doing

- No behavior change in steps 1–6.
- No new abstraction over `Tool` or `ModelRegistry`.
- No summarization, no truncation, no session-format change.
- No change to `app.rs`, `session.rs` behavior, or the session/metrics layout.
