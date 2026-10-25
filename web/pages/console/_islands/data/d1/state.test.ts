// URL-state round-trips for the D1 editor (adapted from Noite's
// `lib/storage/d1/state.test.ts`, noite@58a977a: redacted-column guards are
// gone with the owner-only console).

import { describe, expect, test } from "bun:test";

import type { D1TableSchema } from "./types.ts";
import {
  decodeFilters,
  decodeSort,
  encodeFilters,
  encodeSort,
  MAX_FILTERS as maxFilters,
  pageCount,
  toPageIndex,
  toPageSize,
  toView,
  validFilters,
  validSort,
} from "./state.ts";

describe("filters", () => {
  test("round-trips values containing the separators", () => {
    const filters = [
      { column: "name", op: "eq" as const, value: "a~b|c" },
      { column: "note", op: "like" as const, value: "%50%+" },
      { column: "deletedAt", op: "is_null" as const, value: "" },
    ];

    expect(decodeFilters(encodeFilters(filters))).toEqual(filters);
  });

  test("drops malformed entries instead of failing the query", () => {
    expect(decodeFilters("nope")).toEqual([]);
    expect(decodeFilters("a~bogus~1")).toEqual([]);
    expect(decodeFilters("~eq~1")).toEqual([]);
    expect(decodeFilters("a~eq~%E0%A4%A")).toEqual([]);
  });

  test("caps at the server's filter limit", () => {
    const many = Array.from({ length: 14 }, (_, i) => ({
      column: `c${i}`,
      op: "eq" as const,
      value: String(i),
    }));

    expect(decodeFilters(encodeFilters(many))).toHaveLength(maxFilters);
  });
});

describe("sort", () => {
  test("round-trips a column that contains a colon", () => {
    const sort = { column: "a:b", desc: true };
    expect(decodeSort(encodeSort(sort))).toEqual(sort);
  });

  test("no sort is an empty string", () => {
    expect(encodeSort(null)).toBe("");
    expect(decodeSort("")).toBeNull();
    expect(decodeSort("name:sideways")).toBeNull();
    expect(decodeSort("nocolon")).toBeNull();
  });
});

describe("scalars", () => {
  test("page, size and view parse defensively", () => {
    expect(toPageIndex("-3")).toBe(0);
    expect(toPageIndex("x")).toBe(0);
    expect(toPageIndex("4")).toBe(4);
    expect(toPageSize("50")).toBe(50);
    expect(toPageSize("10")).toBe(25);
    expect(toPageSize("9000")).toBe(25);
    expect(toView("definition")).toBe("definition");
    expect(toView("bogus")).toBe("data");
    expect(pageCount(0, 25)).toBe(1);
    expect(pageCount(51, 25)).toBe(3);
  });
});

const SCHEMA: D1TableSchema = {
  binding: "DB",
  columns: [
    { default_value: null, name: "id", not_null: true, pk: 1, type: "TEXT" },
    { default_value: null, name: "note", not_null: false, pk: 0, type: "TEXT" },
  ],
  foreign_keys: [],
  indexes: [],
  sql: null,
  table: "session",
};

describe("validation against a schema", () => {
  test("an unknown sort column is dropped", () => {
    expect(validSort({ column: "nope", desc: false }, SCHEMA)).toBeNull();
    expect(validSort({ column: "note", desc: true }, SCHEMA)).toEqual({
      column: "note",
      desc: true,
    });
    expect(validSort({ column: "note", desc: false })).toBeNull();
  });

  test("filters on unknown columns are dropped", () => {
    expect(
      validFilters(
        [
          { column: "nope", op: "eq", value: "x" },
          { column: "note", op: "eq", value: "x" },
        ],
        SCHEMA,
      ),
    ).toEqual([{ column: "note", op: "eq", value: "x" }]);
    expect(validFilters([{ column: "note", op: "eq", value: "x" }])).toEqual(
      [],
    );
  });
});
