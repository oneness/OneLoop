# Mental model: one message, end to end

`architecture.md` is the reference — every mechanism, every reason. This is the
map: the single path a message takes, from the line you type to the answer that
comes back.

**In one sentence:** a turn is one or more rounds of *request → answer → run
the tools it asked for*, ending on an answer that asks for no tools.

```
  you type a line
        |
        v
+---------------------------------------------------------------+
| app.rs  run_interactive                                       |
|   rustyline reads the line   (Ctrl+C discards the draft)      |
+-------------------------------+-------------------------------+
                                |
                          parse_input
                 +--------------+---------------+
                 |                              |
            a /command or !cmd                 a prompt
                 |                              |
        /clear /compact /model                  |
        /help /cc /ca /cn /ag /aga /agn !cmd    |
                 |                              |
                 +--------------+---------------+
                                |
                                v
+---------------------------------------------------------------+
| app.rs  run_interactive_turn                                  |
|   Ctrl+C races the turn and wins by dropping it               |
+-------------------------------+-------------------------------+
                                |
        (1) TurnStatus::new() ---------> OSC 7501: state=working
        (2) session.push_user(prompt)
                                |
                                v
+===============================================================+
| turn.rs  run_once_with_model        THE TURN LOOP             |
+===============================+===============================+
                                |
        (3) request()           |
            spinner "computing..."                 <------------+
            estimate tokens                                     |
            when_slow -> "▸ computing... · ~Nk tokens"          |
            ProviderRequest { system, messages, tools,          |
                              session id }                      |
            complete_with_retry                                 |
                retry up to N, then offer another model         |
            metrics: api_call { model, ms, tokens, purpose }    |
                                |                               |
                                v                               |
                          what came back?                       |
                 +--------------+---------------+               |
                 |              |               |               |
            Answered        Refused         Overflow            |
                 |              |               |               |
                 |              |         compact() then        |
                 |              |         continue  (once       |
                 |              |         per turn)             |
                 |              v               |               |
                 |            turn over         |               |
                 v                              |               |
        (4) keep(response)                      |               |
            what is in it?  (record)            |               |
              content   -> push_assistant       |               |
                           + print to stdout    |               |
              reasoning -> push_assistant only  |               |
              calls     -> write nothing        |               |
              nothing   -> fallback sentence    |               |
                 |                              |               |
                 v                              |               |
        (5) does it ask for tools?              |               |
              no  ---> turn over                |               |
              yes -+                            |               |
                   |                            |               |
                   v                            |               |
        (6) execute_tool_calls                  |               |
            push_tool_call  (each, before)      |               |
            when_slow -> "▸ <what is running>"  |               |
            (7) batch.rs run: every call in     |               |
                its own task, per-file lock     |               |
            (8) metrics: tool_exec + calls[]    |               |
            (9) duplicate reads -> pointers     |               |
             (10) push_tool_result (each after) |               |
                 "✓ <what ran> · 1.2s"          |               |
                   |                            |               |
                   +----------------------------+---------------+
```

## The same ten steps, in words

1. **The turn claims the status line.** `TurnStatus` holds `working` for as long
   as it lives and reports `idle` when it drops, so no exit path can leave the
   mode line busy.
2. **The prompt is written down before anything is asked of a model.** A session
   must never gain a prompt that never ran, and an overflow needs it back.
3. **One request.** The conversation, the system prompt and the tool schemas go
   to the active model through the retry-and-fallback path. A request is timed
   and recorded as an `api_call`, whether it succeeds or not.
4. **The answer is classified before it is stored.** Four outcomes: content
   (kept and printed), reasoning only (kept, not printed), tool calls only
   (*nothing* written — an empty assistant message between a call and its result
   would change the transcript), nothing at all (the fallback sentence).
5. **Tool calls are the only thing that continues a turn.** A response with
   words and no calls ends it.
6. **Calls are written down before they run**, so a crashed process leaves a
   transcript that can be repaired rather than one the provider rejects.
7. **The batch runs concurrently.** Calls that name the same file take turns
   behind a lock; everything else runs together. Each call times itself, and
   that one measurement feeds both the trace line and the ledger.
8. **The batch is recorded** with its span and each call's own.
9. **A re-read of a payload already in the transcript** is answered with a
   pointer to the original instead of a second copy of it.
10. **Each result is written down and named on screen**, then the loop goes back
    to step 3. The turn ends at step 5 when nothing more is asked for.

## What is written where

The loop never writes a file directly; each of these has one owner.

| What | Where | Written by |
|---|---|---|
| the conversation | `.oneloop/sessions/<date>[-<nnn>].jsonl` | `agent/session.rs` |
| cost per request and per batch | `.oneloop/metrics/<same name>` | `agent/metrics.rs` |
| refusals, with the body the server sent | `.oneloop/errors.log` | `providers.rs` |
| the answer, and nothing else | **stdout** | `turn.rs` (step 4) |
| the trace, warnings, errors | **stderr** | `output.rs` |
| busy/idle and what is being worked on | OSC 7501 to the terminal | `status.rs` |

## The one branch worth its own picture

An overflow is the only refusal the loop can cure, so it is the only one that
does not end the turn.

```
  server: "maximum context length is 131072 tokens"
        |
        v
  compact()                         agent/compact.rs
     render   the transcript as text, big tool results elided
                (a failed result keeps both ends: the error is the point)
     chunk    half the declared window, cut between lines
     summarize  one request per part, reduced over at most 3 rounds
     replace  new session file; the old transcript is named, not deleted
        |
        v
  the prompt is pushed again, and the loop retries
        |
        +-- still does not fit?  -> say so, and end the turn
```

Two things follow from this: a turn gets **one** compaction (a summary of a
summary is not smaller), and `/compact` runs the same steps on demand.

## Three rules that make the rest readable

- **stdout is the model's; stderr is OneLoop's.** That is why `ol "…" >
  answer.md` captures an answer and not a trace.
- **The session is append-only.** Nothing is rewritten; the only rotation is
  `/clear` or a compaction, and both leave the old file on disk.
- **One request at a time, one batch at a time.** Within a turn the model is
  never asked two things at once; within a batch the tools are.

## What this picture leaves out

Each of these sits on the path above but has its own section in
`architecture.md`: the retry ladder and the model picker (`models/retry.rs`),
the exact shape of the trace and when a start line is printed (`Output`),
compaction's chunking arithmetic (`Compaction`), the `!command` path and
`/model <alias>; <prompt>` prefixes (`app.rs`), and Ctrl+C, which drops the turn
mid-flight and records a `turn_cancelled` event with whatever it was waiting on.

One path is deliberately outside this picture: `/cc`, `/ca`, `/cn`, `/ag`,
`/aga` and `/agn` ask Claude Code or Antigravity instead of the active model, by
running their CLI. No request goes through the loop, no tool is called, and what
comes back is recorded as reference material for the next turn
([Second opinions](architecture.md#second-opinions)).
