# OneLoop — mental model

OneLoop is a loop around one model. `app` reads a line, `agent::turn` sends it,
tools run, their results go back, and the loop ends when the model asks for
nothing more. This is a picture of that loop, not a tour of the code.
<!-- module: app --> <!-- module: agent::turn -->

## The flow

```text
 input
   |
   v
+------------------------------------------------------------------+
| app          read a line: prompt, /command, or ! shell line        |
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
       v                       v                               v
+------+-------+   +------------------------+   +------------------------+
| session      |   | batch -> tools         |   | providers -> models    |
| append-only  |   | all at once, per-file  |   | transport, config,     |
| + metrics    |   | locks                  |   | retry                  |
+------+-------+   +-----------+------------+   +-----------+------------+
       ^                       |                            |
       |   results go back to turn <------------------------+
       |
  compact on overflow, once per turn
```

Session and metrics live under `.oneloop/`; the answer goes to stdout, the
trace to stderr.
<!-- module: agent --> <!-- module: agent::session --> <!-- module: agent::metrics -->
<!-- module: agent::batch --> <!-- module: tools --> <!-- module: providers -->
<!-- module: models --> <!-- module: agent::compact --> <!-- module: output -->
<!-- edge: app>agent --> <!-- edge: agent>agent::session --> <!-- edge: agent>agent::metrics -->
<!-- edge: agent::turn>agent::batch --> <!-- edge: agent::batch>tools --> <!-- edge: agent::turn>providers -->
<!-- edge: agent::compact>agent::turn --> <!-- edge: models>models::retry -->

## Where to start reading

`src/main.rs` → `app.rs` → `agent.rs` → `agent/turn.rs`, then follow the one
arrow your question is about. <!-- module: main --> <!-- module: app --> <!-- module: agent --> <!-- module: agent::turn -->
