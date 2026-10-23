# Moderation guide

Every deploy passes the pre-live gate before it can serve anything. The
gate asks TypeSafe Jev (on Cloudflare) about each new piece of text.

## The pre-live gate

The gate reads what visitors can read: JSX text, string literals and
template parts in `.ts/.tsx/.js/.jsx/.mjs` (imports, URLs, CSS classes,
comments and identifiers skipped), `.html/.md/.txt` files, JSON strings
under `public/`, SVG text, SQL string literals in migrations and seeds, the
prerendered `dist/` HTML, and the slug plus app name.

- Strong profanity, slurs and masked spellings (`f.u.c.k`, leetspeak, other
  languages), sexual content involving minors, phishing and severely harmful
  content **block** the deploy. The commit stays on `main`, the
  previous version keeps serving, and the tool returns `blocked` with the
  file, the line and an excerpt — fix those lines and commit again.
- Mild words ("damn", "hell", "crap") pass.
- Only new text is checked; text cleared before is skipped.
- Over the caps (3000 files, 256 KiB each, 8 MiB total) the deploy is
  blocked as `too_large_to_review`.

- When the content check is briefly unavailable the deploy ends `failed`
  with `review_unavailable`: nothing unchecked goes live; commit again in a
  minute.

## After going live

Checks of the rendered pages (including stored visitor text) and abuse
reports are coming in a later release. Uploaded files are not classified.

## Appeals

There is no self-serve override. When a human disagrees with a block, tell
them the block stands until the platform owner reviews it, and relay any
`user_message` verbatim instead of inventing a workaround.
