// The shell keys every view loader by {slug, view, range, epoch}: a
// response lands only when its key still matches, quiet poll failures keep
// the last value as `stale` instead of passing as live, and only a loud
// first-load failure becomes `failed`.
import { describe, expect, test } from "bun:test";

import {
  emptyRegistry,
  reduceFetch,
  sameKey,
  settleFetch,
  staleMessage,
  startFetch,
  type DataKey,
  type FetchState,
} from "./state.ts";

function key(over: Partial<DataKey> = {}): DataKey {
  return { slug: "shop", view: "overview", range: 24, epoch: 7, ...over };
}

describe("sameKey", () => {
  test("matches identical keys", () => {
    expect(sameKey(key(), key())).toBe(true);
  });

  test("rejects a change in any single field", () => {
    expect(sameKey(key(), key({ slug: "blog" }))).toBe(false);
    expect(sameKey(key(), key({ view: "metrics" }))).toBe(false);
    expect(sameKey(key(), key({ range: 168 }))).toBe(false);
    expect(sameKey(key(), key({ epoch: 8 }))).toBe(false);
  });
});

describe("reduceFetch", () => {
  test("drops actions whose key is not current", () => {
    const state: FetchState = { status: "ready", at: 1 };
    expect(
      reduceFetch(state, key(), { type: "succeeded", key: key({ slug: "other" }), at: 2 }),
    ).toBe(state);
    expect(
      reduceFetch(state, key(), {
        type: "failed",
        key: key({ view: "logs" }),
        message: "boom",
      }),
    ).toBe(state);
    expect(
      reduceFetch(state, key(), {
        type: "failed",
        key: key({ range: 168 }),
        message: "boom",
      }),
    ).toBe(state);
    expect(
      reduceFetch(state, key(), {
        type: "failed",
        key: key({ epoch: 8 }),
        message: "boom",
      }),
    ).toBe(state);
  });

  test("ignores every action when there is no current key", () => {
    const state: FetchState = { status: "idle" };
    expect(
      reduceFetch(state, null, { type: "succeeded", key: key(), at: 2 }),
    ).toBe(state);
  });

  test("quiet reload failure after ready keeps the value as stale", () => {
    const next = reduceFetch(
      { status: "ready", at: 1 },
      key(),
      { type: "failed", key: key(), message: "net down" },
    );
    expect(next).toEqual({ status: "stale", at: 1, message: "net down" });
  });

  test("first-load failure becomes failed", () => {
    for (const state of [
      { status: "idle" },
      { status: "loading" },
      { status: "failed", message: "old" },
    ] as FetchState[]) {
      expect(
        reduceFetch(state, key(), { type: "failed", key: key(), message: "net down" }),
      ).toEqual({ status: "failed", message: "net down" });
    }
  });

  test("quiet reload keeps ready instead of flashing loading", () => {
    const state: FetchState = { status: "ready", at: 1 };
    expect(
      reduceFetch(state, key(), { type: "began", key: key(), quiet: true }),
    ).toBe(state);
  });

  test("loud reload leaves ready for loading", () => {
    expect(
      reduceFetch(
        { status: "ready", at: 1 },
        key(),
        { type: "began", key: key(), quiet: false },
      ),
    ).toEqual({ status: "loading" });
  });

  test("success clears stale", () => {
    expect(
      reduceFetch(
        { status: "stale", at: 1, message: "net down" },
        key(),
        { type: "succeeded", key: key(), at: 2 },
      ),
    ).toEqual({ status: "ready", at: 2 });
  });
});

describe("registry", () => {
  test("late response for another app is dropped", () => {
    let r = emptyRegistry();
    r = startFetch(r, "actions", key(), false).registry;
    const settled = settleFetch(r, "actions", key({ slug: "other" }), true, undefined, 2);
    expect(settled).toBe(r);
    expect(settled.states.actions).toEqual({ status: "loading" });
  });

  test("quiet failure after ready records stale without touching keys", () => {
    let r = emptyRegistry();
    r = startFetch(r, "metrics", key(), false).registry;
    r = settleFetch(r, "metrics", key(), true, undefined, 1);
    r = startFetch(r, "metrics", key(), true).registry;
    r = settleFetch(r, "metrics", key(), false, "net down");
    expect(r.states.metrics).toEqual({ status: "stale", at: 1, message: "net down" });
  });

  test("staleMessage only reports the given ids", () => {
    let r = emptyRegistry();
    r = startFetch(r, "metrics", key(), false).registry;
    r = settleFetch(r, "metrics", key(), true, undefined, 1);
    r = startFetch(r, "metrics", key(), true).registry;
    r = settleFetch(r, "metrics", key(), false, "net down");
    expect(staleMessage(r, ["metrics", "actions"])).toBe("net down");
    expect(staleMessage(r, ["actions", "errors"])).toBeNull();
    expect(staleMessage(r, [])).toBeNull();
  });
});
