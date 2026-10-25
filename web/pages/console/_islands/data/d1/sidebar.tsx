// The D1 editor's left rail: database identity, a table filter and the
// table list with row counts. Collapses to a `<select>` on small screens.
// Ported from Noite's `lib/storage/d1/sidebar.tsx` (noite@58a977a); the
// read-only lock and the app back-link are gone (owner-only console; the
// console sidebar already names the app).

import { atom } from "ilha";

import type { D1TableInfo } from "./types.ts";


export const TableSidebar = ({
  binding,
  current,
  onSelect,
  tables,
}: {
  binding: string;
  current: string;
  onSelect: (table: string) => void;
  tables: D1TableInfo[];
}) => {
  const query = atom("");

  const needle = query().trim().toLowerCase();
  const visible =
    needle === ""
      ? tables
      : tables.filter((table) => table.name.toLowerCase().includes(needle));

  return (
    <aside class="d1-rail bg-base-100 border-base-300 hidden w-64 shrink-0 flex-col border-r md:flex">
      <div class="border-base-300 flex flex-col gap-1 border-b p-3">
        <div class="flex items-center gap-2">
          <span class="truncate font-semibold" title={binding}>
            {binding}
          </span>
          <span class="badge badge-sm">D1</span>
        </div>
      </div>
      <div class="px-3 pt-3 pb-1">
        <label class="input input-sm w-full">
          <input
            type="search"
            aria-label="Search tables"
            placeholder="Search tables"
            value={query()}
            oninput={(event) => {
              query.set(event.currentTarget.value);
            }}
          />
        </label>
      </div>
      <nav class="flex-1 overflow-y-auto p-2" aria-label="Tables">
        <ul class="menu w-full gap-0.5 p-0">
          {visible.map((table) => (
            <li key={table.name}>
              <button
                type="button"
                class={table.name === current ? "menu-active gap-2" : "gap-2"}
                aria-current={table.name === current ? "true" : undefined}
                onclick={() => {
                  onSelect(table.name);
                }}
              >
                <span class="min-w-0 flex-1 truncate text-left">
                  {table.name}
                </span>
                <span class="shrink-0 text-xs tabular-nums opacity-50">
                  {table.row_count}
                </span>
              </button>
            </li>
          ))}
        </ul>
        {visible.length === 0 ? (
          <p class="px-2 text-sm opacity-60">No tables match.</p>
        ) : null}
      </nav>
      <div class="border-base-300 border-t px-3 py-2 text-xs opacity-50">
        {tables.length} table{tables.length === 1 ? "" : "s"}
      </div>
    </aside>
  );
};

/** The sidebar's mobile stand-in: one select above the grid. */
export const TableSelect = ({
  binding,
  current,
  onSelect,
  tables,
}: {
  binding: string;
  current: string;
  onSelect: (table: string) => void;
  tables: D1TableInfo[];
}) => (
  <div class="border-base-300 flex flex-wrap items-center gap-2 border-b px-3 py-2 md:hidden">
    <span class="truncate text-sm font-semibold">{binding}</span>
    <span class="badge badge-sm">D1</span>
    <label class="ml-auto flex items-center gap-2 text-sm">
      <span class="sr-only">Table</span>
      <select
        class="select select-sm"
        aria-label="Table"
        onchange={(event) => {
          onSelect(event.currentTarget.value);
        }}
      >
        {tables.map((table) => (
          <option
            key={table.name}
            value={table.name}
            selected={table.name === current}
          >
            {table.name} ({table.row_count === 1 ? "1 row" : `${table.row_count} rows`})
          </option>
        ))}
      </select>
    </label>
  </div>
);
