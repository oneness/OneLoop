# project-map

A deterministic map of a project's modules and their contracts. The driver
(`generate`) owns the graph, the layering and the output format; a language
adapter under `adapters/` owns source parsing. Adding a language means adding
one adapter file — nothing downstream changes.

## Commands

```sh
./project-map/generate                  # write .oneloop/project-map.json
./project-map/generate --stdout         # same bytes, to stdout
./project-map/generate --check          # validate citations in docs/mental-model.md
./project-map/generate --check --doc path/to/doc.md
```

`--check` regenerates the artifact in memory, does not write it, and exits 1
when a citation is invalid. It also prints *uncited mentions* — facts and
modules the prose names without a marker — as informational warnings. Those
warnings do not fail the check.

## Citation markers

Markers are invisible HTML comments. Three kinds:

```html
<!-- module: agent::turn -->                          <!-- an existing module id -->
<!-- edge: app>agent -->                              <!-- an existing dependency edge -->
<!-- fact: config::DEFAULT_MAX_RETRIES -->            <!-- an existing fact id -->
<!-- fact: config::DEFAULT_MAX_RETRIES = 10 -->       <!-- also asserts its value -->
```

A fact citation may omit the value. When present, the checker compares it with
the artifact's `value` as text. Unknown kinds, unknown ids and value mismatches
are errors, reported with the document's line number.

## Artifact

`.oneloop/project-map.json` is a build artifact, gitignored and not committed.
It holds `modules` (id, path, `//!` purpose, public items, exports, `mod`
declarations, resolved imports), `edges` tagged `declares` (module tree),
`uses` (imports) or `path` (fully qualified `crate::…` / `super::…` references
in code, not comments or strings), `layers` computed from `uses` and `path`
edges, `entry_points`, and named `facts` (constants and `ONELOOP_*` variables
with file:line evidence). Identity never depends on line numbers; generated JSON
is sorted and byte-identical across runs.

## Workflow

When code moves a module, a public contract or a dependency direction,
`AGENTS.md` requires: generate the artifact, use the `mental-model` skill to
rewrite the page from it, then run `--check`. The skill lives in
`.oneloop/skills/mental-model/` and is committed; the artifact is not. There
is no CI hook.

## Adapters

`config.json` names the adapter. An adapter module exposes:

```python
def scan(root: Path, config: dict) -> dict:
    return {"modules": [...], "facts": [...]}
```

`root` is the project root; paths in the result are repo-relative. The Rust
adapter reads source text directly — no compiler, no build, no network — and
skips test modules. When it finds cross-module paths it blanks string and
comment contents first, so rustdoc links and literals cannot create false
edges.
