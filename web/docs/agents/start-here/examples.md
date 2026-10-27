---
title: Examples
description: Apply proven features with add_example, then customise.
---

New apps are empty branded shells: layout, brand, settings, and nothing to
delete. Real features live in the examples, served over MCP from the pinned
templates repo. One tool call applies one into your app as a committed,
deployed change — you only customise copy and design afterwards.

## Apply one

Call `add_example {app, name}`. It resolves the example's `requires` first,
copies every file to the same path, adds nav entries to `pages/_nav.ts`,
merges `dependencies` into `package.json`, inserts `d1`/`r2` bindings into
`void.config.ts`, renumbers migrations past the app's highest, and commits
all of it in one change through the normal commit + gate + deploy path.
The result names the added examples, the written files, the live hrefs, and
the deploy.

Or start with features: `create_app {slug, examples: ["passkeys", "photos"]}`
seeds the template and applies the examples before the first commit, so the
app is born with working pages and one deploy.

After applying, customise copy and design with `commit`, and verify with
`look_at_app`.

## List them

Call `examples` with no arguments. Filter with `template: "app"` or
`template: "website"`. Each row names the pattern, its template, its
dependencies, and its files with line counts. The result ends with its own
`next` step.

## Inspect one

Call `examples {name}`. The result is the pattern's README first, then its
file list with line counts — small enough to stay under 6 KB. Pass
`files: true` for the full contents in the text block (cut at the 200 KB
output budget exactly like `read_files`; `structuredContent` carries
metadata plus the file list, never the contents twice).

An unknown name refuses with `invalid_argument` and lists the valid names.
A misspelled template (`"Website"`, `"static"`) refuses the same way: only
`"app"` and `"website"` exist.

## Without add_example

Prefer `add_example`. If you must hand-copy (the app's files diverged and
the engine refused), the README's "What add_example does" section is the
checklist: copy each file to the same path, add the page to `pages/_nav.ts`
at the `// outer:nav-items` marker (icons into `// outer:nav-icons`), add
any dependency to `package.json` or binding to `void.config.ts` at the
`// outer:bindings` marker, and renumber migrations past the app's highest.
Finish with the README's gotchas, then verify with `look_at_app`.
