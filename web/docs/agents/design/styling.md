---
title: Style with daisyUI
description: Use daisyUI components, layout-only Tailwind, and 360px-first responsive rules.
---

## Use daisyUI components first

Reach for daisyUI component classes before utilities:

| Need | Classes |
| --- | --- |
| Bar, card | `navbar`, `card card-body card-title` |
| Actions, inputs | `btn btn-primary btn-ghost btn-sm`, `input` |
| Groups, lists | `join join-item`, `list list-row list-col-grow` |
| Feedback | `badge`, `alert alert-soft`, `text-error` |
| Structure | `fieldset`, `footer`, `link` |

1. Use Tailwind utilities only for layout (`flex`, `gap-*`, `max-w-*`).
2. Mark faint text with `opacity-70`, never gray color utilities.
3. Ship `light` and `dark` themes only; switching needs no code.

## Lay out phones first

1. Keep `charset` and `viewport` meta tags first in `head`.
2. Verify every page at 360px wide before desktop widths.
3. Stack by default; spread out from `sm:` up (`grid grid-cols-1 sm:grid-cols-3`, `flex flex-col sm:flex-row`).
4. Use widths that fit the screen; never set fixed widths wider than 360px.
5. Shorten long text with `truncate` or `break-words`.
6. Wrap wide tables in `overflow-x-auto`.
7. Use `btn` tap targets for primary actions and `w-full` inputs.
8. Prefer `min-h-dvh` over `min-h-screen` so mobile browser bars never cut content.

For the person-side walkthrough, see [Start from phones](/design/phones-first).
