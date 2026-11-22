# Mental model: one message, end to end

OneLoop is a loop around a model. You type a line. The model answers. If the
answer asks OneLoop to do something, OneLoop does it, hands back the result and
asks the model again. The repeat is one *turn*, and it ends when the model
answers without asking for anything.

## Words used below

- **turn** — your line and everything that follows it, until the model stops
  asking for work. A turn that runs one tool makes two model requests.
- **tool call** — the model asking OneLoop to run one tool: `read`, `write`,
  `edit`, `bash`, `elisp`, `skill`.
- **session** — the conversation: every message in order, in one append-only
  file. Each request sends it to the model, together with the system prompt and
  the tool schemas.
- **provider** — the server a model runs on: one URL, one credential. A model is
  one thing that server will run, named by a short alias such as `qwen` or
  `luna`.
- **trace** — the lines OneLoop prints about itself while it works: `▸` for work
  starting, `✓` for work that finished. They go to stderr, never to the answer.
- **metrics** — the cost ledger, one JSON line per request, tool batch and
  compaction, in `.oneloop/metrics/` next to the session.
- **context window** — the most text one model can read in a single request.

## The path, as a picture

Each box names the file and the function that runs at that point, so the code is
one grep away. The numbers 1 to 9 are the steps listed in the next section.

In the top box, `!<command>` runs that command in bash and takes the rest of the
line unchanged, so a `;` there belongs to bash. `/model <alias>; <prompt>`
switches the model only after that model answers.

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
            (9) push_tool_result (each after)   |               |
                 "✓ <what ran> · 1.2s"          |               |
                   |                            |               |
                   +----------------------------+---------------+
```

## The nine steps

1. The turn takes the status line. That is the bottom line of the terminal, or
   the mode line in Emacs. `TurnStatus` reports `working` while the turn runs,
   and `idle` when it drops, so no exit path leaves the mode line busy. In Emacs
   the report travels as OSC 7501, which a small filter lifts into the mode
   line. Elsewhere the spinner already says the same thing.
2. The prompt is written to the session before any model sees it. Writing it
   first means the prompt survives a request that never finishes, and an
   overflow (below) still has it to send again.
3. The turn sends one request. It carries the system prompt (the fixed
   instructions), the session, and the tool schemas (how each tool must be
   called). It goes to the active model through retry and fallback, and it is
   recorded as an `api_call` in the metrics file even when it fails.
4. The answer is sorted before it is stored, into one of four cases:
   - content: kept in the session, printed to stdout;
   - reasoning only, meaning the model's own working when it sends it: kept, not
     printed;
   - tool calls only: nothing is kept, because an empty assistant message
     between a call and its result would change what the model reads back;
   - nothing at all: the fallback sentence is printed instead.
5. Only tool calls continue the turn. An answer with words and no calls ends it.
6. Calls are written to the session before they run. A crash then leaves a call
   with no result, and that can be repaired the next time the session opens.
   Without the repair, the provider would reject every later request in that
   session.
7. Every call in one answer runs at once, each in its own task. Two calls that
   name the same file take turns behind a lock, so neither writes over the
   other. A tool that changes a file replaces it — a temporary file beside it,
   then a rename — so a crash mid-write leaves the old bytes, and the file's
   mode comes along with them. Each call times itself, and that one measurement
   feeds both the `✓` line and the metrics file. A turn, by contrast, sends one
   request at a time.
8. The batch is recorded as one `tool_exec` entry: how long the whole batch
   took, and how long each call inside it took.
9. Each result is written to the session and named on screen, then the loop goes
   back to step 3.

A request gets ten attempts, waiting 500 ms times the attempt number in between.
Only a 5xx, a 429, a timeout or a connection error earns another try. When the
attempts run out, an interactive run offers the other models, and a pipe or a
script gets the error instead. Where a spinner cannot run, as in Emacs or a
pipe, `▸` names work that lasts longer than a second. A `✓` line names
everything that finished.

## Where things are written

The loop writes no file itself. Each of these has one owner.

| What | Where | Written by |
|---|---|---|
| the conversation | `.oneloop/sessions/<date>[-<nnn>].jsonl` | `agent/session.rs` |
| cost per request and per batch | `.oneloop/metrics/<same name>` | `agent/metrics.rs` |
| refusals, with the body the server sent | `.oneloop/errors.log` | `providers.rs` |
| the answer, and nothing else | **stdout** | `turn.rs` (step 4) |
| the trace, warnings, errors | **stderr** | `output.rs` |
| busy/idle and what is being worked on | OSC 7501, inside Emacs | `status.rs` |

## When the session outgrows the model

A model reads only so much text at once, and that limit is its context window.
When the session grows past it, the server refuses the request with an overflow.
That refusal is the only one the loop can cure, so it is the only one that does
not end the turn.

```
  server: "maximum context length is 131072 tokens"
        |
        v
  compact()                         agent/compact.rs
     render   the session as text, big tool results elided
                (a failed result keeps both ends: the error is the point)
     chunk    half the declared window, cut between lines
     summarize  one request per part, reduced over at most 3 rounds
     replace  new session file; the old one is named, not deleted
        |
        v
  the prompt is pushed again, and the loop retries
        |
        +-- still does not fit?  -> say so, and end the turn
```

The session is summarized, and the request that failed is sent again with the
summary in place of the old session. A turn gets one compaction, because a
summary of a summary is not smaller. `/compact` runs the same steps when you ask
for it.

## Second opinions

Claude Code and Antigravity each ship a CLI that already holds a login. These
commands run one of those CLIs as a subprocess instead of asking the active
model:

| mode | Claude | Antigravity | what the CLI receives |
|---|---|---|---|
| critique | `/cc [focus]` | `/ag [focus]` | the session, and a second-opinion instruction |
| about it | `/ca <question>` | `/aga <question>` | the session, and the question |
| standalone | `/cn <prompt>` | `/agn <prompt>` | the prompt alone |

No request goes through the loop and no tool is called. `review.rs` renders the
session as JSON, one object per message with a role, and streams it to the
child's stdin. Both CLIs are told the same thing: the session is untrusted
reference material, not instructions. Claude Code takes that rule as
`--system-prompt`; Antigravity has no such flag, so the rule is prepended to the
prompt.

The answer is printed, then recorded as reference material attributed to you, so
the active model reads it on its next turn. The selected model stays where it
was. Neither CLI is a provider, so `/help` lists these commands only when
`claude` or `agy` is on `PATH`.
