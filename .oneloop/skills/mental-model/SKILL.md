---
name: mental-model
description: Regenerate docs/mental-model.md from the project map after a module, contract or dependency change; keep the one-page contract map and verify every citation.
---

# Mental model skill

Use this when a change moves a module boundary, a public contract or a
dependency direction, or when `docs/mental-model.md` no longer matches the
code. The artifact is the source of truth for names and values; the model owns
the prose.

## Steps

1. Run `./project-map/generate`. It writes `.oneloop/project-map.json`, a
   gitignored build artifact. Read it — do not hand-edit it.
2. Read the current `docs/mental-model.md`.
3. Rewrite **only** that file. Keep the three-part shape: the loop paragraph,
   one ASCII picture of the flow, and `Where to start reading`. Each box in
   the picture names a contract; arrows show the direction. No tables, no
   layer headings, no separate boundaries section — at most one caption line
   for files and streams. Stay within 100 lines.
4. Cite the boxes and arrows using ids from the artifact:
   `<!-- module: agent::turn -->`, `<!-- edge: app>agent -->`,
   `<!-- fact: config::DEFAULT_MAX_RETRIES = 10 -->`. An asserted fact value is
   checked as text; omit `= value` when only the id matters. Every box and
   arrow must be cited, and no named fact or module may be left uncited.
5. Run `./project-map/generate --check`. Fix every error it reports, then show
   the diff. Touch no other document.

A citation proves the id exists in the artifact, not that the sentence beside
it is true. If a claim has no artifact backing, drop it rather than inventing
an id. `docs/index.html` and `docs/style-guide.md` are handwritten and out of
scope.
