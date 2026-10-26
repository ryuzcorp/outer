// Account hub routing: old top-level `?t=` URLs land on the matching hub
// tab, and the hub emits canonical `?t=account&acctab=` URLs.
import { afterEach, describe, expect, test } from "bun:test";

import {
  readRoute,
  routeUrl,
  toAccountRoute,
  toAcctab,
  type Route,
} from "./common.tsx";

const scope = globalThis as unknown as Record<string, unknown>;

function stubSearch(search: string): void {
  scope["window"] = { location: { search } };
}

afterEach(() => {
  delete scope["window"];
});

function route(over: Partial<Route>): Route {
  return {
    app: null,
    view: "overview",
    acctab: "account",
    range: 24,
    sha: null,
    codeTab: null,
    db: "",
    table: "",
    page: "",
    size: "",
    sort: "",
    f: "",
    q: "",
    dview: "",
    ...over,
  };
}

describe("account hub routes", () => {
  test("legacy ?t= URLs land on the matching hub tab", () => {
    const cases = [
      ["account", "account"],
      ["usage", "usage"],
      ["connections", "connections"],
      ["invites", "invites"],
    ] as const;
    for (const [t, tab] of cases) {
      stubSearch(`?t=${t}`);
      const r = readRoute();
      expect(r.view).toBe("account");
      expect(r.acctab).toBe(tab);
    }
  });

  test("the hub tab rides ?acctab=, defaulting to account", () => {
    stubSearch("?t=account&acctab=usage");
    expect(readRoute().acctab).toBe("usage");
    stubSearch("?t=account&acctab=nope");
    expect(readRoute().acctab).toBe("account");
    stubSearch("?t=account");
    expect(readRoute().acctab).toBe("account");
  });

  test("admin stays its own view", () => {
    stubSearch("?t=admin");
    const r = readRoute();
    expect(r.view).toBe("admin");
  });

  test("an ?app= slug survives the legacy mapping", () => {
    stubSearch("?app=groceries&t=usage");
    const r = readRoute();
    expect(r.app).toBe("groceries");
    expect(r.view).toBe("account");
    expect(r.acctab).toBe("usage");
  });

  test("canonical URLs carry the tab; the default stays bare", () => {
    expect(routeUrl(route({ view: "account", acctab: "usage" }))).toBe(
      "/console?t=account&acctab=usage&r=24",
    );
    expect(routeUrl(route({ view: "account", acctab: "account" }))).toBe(
      "/console?t=account&r=24",
    );
    expect(routeUrl(route({ app: "groceries", view: "account", acctab: "connections" }))).toBe(
      "/console?app=groceries&t=account&acctab=connections&r=24",
    );
  });

  test("toAccountRoute leaves app views and admin alone", () => {
    expect(toAccountRoute("overview")).toBeNull();
    expect(toAccountRoute("admin")).toBeNull();
    expect(toAccountRoute("usage")).toEqual({ view: "account", acctab: "usage" });
    expect(toAcctab("connections")).toBe("connections");
    expect(toAcctab("nope")).toBe("account");
  });
});
