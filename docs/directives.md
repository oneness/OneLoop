# Directive and Multi-Model Orchestration Spec

Status: implemented.

This document specifies oneloop's prompt directive system. Directives are
runtime control metadata parsed by oneloop before any provider call. They are not
sent verbatim to the model unless explicitly preserved as part of the user prompt.

## Goals

- Replace the removed `@provider` prefix with an extensible directive syntax.
- Support one-shot provider routing without environment variable changes.
- Support multi-model consensus/debate workflows.
- Keep provider/orchestration decisions deterministic and outside model control.
- Keep skills focused on model behavior, not runtime routing.
- Avoid accidental interpretation of normal Markdown as control metadata.

## Non-goals

- Skills are not responsible for choosing providers.
- Debate mode does not initially allow models to mutate the workspace.
- This spec does not define a general scripting language.
- This spec does not replace existing REPL commands such as `/clear`.

## Syntax

Directives use a `#!` prefix and must appear at the very start of the prompt,
before the user body.

```text
#!provider anthropic
Explain this module.
```

Multiple directives may appear as a contiguous block. In interactive REPL mode,
if the first line contains only directives and no body, oneloop prompts for body
lines and runs the combined prompt when you submit an empty line:

```text
#!consensus anthropic openai zai
· Should oneloop implement Lua plugins before markdown skills?
·
```

Inline REPL prompts also work:

```text
#!consensus anthropic openai zai Should oneloop implement Lua plugins before markdown skills?
```

One-shot mode supports quoted multi-line prompts:

```sh
./ol $'#!consensus anthropic openai zai\n#!judge anthropic\n\nShould oneloop implement Lua plugins before markdown skills?'
```

Directive parsing stops at the first non-directive line. Everything after the
directive block is the user prompt body.

This is valid:

```text
#!provider openai
# This Markdown heading is part of the user prompt.
Explain this heading.
```

This is not a directive, because the directive does not appear at the beginning
of the prompt:

```text
Please explain this shell script:
#!provider anthropic
```

## Grammar

The initial grammar is intentionally line-oriented:

```text
prompt          := directive_block? body
directive_block := directive_line* blank_line?
directive_line  := "#!" name args?
name            := ASCII identifier, `[a-z][a-z0-9_-]*`
args            := rest of line, trimmed
body            := remaining text
```

Unknown directives are errors. Invalid directive arguments are errors. The agent
should fail before making any provider call if directive parsing fails.

## Directives

### Provider shorthand

Provider names can be used directly after `#!`.

Single provider shorthand:

```text
#!anthropic Explain this module.
```

Equivalent to:

```text
#!provider anthropic Explain this module.
```

Multiple provider shorthand:

```text
#!anthropic openai zai Should oneloop use skills, plugins, or both?
```

Equivalent to:

```text
#!consensus anthropic openai zai Should oneloop use skills, plugins, or both?
```

### `#!provider`

Route a normal single-agent request to one provider.

```text
#!provider anthropic
Review this function.
```

Arguments:

```text
#!provider <provider>
```

Rules:

- `<provider>` must be configured and available in `ProviderRegistry`.
- The request uses the normal agent loop with tools enabled.
- This supersedes the removed `@provider` prefix.

### `#!consensus`

Ask multiple providers independently, then synthesize a final consensus.

```text
#!consensus anthropic openai zai
Should provider routing be a skill or a directive?
```

Arguments:

```text
#!consensus <provider> [provider...]
```

Rules:

- At least two providers are required.
- Initial provider calls should run in parallel.
- Each provider receives the same prompt body.
- Initial implementation uses no tools during provider responses.
- A judge provider synthesizes the final answer.
- If `#!judge` is omitted, use the first provider listed in `#!consensus`.

Output should clearly label each provider response and the final consensus.

Recommended final synthesis prompt shape:

```text
The user asked:

<original prompt>

Several models answered independently:

<provider A answer>
<provider B answer>
<provider C answer>

Synthesize a final consensus. Identify agreements, disagreements, tradeoffs,
and a practical recommendation. Do not simply average the answers; prefer the
best-supported reasoning.
```

### `#!debate`

Run a multi-round debate before final synthesis.

```text
#!debate anthropic openai zai
#!rounds 2
#!judge anthropic

Should oneloop add plugin hooks before skills?
```

Arguments:

```text
#!debate <provider> [provider...]
```

Rules:

- At least two providers are required.
- `#!rounds` controls the number of critique/revision rounds.
- If `#!rounds` is omitted, default to `1`.
- Debate mode ends with a judge synthesis.
- If `#!judge` is omitted, use the first provider listed in `#!debate`.
- Initial implementation uses no tools during debate rounds.

Suggested flow for `rounds = 1`:

1. Parallel independent answers from each provider.
2. Send all answers to each provider and ask for critique/revision.
3. Send the full debate transcript to the judge provider for final synthesis.

Suggested flow for `rounds = 2`:

1. Parallel independent answers.
2. Parallel critique/revision round 1.
3. Parallel critique/revision round 2 using prior critiques.
4. Judge synthesis.

### `#!judge`

Select the provider that writes the final consensus/debate synthesis.

```text
#!consensus anthropic openai zai
#!judge openai
```

Arguments:

```text
#!judge <provider>
```

Rules:

- Only valid with `#!consensus` or `#!debate`.
- The judge provider must be configured and available.
- The judge does not need to be one of the debate participants, though using a
  participant is the default and recommended behavior.

### `#!rounds`

Set the number of debate rounds.

```text
#!debate anthropic openai
#!rounds 2
```

Arguments:

```text
#!rounds <positive integer>
```

Rules:

- Only valid with `#!debate`.
- Default: `1`.
- Recommended maximum: `3`.
- Values above the maximum should produce a validation error unless explicitly
  configured later.

### `#!tools`

Control tool availability during orchestration modes.

```text
#!consensus anthropic openai
#!tools none
```

Arguments:

```text
#!tools none
#!tools read web_search
```

Rules:

- Initial default for `#!consensus` and `#!debate`: `none`.
- Initial default for `#!provider`: normal built-in tools.
- Mutating tools (`write`, `edit`, destructive `bash`) should not be enabled in
  debate/consensus mode for the first implementation.
- If tool support is added to debate later, prefer a shared evidence-gathering
  phase over allowing each provider to independently mutate or inspect state.

## Interaction with skills

Skills and directives solve different problems.

- Directives control runtime orchestration before model calls.
- Skills provide model-facing instructions after provider selection.

A skill may describe how to perform architecture review or code review, but it
must not be the primary mechanism for provider routing. Provider routing must be
parsed by oneloop before the prompt is sent to any model.

Valid combined usage:

```text
#!consensus anthropic openai zai
Use the architecture review skill. Should we ship Lua plugins before markdown skills?
```

## Removed provider-prefix syntax

The previous provider-prefix syntax is removed instead of preserved as a
compatibility alias.

```text
@anthropic Explain this file.
```

Rules:

- Inputs beginning with `@anthropic`, `@openai`, or another provider name are
  treated as normal user prompt text.
- Provider routing must use `#!provider`.
- New orchestration features must be added only to `#!` directives.

## Session behavior

For normal `#!provider` requests, session behavior remains unchanged.

For `#!consensus` and `#!debate`, recommended session behavior:

- Store the user's original prompt body once.
- Store labeled provider responses as assistant messages or a structured debate
  transcript message.
- Store the final synthesis as the final assistant message.
- Avoid interleaving hidden orchestration prompts into the visible conversation
  unless needed for debugging.

A future structured message type may be useful, but the current implementation
uses plain assistant markdown with clear headings.

## Output format

Consensus/debate output should be readable in a terminal and easy to scan.

Recommended shape:

```text
── Anthropic ──
<answer>

── OpenAI ──
<answer>

── Z.AI ──
<answer>

── Consensus ──
<final synthesis>
```

For debate mode with rounds:

```text
── Round 1: Initial Answers ──

── Anthropic ──
...

── OpenAI ──
...

── Round 2: Critiques/Revisions ──

── Anthropic ──
...

── OpenAI ──
...

── Final Consensus ──
...
```

## Error handling

The directive parser should fail fast before any provider call when:

- a directive is unknown,
- a required argument is missing,
- a provider is unavailable,
- incompatible directives are combined,
- `#!rounds` is invalid,
- `#!tools` requests unsupported tools.

Examples of incompatible directives:

```text
#!provider anthropic
#!consensus anthropic openai
```

```text
#!provider openai
#!rounds 2
```

## Implementation status

Implemented:

- `#!provider` parser support.
- provider shorthand, e.g. `#!anthropic` and `#!anthropic openai`.
- `#!consensus` with parallel no-tool calls and one judge synthesis.
- `#!judge`.
- `#!debate` with `#!rounds`.
- parse summaries before routed/directive runs.
- interactive directive body prompting when a directive has no body.

Known limitation:

- `#!tools read web_search` is parsed and validated, but orchestration does not
  yet run a full tool loop. Multi-model PR/code review should first gather shared
  evidence, then fan out to providers.
