// Account hub routing: old top-level `?t=` URLs land on the matching hub
// tab, and the hub emits canonical `?t=account&acctab=` URLs.
import { afterEach, describe, expect, test } from "bun:test";

import {
  canOpenApp,
  hrefFor,
  mergeRoute,
  readRoute,
  routeUrl,
  toAccountRoute,
  toAcctab,
  toRange,
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

  test("toAccountRoute leaves the hub itself alone", () => {
    // The hub's own `?t=account` must not reset an explicit tab: merging
    // `{ view: "account", acctab }` keeps the tab (this was the stuck-tab
    // bug: every hub click re-landed on Account).
    expect(toAccountRoute("account")).toBeNull();
    expect(mergeRoute(route({ acctab: "usage" }), { view: "account", acctab: "connections" })).toMatchObject({
      view: "account",
      acctab: "connections",
    });
    expect(mergeRoute(route({ acctab: "usage" }), { view: "usage" })).toMatchObject({
      view: "account",
      acctab: "usage",
    });
  });

  test("hub tab hrefs are canonical and survive a reload", () => {
    const href = hrefFor(route({ app: "groceries", view: "overview" }), {
      view: "account",
      acctab: "usage",
    });
    expect(href).toBe("/console?app=groceries&t=account&acctab=usage&r=24");
    stubSearch(new URL(href, "https://outer.now").search);
    expect(readRoute()).toMatchObject({ app: "groceries", view: "account", acctab: "usage" });
  });
});

describe("range param", () => {
  test("toRange accepts the shell's ranges, defaulting to 24h", () => {
    expect(toRange("24")).toBe(24);
    expect(toRange("168")).toBe(168);
    expect(toRange("720")).toBe(720);
    expect(toRange("nope")).toBe(24);
    expect(toRange(null)).toBe(24);
  });

  test("the range rides ?r= and survives a reload", () => {
    for (const hours of [24, 168, 720] as const) {
      const href = hrefFor(route({ app: "groceries", view: "metrics" }), { range: hours });
      expect(href).toBe(`/console?app=groceries&t=metrics&r=${hours}`);
      stubSearch(new URL(href, "https://outer.now").search);
      expect(readRoute()).toMatchObject({ app: "groceries", view: "metrics", range: hours });
    }
  });

  test("switching only the range keeps the app and view", () => {
    const r = mergeRoute(route({ app: "groceries", view: "overview", range: 24 }), { range: 720 });
    expect(r).toMatchObject({ app: "groceries", view: "overview", range: 720 });
    expect(hrefFor(route({ app: "groceries", view: "overview", range: 24 }), { range: 720 })).toBe(
      "/console?app=groceries&t=overview&r=720",
    );
  });
});

describe("foreign-app access", () => {
  test("admins may open slugs outside their own rail", () => {
    expect(canOpenApp(true, ["shop"], "groceries")).toBe(true);
    expect(canOpenApp(true, ["shop"], "shop")).toBe(true);
    expect(canOpenApp(true, [], "groceries")).toBe(true);
  });

  test("everyone else only opens rail apps", () => {
    expect(canOpenApp(false, ["shop"], "shop")).toBe(true);
    expect(canOpenApp(false, ["shop"], "groceries")).toBe(false);
    expect(canOpenApp(false, [], "groceries")).toBe(false);
  });
});
