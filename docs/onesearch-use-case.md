# OneSearch: a worked example

This file records one real repository search, start to finish: the question,
what OneSearch returned, what grep returned on the same question, and why the
two outcomes differ. It exists so the tool's place can be argued from evidence
rather than asserted.

Line numbers are a snapshot. They drift; the file paths and the argument about
vocabulary do not.

## The question

> How does the edit tool run, and how are concurrent edits to the same file
> protected?

This is a normal question to have about a codebase you did not write: you can
describe a behaviour precisely, but you cannot name the identifier that
implements it. That is the state in which most repository search begins.

## What OneSearch returned

```
~/repos/OneSearch/target/debug/onesearch --root . \
  "how does the edit tool run and how are concurrent edits to the same file protected" \
  --filter code --top-k 8 --timing
```

`62.3ms` reported on stderr. The first three results:

| rank | score | chunk |
| --- | --- | --- |
| 1 | 0.0154 | `src/agent.rs:53-64` — the `run_batch` doc comment |
| 2 | 0.0152 | `src/agent.rs:524-537` — `edits_to_one_file_in_a_batch_do_not_lose_each_other` |
| 3 | 0.0149 | `src/tools/edit.rs:20-42` — `impl Tool for EditTool` |

Scores are ranking values, not probabilities. What matters is that all three
are correct and none are noise: the doc comment states the design, the test
demonstrates it, and the tool implementation is where the call lands.

One characteristic worth noting: the top hit is a **comment**, not the code it
describes. `run_batch` is at `src/agent.rs:65`, but the indexed chunk that won
is the prose immediately above it. In a codebase that documents *why*, the
explanation is often higher-signal than the body, and a semantic index finds it.

### The answer, for completeness

- `src/agent.rs:65` `run_batch` — tool calls in one batch run concurrently via
  `tokio::spawn` and `join_all`, gated per file.
- `src/agent.rs:70` — one `HashMap<PathBuf, Arc<Mutex<()>>>` per batch.
- `src/agent.rs:123` `file_key` — the lock key is the call's `path` argument,
  resolved and normalized, for `read`/`write`/`edit` only. `None` for `bash`
  and `elisp`, which can touch anything, so no single path stands for them.
- `src/agent.rs:85-90` — `locks.entry(key).or_default().clone()` hands the same
  `Arc<Mutex<()>>` to every call naming that file; `lock.lock_owned().await`
  holds it for the whole call. Different files still run in parallel.
- `src/agent.rs:135` `normalize` — collapses `.` and `..` lexically without
  touching the filesystem, so `sub/../note.txt` and `note.txt` cannot look like
  two different files to the lock.

The test at `src/agent.rs:524` is the proof. It issues two edits to one file —
one spelled `note.txt`, the other `sub/../note.txt`, both with
`old_text: "foo"` — and asserts **exactly one error**. Had they raced, both
would have read `"foo"`, both would have written, one write would be silently
lost, and the test would see zero errors. The single failure *is* the
serialization evidence: the second edit correctly rejects because the first
already turned `"foo"` into `"bar"`.

## What grep returned

The same question, asked five ways with the obvious vocabulary.

| grep | result |
| --- | --- |
| `grep -rn "concurrent edit" --include=*.rs src` | zero hits |
| `grep -rniE "lost update\|read-modify-write\|race condition\|serializ" --include=*.rs src` | zero real hits — every match is `serde::Deserialize` |
| `grep -rn "take turns" --include=*.rs src` | 2 hits, both correct |
| `grep -rn "edit tool" -il src` | zero files |
| `grep -rn "Mutex" --include=*.rs src` | 14 hits, of which the two that matter are 10th and 11th |

Read that table as a set of guesses, because that is what each line is. Four of
the five failed. The one that succeeded — `"take turns"` — succeeded only
because the answer had already been found by then; nobody starts a search from
"take turns".

And `"Mutex"` is the instructive failure. It does locate the mechanism, but it
returns OAuth token refreshes, the spinner's `AbortHandle` wrapper, and the
status-report atom alongside it. Twelve irrelevant hits to reach two relevant
ones, and the ordering is file order, not relevance — so the answer is in the
middle of the output rather than the top.

It is also incomplete. The line that actually acquires the lock,
`src/agent.rs:89`, reads `Some(lock) => Some(lock.lock_owned().await)` and
therefore contains no `Mutex` at all. Grepping for the type finds the
declaration and never finds the use — which is the general shape of the
problem, not a quirk of this variable name.

### Why grep failed

The codebase does not call this "concurrency protection". It calls it **"Calls
that name the same file take turns."** There is no shared vocabulary between
the question a reader asks and the words the file uses.

That is the general case, not an accident of this question. Grep is a literal
match engine: it can only find strings you already guessed correctly. It fails
in a specific and predictable way — when your mental model of the problem and
the code's own vocabulary do not overlap.

## A second query, with no shared vocabulary

To check that the first result was retrieval rather than luck, the same
question was asked in words that appear nowhere near the code:

```
~/repos/OneSearch/target/debug/onesearch --root . \
  "two things writing the same file at once corrupting each other" \
  --filter code --top-k 3
```

The top two hits were unchanged — `src/agent.rs:524-537` and
`src/agent.rs:53-64`, in that order. Nothing in that sentence is a term of art
in this repository. The retrieval is semantic, not lexical.

## What grep is still better at

Being honest about scope is what keeps the tool credible. Grep wins outright
when the question asks for **completeness rather than relevance**:

- "Is `normalize` called anywhere else?" — needs every match, definitively.
- "Prove this string is unused." — absence must be exhaustive, not top-k.
- "Change every call site." — a ranked list of ten is not a plan.

This document was in fact assembled with both. OneSearch found the place; grep
pinned the exact line numbers and confirmed which symbols were and were not
present. The working rule is: **OneSearch to find the place, grep to sweep it.**

## Two things grep structurally cannot do

Neither was needed above, which is why they are listed separately rather than
folded into the argument.

- `find-related FILE LINE` takes a location you already trust and returns
  semantically similar chunks — every other site that shares a pattern. It is
  the nearest thing to a call graph, but derived from embeddings rather than
  references, so it also surfaces *conceptual* siblings that static analysis
  never links.
- `--filter session` searches prior OneLoop conversation. Bash tools cannot see
  session history at all, so this is a capability with no grep equivalent.

## Conclusion

OneSearch earns its place for one situation, and it is the situation repository
search is usually in: **you can describe the behaviour but not the identifier.**

It is not a replacement for grep and should not be sold as one. It is a
front-end to the same corpus that changes the entry condition — you no longer
have to already know the word the file uses.

The measured claim, in one line: from a precise question to the right
implementation, test, and tool definition in a single query, 62ms, on the first
attempt — where four out of five well-chosen greps returned nothing useful.
