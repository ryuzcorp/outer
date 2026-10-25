---
title: Moderation guide
description: The pre-live gate, what it blocks, and what is still coming.
---

Every deploy passes the pre-live gate before it can serve anything. There
is no after-live crawl and no self-serve appeal yet — both are coming.

## The pre-live gate

The gate reads what visitors can read: JSX text, string literals and
template parts in `.ts/.tsx/.js/.jsx/.mjs` (imports, URLs, CSS classes,
comments and identifiers are skipped), `.html`/`.md`/`.txt` files, JSON
strings under `public/`, SVG text, SQL string literals in migrations and
seeds, the prerendered `dist/` HTML, and the slug plus app name.

Only new text is checked: each string is hashed, and chunks cleared for
the app before are skipped. Caps: 3000 files, 256 KiB each, 8 MiB of
text — over the caps the deploy is blocked as `too_large_to_review`.

New chunks go to Jev (`typesafe/jev-1.14.0` on Workers AI via AI Gateway).
Jev's decision, per chunk:

| Signal | Result |
| --- | --- |
| `profanity ≥ 0.5` | block (`profanity`) |
| `minors > 0.3`, `phishing > 0.9`, or `round(severity) == 3` | block (`harmful`) plus admin queue |
| `hate ≥ 0.5` | recorded for the admin queue, never blocks |

Mild words ("damn", "hell", "crap") pass. Masked spellings, leetspeak
and other languages are read by the classifier itself — there is no
wordlist to evade or allowlist to join.

A blocked deploy keeps the commit on `main` and the previous version
keeps serving. `commit` returns `blocked` with the file, the line, the
score and an excerpt — the excerpt is the chunk's own first 100
characters, unmasked — so fix those lines and commit again. It counts as
a code change, and it is never a rollback target.

When Jev is unreachable, the deploy fails **closed**: nothing goes live
and `commit` returns `failed` with `review_unavailable`. (On localhost
dev without the Cloudflare pair the gate logs a warning and passes —
production always classifies.)

Slugs and app names are checked when claimed, before any deploy runs:
brand and account-page fragments (`paypal`, `apple`, `bank`, `login`, … —
whole or embedded in the slug) are refused outright, and the slug and name
go through the same Jev decision as the table above. When Jev is
unreachable the claim fails closed ("Couldn't check this name right now;
try again in a minute"). On localhost dev without the Cloudflare pair the
check logs a warning and passes, like the gate.

## Coming

- **After-live crawl**: rendered pages are not re-checked yet, so
  visitor-written text that only appears at runtime is not covered.
- **Appeals**: there is no `request_review` tool and no self-serve
  override yet. When a human disagrees with a block, relay the
  `user_message` verbatim instead of inventing a workaround.

## Sources

- Gate decisions and caps: `crates/outer/src/moderation/gate.rs`,
  `crates/outer/src/moderation/extract.rs`
- Jev client and fail-closed behavior:
  `crates/outer/src/moderation/jev.rs`,
  `crates/outer/src/deploy/pipeline.rs` (`review_gate`)
