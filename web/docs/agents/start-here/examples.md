---
title: Examples
description: Copy proven patterns onto the empty shell with the examples tool.
---

New apps are empty branded shells: layout, brand, settings, and nothing to
delete. Real features live in the examples, served over MCP from the pinned
templates repo.

## List them

Call `examples` with no arguments. Filter with `template: "app"` or
`template: "website"`. Each row names the pattern, its template, its
dependencies, and its files with line counts. The result ends with its own
`next` step.

## Copy one

Call `examples {name}`. The result is the pattern's README first, then each
file as a fenced block headed by its path. The README is written for you:
what the pattern shows, then numbered steps. Follow them in order:

1. Copy each file to the same path in the app repo.
2. Add the page to `pages/_nav.ts`.
3. Add any dependency to `package.json`, or any binding to `void.config.ts`.
4. Rename the migration when the app already has migrations.
5. Finish with the README's gotchas, then verify with `look_at_app`.

Files are complete and working when copied onto a fresh copy of their
template, and assume no other example is present. Results past the 200 KB
output budget arrive truncated: read the rest file by file, or copy what
arrived and fetch the remainder.

## Unknown names

An unknown name refuses with `invalid_argument` and lists the valid names.
A misspelled template (`"Website"`, `"static"`) refuses the same way: only
`"app"` and `"website"` exist.
