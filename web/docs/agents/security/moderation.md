---
title: Pass the moderation gate
description: Know what the pre-live gate extracts, how Jev decides, and what blocks do.
---

Every deploy passes the pre-live gate before it serves anything. A blocked deploy keeps its commit and keeps serving the previous version.

## Expect extraction from these sources

1. Expect `oxc` string extraction: strings of 2+ copy-like letters; imports, URLs, CSS, comments, and identifiers stay skipped.
2. Cover `.html`, `.md`, and `.txt` files.
3. Cover JSON strings under `public/` plus `manifest.webmanifest`.
4. Cover SVG text nodes.
5. Cover SQL string literals in migrations and seeds.
6. Cover prerendered `dist/` HTML.
7. Cover the slug plus app name.

## Respect extraction caps and dedup

1. Stay under 3000 files, 256 KiB per file, and 8 MiB of text.
2. Expect larger inputs to block as `too_large_to_review`.
3. Hash each chunk with sha256 and skip chunks already cleared for the app.
4. Treat only new text as reviewable.

## Predict the Jev decision

New chunks go to Jev (`typesafe/jev-1.14.0`):

| Signal | Result |
| --- | --- |
| `profanity ≥ 0.5` | Block as `profanity`. |
| `minors > 0.3`, `phishing > 0.9`, or `round(severity) == 3` | Block as `harmful`, plus admin queue. |
| `hate ≥ 0.5` | Record for the admin queue; never blocks. |

Mild words pass. The classifier reads masked spellings, leetspeak, and other languages itself.

## Handle blocks and failures

1. Read each finding as `{path, line, excerpt, category, signal, score}` with a `user_message`.
2. Quote the excerpt verbatim (the chunk's first 100 characters, unmasked).
3. Relay `user_message` verbatim to the person.
4. Fix flagged lines and commit again; a block counts as a code change and never becomes a rollback target.
5. Expect brand fragments (`paypal`, `apple`, `bank`, `login`, whole or embedded) refused at claim time.
6. Expect fail-closed `review_unavailable` when Jev is down; localhost dev without the Cloudflare pair passes with a warning.
7. Offer no self-serve override; there is no appeal tool. Relay `user_message` verbatim instead of inventing a workaround.

## Respect after-live enforcement

1. Expect an after-live crawl of rendered pages after each deploy; new chunks re-enter the gate.
2. Expect flagged apps to land in the admin moderation queue; an admin can hold or suspend the app, and suspension stops its fleet.
3. Relay hold and suspend messages verbatim.

For the person-side walkthrough, see [Follow the content rules](/security/moderation).
