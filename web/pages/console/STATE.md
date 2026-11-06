# Console state machines

Owners and gaps for the implicit machines in `web/pages/console`. One line
each: states, writer, known gap.

## Shell loaders (this change)

- `FetchState` per resource (`idle|loading|ready|stale|failed`): `loadViewData`
  writes via `startFetch`/`settleFetch` in `_islands/state.ts`; quiet failures
  keep the last value as `stale`, bannered above `.main-body`.
- `DataKey` (`slug|view|range|epoch`) gate: every `put` lands only on key
  match; late/cross-view/cross-range responses are dropped.
- Errors poll runs only on `errors`/`overview` (badge); other views skip it.
- `loadMoreActions` page: dropped unless the full key still matches.
- Inline logs refresh: same key check before writing `logs`.
- Gap: account/admin panels still use bare `epoch` checks, not keys.

## Later machines (untouched)

- Actions pagination (`loadingMore|moreError`): `loadMoreActions` appends;
  gap: poll reload resets the list mid-scroll.
- Logs follow (`LogLine[]` + follow flag in `views/logs.tsx`); gap: no key.
- Errors adoption/jump (`pendingJump` in shell, `views/errors.tsx`); gap: jump
  consumed on mount only.
- WebMCP confirm slot (`pendingConfirm` + `createConfirmHandler`); gap: one
  slot, concurrent calls overwrite.
- Deploy ops (expand rows in `ui.expanded`, `onPickSha`, retry); gap: retry
  reuses the shell `reload`.
- D1 grid/editor (`views/d1.tsx` local state); gap: no shell keying.
- URL route + overlays (`route` atom, `mergeRoute`/`readRoute`, drawer,
  palette, create/account modals); gap: overlays live outside the route.
