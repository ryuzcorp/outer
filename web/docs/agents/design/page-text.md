---
title: Keep page text shippable
description: Avoid strong profanity in rendered text, seeds, and SQL literals.
---

## Write clean page text

1. Keep strong profanity out of page text, seed data, and SQL string literals.
2. Remember the pre-live gate reads rendered text plus string literals.
3. Fix flagged lines and commit again after a block.
4. Expect a blocked deploy to keep serving the previous version.

For the person-side walkthrough, see [Keep page text shippable](/design/page-text).
