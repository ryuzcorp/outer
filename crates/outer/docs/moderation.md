# Moderation guide

Every deploy passes the pre-live gate before it can serve anything; live
apps are also crawled, and abuse reports are reviewed.

## The pre-live gate

The gate reads what visitors can read: JSX text, string literals and
template parts in `.ts/.tsx/.js/.jsx/.mjs` (imports, URLs, CSS classes,
comments and identifiers skipped), `.html/.md/.txt` files, JSON strings
under `public/`, SVG text, SQL string literals in migrations and seeds, the
prerendered `dist/` HTML, and the slug plus app name.

- Strong profanity, slurs, masked spellings (`f.u.c.k`, leetspeak) and
  severe content **block** the deploy. The commit stays on `main`, the
  previous version keeps serving, and the tool returns `blocked` with the
  file, the line and an excerpt — fix those lines and commit again.
- Mild words ("damn", "hell", "crap") pass.
- Only new text is checked; text cleared before is skipped.
- Over the caps (3000 files, 256 KiB each, 8 MiB total) the deploy is
  blocked as `too_large_to_review`.

## After going live

Rendered pages (including D1 rows and fetched text) are crawled after every
deploy. Strong profanity on a rendered page **holds** the app (503 until a
clean deploy or a review); phishing and severe content **suspend** it. Run
all visitor text through the template's `moderate()` helper before storing
it. Uploaded files are not classified — that is why uploads are Pro-only.

## Appeals

There is no self-serve override. When a human disagrees with a block, tell
them the block stands until the platform owner reviews it, and relay any
`user_message` verbatim instead of inventing a workaround.
