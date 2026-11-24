# project-map

A deterministic map of OneLoop's modules and contracts, and the checker behind
[`docs/mental-model.md`](../docs/mental-model.md). The driver is Rust
(`main.rs`); `generate` is only a wrapper that finds the checkout and runs the
binary. It reads source text directly — no compiler, no build, no network — and
the page itself is authored: this tool never invents a sentence.

## Commands

```sh
./project-map/generate                  # write .oneloop/project-map.json
./project-map/generate --stdout         # the same bytes, to stdout
./project-map/generate --check          # validate citations in docs/mental-model.md
./project-map/generate --check --doc path/to/doc.md
./project-map/generate --out path.json  # write the artifact elsewhere
```

`--check` rebuilds the map in memory, writes nothing, and exits 1 when a
citation does not resolve. It also prints *uncited mentions* — modules and
facts the prose names without a marker — as information; those do not fail the
check.

## Citation markers

Markers are invisible HTML comments. Three kinds resolve against the map:

```html
<!-- module: agent::turn -->                     <!-- an existing module id -->
<!-- edge: app>agent -->                         <!-- an existing dependency edge -->
<!-- fact: config::DEFAULT_MAX_RETRIES -->       <!-- an existing fact id -->
<!-- fact: config::DEFAULT_MAX_RETRIES = 10 -->  <!-- also asserts its value -->
```

A fact citation may omit the value. When present, the checker compares it with
the map's `value` as text. An unknown kind, an unknown id or a value mismatch is
an error, reported with the document's line number.

## The artifact

`.oneloop/project-map.json` is a build artifact: gitignored, not committed, and
byte-identical across runs. It holds:

- `modules` — id, repo-relative path, the `//!` purpose, public items with
  signatures and `file:line`, exports, `mod` declarations, resolved imports,
  and line count.
- `edges` — `declares` (module tree), `uses` (imports) and `path` (fully
  qualified `crate::…` / `super::…` references in code, not comments or
  strings), each with the statements that produced it.
- `layers` — topological layers from `uses` and `path` edges; `unresolved`
  lists cycles. Containment is not a dependency direction.
- `facts` — named constants and `ONELOOP_*` variables, with `file:line`.

Identity never depends on a line number, and every list is sorted.

## Configuration

`config.json` names what to scan and where the result goes:

```json
{
  "adapter": "rust",
  "roots": ["src"],
  "entry_points": ["src/main.rs"],
  "artifact": ".oneloop/project-map.json",
  "doc": "docs/mental-model.md"
}
```

The adapter field is kept for format compatibility; the Rust parser is now
compiled into the binary. A second language means another parser in `main.rs`,
not another runtime.

## Workflow

When code moves a module, a public contract or a dependency direction,
`AGENTS.md` requires: generate the artifact, use the `mental-model` skill to
rewrite the page from it, then run `--check`. The skill lives in
`.oneloop/skills/mental-model/` and is committed; the artifact is not. There
is no CI hook.
