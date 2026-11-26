# OneLoop — mental model

OneLoop is a loop around one model. `app` reads a line, `agent::turn` sends it,
tools run, their results go back, and the loop ends when the model asks for
nothing more. When the server says the context is full, the turn ends without
answering that message: the session rotates at once so the next prompt is
fresh, and the old conversation is summarized off the turn's path into a dated
handoff file. Resend the message that overflowed. If the summarizer's model
keeps failing, the handoff asks the REPL for a choice and the user picks one
at the next prompt boundary.
<!-- module: app --> <!-- module: agent::turn --> <!-- module: agent::compact -->

## The flow

```text
 input
   |
   v
+------------------------------------------------------------------+
| app          read a line: prompt, /command, or ! shell line        |
|              drain handoff notices and model choices each prompt   |
+-------------------------------+----------------------------------+
                                |
                                v
+------------------------------------------------------------------+
| agent        assemble spec, session, metrics, models, prompt      |
+-------------------------------+----------------------------------+
                                |
                                v
+------------------------------------------------------------------+
| turn         one request; keep answer; ask for tools or end       |
+------+-----------------------+-------------------------------+----+
       |                       |                               |
       | context full          v                               v
       v          +------------------------+   +------------------------+
+------+-------+  | batch -> tools         |   | providers -> models    |
| compact      |  | parallel, but one      |   | transport, config,     |
| rotate now,  |  | file at a time         |   | retry                  |
| summarize in |  +-----------+------------+   +-----------+------------+
| background   |              |                            |
+------+-------+   results go back to turn <----------------+
       |
       | writes handoff; notice at next prompt
       v
+------------------------------------------------------------------+
| files        sessions/*.jsonl, metrics ledger, *-handoff.md       |
+------------------------------------------------------------------+
```

Session, metrics, and handoff files live under `.oneloop/`; the answer goes to
stdout, the trace to stderr.
<!-- module: agent --> <!-- module: agent::session --> <!-- module: agent::metrics -->
<!-- module: agent::batch --> <!-- module: tools --> <!-- module: providers -->
<!-- module: models --> <!-- module: output --> <!-- module: status -->
<!-- edge: app>agent --> <!-- edge: agent>agent::session --> <!-- edge: agent>agent::metrics -->
<!-- edge: agent::turn>agent::batch --> <!-- edge: agent::batch>tools --> <!-- edge: agent::turn>providers -->
<!-- edge: agent::turn>agent::compact --> <!-- edge: agent::compact>agent::turn -->
<!-- edge: agent::compact>agent::metrics -->
<!-- edge: models>models::retry -->

## Where to start reading

`src/main.rs` → `app.rs` → `agent.rs` → `agent/turn.rs`, then follow the one
arrow your question is about. A handoff question ends in `agent/compact.rs`.
<!-- module: main --> <!-- module: app --> <!-- module: agent --> <!-- module: agent::turn -->
