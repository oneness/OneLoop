# OneSearch: worked examples

This file records real repository searches, start to finish: the question,
what OneSearch returned, what grep returned on the same question, and why the
two outcomes differ. It exists so the tool's place can be argued from evidence
rather than asserted.

The first case is this repository, in full — a codebase whose vocabulary the
author already carries in their head, which is the friendliest possible case
for grep. The second is a wider sample: twenty-four questions against
`pydantic/pydantic`, a repository nobody here wrote, ten times the chunk count,
with a vendored legacy tree and generated stubs. The first establishes the
mechanism; the second measures how often it holds.

Both were re-run on 2026-10-22 against a release build. A debug build is around
ten times slower on both indexing and query and prints identical output, so
every timing below is from `target/release`; where the cost of loading the
index matters, the one-shot and `batch` figures are given separately.

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
~/repos/OneSearch/target/release/onesearch search \
  "how does the edit tool run and how are concurrent edits to the same file protected" \
  --filter code --top-k 8 --timing
```

`68ms` reported on stderr — and that figure includes loading the index and the
embedding model, because a one-shot invocation has nothing warm to reuse. The
same query inside one `batch` process costs the same ~70 ms for the first
answer and **3–6 ms** for every answer after it. The first three results:

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

### The library and the librarian

Both tools walk the same shelves. They differ in the entry condition.

Grep is a library with nobody at the desk. You have to know the exact title
before you walk in, and the catalogue is unforgiving: if the book is filed as
*Shadows* and you ask for *Duplicates*, you leave with nothing — and with no
information about why, or what to try next.

OneSearch is the librarian. You describe the book — "a field name that collides
with something on a parent class" — and the shelf is found for you. That alone
would make it useful. What makes it durable is that the librarian also tells
you the word the book is filed under, so the next visit takes ten seconds:
`duplicate` still returns nothing, but you are no longer guessing.

The analogy has one honest limit, and it is the one this document keeps
returning to: the librarian points at the shelf and stops. It does not read the
book, connect it to four others, or explain why the author wrote it that way.
You still do the reading — and grep still does the sweeping.

## A second query, with no shared vocabulary

To check that the first result was retrieval rather than luck, the same
question was asked in words that appear nowhere near the code:

```
~/repos/OneSearch/target/release/onesearch search \
  "two things writing the same file at once corrupting each other" \
  --filter code --top-k 3
```

The top two hits were unchanged — `src/agent.rs:524-537` and
`src/agent.rs:53-64`, in that order, on a re-run against the current checkout.
Nothing in that sentence is a term of art in this repository. The retrieval is
semantic, not lexical.

## What grep is still better at

Being honest about scope is what keeps the tool credible. Grep wins outright
when the question asks for **completeness rather than relevance**:

- "Is `normalize` called anywhere else?" — needs every match, definitively.
- "Prove this string is unused." — absence must be exhaustive, not top-k.
- "Change every call site." — a ranked list of ten is not a plan.

This document was in fact assembled with both. OneSearch found the place; grep
pinned the exact line numbers and confirmed which symbols were and were not
present. The working rule is: **OneSearch to find the place, grep to sweep it.**
On unfamiliar code the order is fixed — the finding has to come first, because
the sweep needs a term you do not yet have.

## Two things grep structurally cannot do

Neither was needed above, which is why they are listed separately rather than
folded into the argument.

- `related FILE LINE` takes a location you already trust and returns
  semantically similar chunks — every other site that shares a pattern. It is
  the nearest thing to a call graph, but derived from embeddings rather than
  references, so it also surfaces *conceptual* siblings that static analysis
  never links.
- `--filter session` searches prior OneLoop conversation. Bash tools cannot see
  session history at all, so this is a capability with no grep equivalent.

## A second repository, and what it changes

The example above is close to home — the author knows what this codebase calls
things. `pydantic/pydantic` at `e3b6f55a8` is the honest test: 850 indexed
files, **16,571 chunks** of pydantic v2 Python, `pydantic-core` Rust, `docs/`,
and a vendored `pydantic/v1/` tree that competes with all of it.

Every question was written as a behaviour *before* the answer was known, then
the answer was located in the repository to score the result. The tally below
is therefore a judgement about whether the answering chunk was returned — not a
harness.

### What it cost

One process, many questions:

```
printf '%s\n' "field alias population by_alias by_name lookup" \
  | ~/repos/OneSearch/target/release/onesearch --repo-dir ~/repos/PYDANTIC \
    batch --filter code --top-k 3 --timing
```

| measure | value |
| --- | --- |
| `index` on a warm checkout | **0.05s** — "reused 850 files, rebuilt 0" |
| first query in a process | 69–76 ms (index and model load) |
| every query after that, same process | **4.6–5.4 ms** |
| one-shot CLI invocation | 63–86 ms, essentially all of it the same load |
| cache on disk | 34 MB |

Three of those carry the argument. 0.05 s means the index is not a build step
you schedule. 5 ms means a second attempt is free, so the right response to a
mediocre result is to ask again rather than argue with the ranking. And the
70 ms floor is why the queries belong in one `batch` process: paid once, it is
the difference between a search loop and a search.

### What it got right

| question, first attempt, no identifier named | top chunk |
| --- | --- |
| circular reference between two models fails to resolve | `pydantic/_internal/_generate_schema.py:2930` — the `circular-reference-schema` error |
| field alias population by_alias by_name lookup | `pydantic-core/src/validators/shared/lookup_tree.rs:35` — and 4 of 4 on point |
| how are generic models parameterized into a concrete subclass | `pydantic/_internal/_generics.py:104` |
| custom encoder for datetime when dumping to JSON | `pydantic/_internal/_generate_schema.py:278` — `_add_custom_serialization_from_json_encoders` |
| allow partial JSON validation returning the part that parsed | `pydantic/type_adapter.py:388` — the flag *and* the `experimental.md#partial-validation` anchor |
| duplicate field name in the model class body | miss — see below; rephrased, then `_internal/_fields.py:454` at rank 1 |

The last row is the pattern worth keeping. The query that worked was
**"warning when a field name clashes with an attribute inherited from a base
class"** — a sentence that shares no term with the code, which says `shadows`.

`related` also earned its place here in a way a call graph would not. Seeded
with the one line of the answer that grep had pinned,
`_internal/_generate_schema.py:848`, it returned `_generate_schema.py:1906-1920`
— the dataclass twin of the same trick — plus the `definition-ref` machinery
around it. Seeded with `lookup_key.rs:381`, five of five neighbours were
in-pattern.

### What it missed

- **The mechanism, hidden behind an adjacent docstring.** "Why does a field with
  a default not get validated unless `validate_default` is set" returned
  `resolve_default_value` docstrings; the mechanism is
  `_generate_schema.py:2710`, which grep finds in 39 lines across 6 files.
- **"Where is the core schema cached on the class"** missed twice, with two
  different phrasings. `grep __pydantic_core_schema__` finds the answer in 8
  lines.
- **Duplicated prose in generated stubs.** For the serialization-fallback
  question, three of the top four chunks were the *same paragraph* from
  `pydantic_core/_pydantic_core/__init__.pyi` — two functions, one docstring —
  while `pydantic-core/src/serializers/infer.rs:514-529`, which is what the
  question is about, sat at rank 4. Rephrased to "unable to serialize unknown
  type error", it came back at ranks 1 and 2.
- **The public API outranks the implementation.** "How does a discriminated
  union choose a member" returned three chunks from `pydantic/types.py` — the
  user-facing `Discriminator` and `Tag` — and none from
  `_internal/_discriminated_union.py`. For a user that is the better answer; for
  a maintainer it is the wrong layer.
- **A vendored tree competes.** Four of twenty-four first attempts ranked a
  `pydantic/v1/` file in the top three, one of them at rank 1 (`v1/schema.py`
  above `validators/string.rs` for a question about `max_length`). v1 is
  indexed because it is in the repository; it is never the answer for a v2
  question.
- **Language separation.** "Populate a model by field name instead of by alias"
  returned only Python. The Rust half needed the question re-asked in Rust's
  vocabulary. Cross-language retrieval is two queries plus `related`, not one.

### The tally, and what grep won

Ten of twenty-four first attempts put the answering chunk in the top three;
six more landed adjacent — right file or right concept, wrong site; six missed.
Three were rephrased, and two of those recovered cleanly.

Grep won three questions outright — and each of the three turned on an
identifier that only someone who already knew the codebase would have typed:

| grep | what it returned |
| --- | --- |
| `validate_default` | 39 lines / 6 files; mechanism at `_internal/_generate_schema.py:2710` |
| `shadow` | 5 lines; `_internal/_fields.py:459` |
| `__pydantic_core_schema__` | 8 lines; `_internal/_generate_schema.py:848` |

Note which word was needed. The question was "what happens when a model
declares the same field name twice"; `grep "duplicate field"` returns **zero**.
The code says `shadows an attribute`. That is the whole asymmetry in one line —
the information was one vocabulary change away, and the only thing that supplied
the change was the near-miss result of a query that had already failed.

### What the misses suggest

Tuning leads, not verdicts.

- **Collapse near-identical chunks before fusion.** The stub-duplication case
  would have promoted the real implementation to rank 1 with no other change.
- **Down-weight generated and vendored paths** — `.pyi`, `_pb2.py`, a `v1/`
  tree. Cheapest precision win available on repositories like this one.
- **Raise `-k` on docstring-dense corpora.** More than one correct chunk was
  present at rank 4 and clipped by the window, not outranked by it.
- **Score shape is not a confidence signal.** The failing "duplicate field"
  query was tightly clustered (0.0098/0.0097/0.0095) and the winning alias query
  was equally flat (0.0161/0.0160/0.0145). A tight cluster means *a topic*, not
  *the answer*, so a warning built on score spread would fire on the good
  results too. The reliable test is a `related` hop, or another query.

## Why not just point a coding agent at the repository?

Because that is the same procedure with a model attached. A repository of this
size is **11.6 MB**, roughly three million tokens: an agent cannot read it, so
"point an agent at it" resolves to *guess a word, grep, read the files the hits
live in, reason*. The choice is not between a tool and an agent. It is whether
the agent's first move is a guess or a lookup — an agent with grep alone is a
very good reader in a library it must know the titles of; it is not the
librarian.

### The same question, two ways

On the alias question, measured:

| | OneSearch | grep + read |
| --- | --- | --- |
| what arrives | 78 lines, **3.1 KB**, top four all on point | 201 hits in 16 files; the six files you would open are 6,658 lines, **294 KB** |
| tokens | ~800 | ~75,000 |
| cost of the first attempt | 5 ms | one model turn |
| cost of a wrong word | 5 ms | another turn, plus the reads it caused |
| signal on failure | a near-miss file that names the right word | "0 results", and nothing else |

One in seventy of the context, for the same answer.

### Four things it gives an agent that grep cannot

1. **Context is the agent's scarce resource, not time.** Seventy-five thousand
   tokens of reading to answer one question is most of a working window spent
   on something 800 tokens answered — and the window is what it needed to do
   the actual task. This is the largest effect and the one a latency benchmark
   never shows.
2. **A wrong answer still carries information.** An agent grepping can only
   search for words it already thought of; when `duplicate` returns zero it has
   nothing to reason from and must invent the next word. A ranked near-miss
   hands back the file, and the file's vocabulary is what makes `shadow`
   findable. The agent is not bad at this — it is paying a full round trip per
   guess to learn what a 5 ms query already knows.
3. **5 ms makes thoroughness rational.** An agent that can afford eight
   reformulations and a `related` hop in under a tenth of a second will do it.
   An agent paying a turn per probe settles for the first plausible hit, which
   is exactly how confident answers about non-existent APIs get written. Cheap
   search makes care the cheaper option.
4. **Determinism, and a corpus grep cannot reach.** Same query, same
   `path:line`, citable and re-checkable with grep; cached and repeatable at no
   API cost. And `--filter session` searches prior conversation, which is not
   in the filesystem at all — no agent, however capable, can grep its way to it.

### Where the agent still wins

The agent does something no retrieval tool does: it reads five chunks, connects
them, runs the code, and answers *why*. OneSearch returns a neighbourhood and
stops. Inside an agent loop the saving is therefore mostly **reads** — large —
rather than **turns** — moderate, since each of the six misses above still costs
a round trip. On a repository of a few dozen files, grep and read win outright
and no index is worth 34 MB. With an LSP or a good hybrid IDE search already in
the toolset, expect the gap to narrow.

## Conclusion

OneSearch earns its place for one situation, and it is the situation repository
search is usually in: **you can describe the behaviour but not the identifier.**

It is not a replacement for grep and should not be sold as one. It is a
front-end to the same corpus that changes the entry condition — you no longer
have to already know the word the file uses.

Two corpora, one conclusion. On the repository this document was assembled in,
the first query was also the last: three correct chunks — a comment stating the
design, a test proving it, and the tool implementation. On pydantic, at ten
times the chunks, with a vendored legacy tree, generated stubs, and a
vocabulary the author did not carry, ten of twenty-four first attempts were
that clean, six were adjacent, and two of the six misses recovered for the
price of a rephrase. The three questions grep won all turned on an identifier
that only someone who already knew the codebase would have typed: the trade, in
one line, is that **grep answers the questions you can already phrase in the
repository's words** — and semantic ranking is what supplies the words.

The measured claim, in one line: on this repository, a precise question about
behaviour reaches the right implementation, the test that proves it, and the
tool definition in a single query, in **3–6 ms** once the index is loaded —
where four out of five well-chosen greps returned nothing useful. On an
unfamiliar repository the same move is worth its place for a different reason:
it is how you earn the words that make the greps well-chosen.
