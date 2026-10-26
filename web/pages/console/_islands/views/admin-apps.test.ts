// Query-builder unit tests for the admin All Apps directory view.
import { describe, expect, test } from "bun:test";

import { adminAppsQuery } from "./admin-apps-query.ts";

describe("adminAppsQuery", () => {
  test("empty params build no query string", () => {
    expect(adminAppsQuery({})).toBe("");
    expect(adminAppsQuery({ q: "", state: "", plan: "" })).toBe("");
  });

  test("sets only the filters given", () => {
    expect(adminAppsQuery({ q: "gro" })).toBe("?q=gro");
    expect(adminAppsQuery({ state: "held", plan: "pro" })).toBe("?state=held&plan=pro");
  });

  test("paging rides alongside the filters", () => {
    expect(adminAppsQuery({ q: "a b", limit: 50, before: "1:2" })).toBe(
      "?q=a+b&limit=50&before=1%3A2",
    );
  });
});
