# Directive System

Status: implemented.

Directives are runtime control metadata parsed from user input before any
provider call. They decide **which** provider(s) to use and **how** to
orchestrate them. The model never sees `#!`.

## Syntax

One pattern: `#!` opens, `#!` closes. Directive words go between, the user
message follows.

```text
#!directive words#! your message here
```

- No `#!` at all → default single mode, full input is the body.
- `#!...#!` → tokens between markers are parsed, text after closing `#!` is
  the prompt body sent to the model(s).

Directive words are space-separated tokens. Unknown tokens are errors.

## Tokens

### Provider names

```text
anthropic    openai    zai
```

Bare provider names. One provider → single mode. Two or more (without an
explicit mode) → consensus.

### Mode keywords

```text
consensus    debate
```

Explicit mode selection. Both require at least two providers. If omitted and
multiple providers are listed, consensus is assumed.

### Key:value modifiers

```text
judge:<provider>      Judge for final synthesis (consensus/debate only)
rounds:<1-3>          Number of critique/revision rounds (debate only)
tools:none            No tools during orchestration
tools:read,web_search Allow specific tools
format:md             Request markdown-formatted output
format:html           Request HTML-formatted output
```

## Examples

### Single provider

```text
#!anthropic#! review this function
```

Routes to Anthropic. Full agent loop — tools, session history,
auto-compaction.

### Consensus (implicit)

```text
#!anthropic openai#! should we use Lua plugins?
```

Two providers without explicit mode → consensus. Both answer in parallel,
first listed provider synthesizes.

### Consensus with judge

```text
#!consensus anthropic openai zai judge:openai#! Should we ship plugins first?
```

All three answer independently. OpenAI writes the final synthesis.

### Debate with rounds

```text
#!debate anthropic openai zai rounds:2 judge:anthropic#! Should we add hooks?
```

Initial answers → 2 rounds of critique/revision → Anthropic synthesizes.

### With tools

```text
#!anthropic openai tools:none#! compare these approaches
```

### Format control

```text
#!anthropic format:md#! summarize this file
#!format:html#! summarize the project
```

No provider specified → uses default provider selection order.

## Error handling

The parser fails before any provider call when:

- closing `#!` is missing
- directive tokens between `#!...#!` are empty
- prompt body after closing `#!` is empty
- a token is unknown (not a provider, mode, or key:value)
- incompatible combinations (e.g. `rounds:` with consensus, `judge:` with
  single provider)
- `debate` or `consensus` with fewer than two providers

## Interaction with skills

- Directives control runtime orchestration (which providers, which mode).
- Skills control model behavior (how to review code, how to brainstorm).
- They never overlap.

## Session behavior

- **Single provider**: full session history, multi-turn, auto-compaction.
- **Consensus/debate**: current prompt only, no session history sent, no tool
  loop. Labeled responses stored as assistant messages.

## Implementation

The parser in `src/directives.rs` is a single `parse_prompt()` function:

1. Check for `#!` prefix.
2. Find closing `#!`.
3. Split directive text into tokens.
4. Categorize tokens (providers, modes, key:value pairs).
5. Resolve mode from providers + explicit keyword.
6. Validate cross-constraints.
7. Return `PromptDirectives` struct.
