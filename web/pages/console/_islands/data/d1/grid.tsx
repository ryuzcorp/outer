// The D1 data grid: a sticky header with sortable columns, a selection
// column with a bulk-delete bar, truncated cells (see ./cells) and the
// pager footer. Rows come from the server for exactly the URL query handed
// in, so this component is mounted per query (`key` in ./panel). Ported
// from Noite's `lib/storage/d1/grid.tsx` (noite@58a977a); reads and the
// bulk delete run through the registered `d1_rows` / `d1_delete_rows`
// actions, and the Noite-only row actions and capability gates are gone.

import { atom } from "ilha";
import type { View } from "ilha";

import { d1run } from "../../actions/data-d1.ts";
import type {
  D1DeleteRowsInput,
  D1RowsInput,
} from "../../actions/data-d1.ts";
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  KeyIcon,
} from "../../icons.tsx";
import { isTimestampColumn, keyFor } from "./d1-values.ts";
import type {
  D1Cell,
  D1Rows,
  D1RowsQuery,
  D1TableSchema,
} from "./types.ts";
import { CellValue } from "./cells.tsx";
import { DEFAULT_PAGE_SIZE, PAGE_SIZES, pageCount } from "./state.ts";

/** Loading state for one mounted query. The panel remounts this component
 * per query key, so the fetch fires once per mount. */
type RowsState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready"; value: D1Rows };

const Pager = ({
  onPage,
  onSize,
  page,
  pageSize,
  total,
}: {
  onPage: (page: number) => void;
  onSize: (size: number) => void;
  page: number;
  pageSize: number;
  total: number;
}) => {
  const pages = pageCount(total, pageSize);

  return (
    <div class="flex flex-wrap items-center gap-2 text-sm">
      <label class="flex items-center gap-1">
        <span class="opacity-60">Page</span>
        <input
          class="input input-sm w-16 text-center"
          type="number"
          min="1"
          max={pages}
          aria-label="Page"
          value={String(page + 1)}
          onchange={(event) => {
            const next = Math.trunc(Number(event.currentTarget.value)) || 1;
            onPage(Math.min(Math.max(next - 1, 0), pages - 1));
          }}
        />
        <span class="opacity-60">of {pages}</span>
      </label>
      <button
        type="button"
        class="btn btn-square btn-ghost btn-sm"
        aria-label="Previous page"
        disabled={page === 0}
        onclick={() => {
          onPage(Math.max(page - 1, 0));
        }}
      >
        <ChevronLeftIcon />
      </button>
      <button
        type="button"
        class="btn btn-square btn-ghost btn-sm"
        aria-label="Next page"
        disabled={page + 1 >= pages}
        onclick={() => {
          onPage(Math.min(page + 1, pages - 1));
        }}
      >
        <ChevronRightIcon />
      </button>
      <label class="flex items-center gap-1">
        <span class="sr-only">Rows per page</span>
        <select
          class="select select-sm"
          aria-label="Rows per page"
          onchange={(event) => {
            onSize(Number(event.currentTarget.value) || DEFAULT_PAGE_SIZE);
          }}
        >
          {PAGE_SIZES.map((size) => (
            <option key={size} value={size} selected={size === pageSize}>
              {size} / page
            </option>
          ))}
        </select>
      </label>
      <span class="opacity-60">
        {total} record{total === 1 ? "" : "s"}
      </span>
    </div>
  );
};

export const RowsGrid = ({
  binding,
  canWrite,
  onChanged,
  onOpenRow,
  onPage,
  onSize,
  onSort,
  onToast,
  query,
  schema,
  slug,
  viewSlot,
}: {
  binding: string;
  /** False for collaborators: rows render read-only, no selection. */
  canWrite: boolean;
  onChanged: () => void;
  onOpenRow: (row: Record<string, D1Cell>) => void;
  onPage: (page: number) => void;
  onSize: (size: number) => void;
  onSort: (column: string) => void;
  onToast: (text: string) => void;
  query: D1RowsQuery;
  schema: D1TableSchema;
  slug: string;
  viewSlot: View;
}) => {
  const state = atom<RowsState>({ kind: "loading" });
  const selected = atom<Record<string, true>>({});
  const confirming = atom(false);
  const busy = atom(false);
  const failure = atom("");
  const started = atom(false);

  if (!started()) {
    started.set(true);
    const input: D1RowsInput = {
      slug,
      binding,
      table: query.table,
      filters: query.filters,
      page: query.page,
      page_size: query.page_size,
      search: query.search,
      sort: query.sort,
    };
    d1run<D1RowsInput, D1Rows>("d1_rows", input).then(
      (value) => {
        state.set({ kind: "ready", value });
      },
      (error: unknown) => {
        state.set({
          kind: "error",
          message: error instanceof Error ? error.message : String(error),
        });
      },
    );
  }

  const current = state();

  if (current.kind === "error") {
    return (
      <div class="flex flex-col items-start gap-2 p-4">
        <p class="text-error m-0 text-sm">{current.message}</p>
        <button
          type="button"
          class="btn btn-sm"
          onclick={() => {
            state.set({ kind: "loading" });
            started.set(false);
          }}
        >
          Retry
        </button>
      </div>
    );
  }

  if (current.kind === "loading") {
    return (
      <div class="flex flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  const data = current.value;

  const schemaColumn = (name: string) =>
    schema.columns.find((column) => column.name === name);

  const selectedIndexes = Object.keys(selected());
  const allSelected =
    data.rows.length > 0 && selectedIndexes.length === data.rows.length;

  const bulkDelete = async () => {
    if (busy()) {
      return;
    }

    busy.set(true);

    try {
      const keys = selectedIndexes.map((index) => {
        const record: Record<string, D1Cell> = {};
        for (const [position, name] of data.columns.entries()) {
          record[name] = data.rows[Number(index)]?.[position] ?? null;
        }
        return keyFor(schema.columns, record);
      });

      const input: D1DeleteRowsInput = { slug, binding, table: data.table, keys };
      const result = await d1run<D1DeleteRowsInput, { deleted: number }>(
        "d1_delete_rows",
        input,
      );

      failure.set("");
      selected.set({});
      confirming.set(false);
      onToast(
        `${result.deleted} row${result.deleted === 1 ? "" : "s"} deleted`,
      );
      onChanged();
    } catch (error) {
      failure.set(error instanceof Error ? error.message : String(error));
    } finally {
      busy.set(false);
    }
  };

  return (
    <div class="flex min-h-0 flex-1 flex-col">
      {selectedIndexes.length > 0 ? (
        <div class="bg-base-200 flex flex-wrap items-center gap-2 px-3 py-1.5 text-sm">
          <span class="font-medium">{selectedIndexes.length} selected</span>
          {confirming() ? (
            <>
              <span class="text-error">
                Delete {selectedIndexes.length} row(s)? This cannot be undone.
              </span>
              <button
                type="button"
                class="btn btn-sm btn-error"
                disabled={busy()}
                onclick={() => {
                  void bulkDelete();
                }}
              >
                {busy() ? "Deleting…" : "Delete"}
              </button>
              <button
                type="button"
                class="btn btn-sm btn-ghost"
                disabled={busy()}
                onclick={() => {
                  confirming.set(false);
                }}
              >
                Cancel
              </button>
            </>
          ) : (
            <button
              type="button"
              class="btn btn-sm btn-ghost"
              onclick={() => {
                confirming.set(true);
              }}
            >
              Delete
            </button>
          )}
          <button
            type="button"
            class="btn btn-sm btn-ghost ml-auto"
            onclick={() => {
              selected.set({});
            }}
          >
            Clear
          </button>
        </div>
      ) : null}
      {failure() ? (
        <p class="text-error m-0 px-3 py-1 text-sm">{failure()}</p>
      ) : null}
      <div class="min-h-0 flex-1 overflow-auto">
        {data.rows.length === 0 ? (
          <p class="m-0 p-4 text-sm opacity-60">
            {data.total === 0
              ? "This table has no rows."
              : "No rows match the current filters."}
          </p>
        ) : (
          <table class="table-sm table-pin-rows table w-full">
            <thead>
              <tr>
                {canWrite ? (
                  <th class="w-10">
                    <input
                      class="checkbox checkbox-sm"
                      type="checkbox"
                      aria-label="Select all rows on this page"
                      checked={allSelected}
                      onchange={() => {
                        if (allSelected) {
                          selected.set({});
                          return;
                        }

                        const next: Record<string, true> = {};
                        for (const [index] of data.rows.entries()) {
                          next[String(index)] = true;
                        }
                        selected.set(next);
                      }}
                    />
                  </th>
                ) : null}
                {data.columns.map((name) => {
                  const column = schemaColumn(name);
                  const sort = query.sort;
                  const mark =
                    sort === null || sort.column !== name
                      ? null
                      : sort.desc
                        ? "▼"
                        : "▲";

                  return (
                    <th key={name} class="whitespace-nowrap">
                      <button
                        type="button"
                        class="inline-flex items-center gap-1 font-semibold"
                        title={`Sort by ${name}`}
                        onclick={() => {
                          onSort(name);
                        }}
                      >
                        {column !== undefined && column.pk > 0 ? (
                          <KeyIcon />
                        ) : null}
                        {name}
                        <span class="text-xs font-normal opacity-50">
                          {column?.type ?? ""}
                        </span>
                        {mark === null ? null : (
                          <span aria-hidden="true">{mark}</span>
                        )}
                      </button>
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody>
              {data.rows.map((cells, index) => {
                const record: Record<string, D1Cell> = {};
                for (const [position, name] of data.columns.entries()) {
                  record[name] = cells[position] ?? null;
                }

                return (
                  <tr
                    key={String(index)}
                    class={canWrite ? "hover cursor-pointer" : undefined}
                    onclick={
                      canWrite
                        ? () => {
                            onOpenRow(record);
                          }
                        : undefined
                    }
                  >
                    {canWrite ? (
                      <td
                        onclick={(event) => {
                          event.stopPropagation();
                        }}
                      >
                        <input
                          class="checkbox checkbox-sm"
                          type="checkbox"
                          aria-label={`Select row ${index + 1}`}
                          checked={selected()[String(index)] === true}
                          onchange={(event) => {
                            const next: Record<string, true> = {};
                            for (const [key] of Object.entries(selected())) {
                              if (key !== String(index)) {
                                next[key] = true;
                              }
                            }
                            if (event.currentTarget.checked) {
                              next[String(index)] = true;
                            }
                            selected.set(next);
                          }}
                        />
                      </td>
                    ) : null}
                    {data.columns.map((name, columnIndex) => {
                      const column = schemaColumn(name);

                      return (
                        <td key={name} class="align-middle">
                          <CellValue
                            cell={cells[columnIndex] ?? null}
                            columnName={name}
                            columnType={column?.type ?? ""}
                            timestamp={isTimestampColumn(
                              name,
                              column?.type ?? "",
                            )}
                          />
                        </td>
                      );
                    })}
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
      <div class="border-base-300 flex flex-wrap items-center justify-between gap-2 border-t px-3 py-2">
        <Pager
          onPage={onPage}
          onSize={onSize}
          page={data.page}
          pageSize={data.page_size}
          total={data.total}
        />
        {viewSlot}
      </div>
    </div>
  );
};
