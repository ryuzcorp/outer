// ⌘K palette + WebMCP layer: registry shape, annotation mapping, the
// current-app default, and the consequential confirm gate. Real Chrome
// WebMCP needs the origin trial, so the browser half is Playwright; this
// file pins the behavior a caller would notice without a browser.
import { afterEach, describe, expect, test } from "bun:test";

import { allActions, findAction, type AnyAction } from "./actions/index.ts";
import { consoleActions, setConsoleBridge } from "./actions/console.ts";
import type { ConsoleApp } from "./api.ts";
import type { Route } from "./common.tsx";
import { actionView, isRunnable, matchScore } from "./palette.tsx";
import {
  applyDefaults,
  executeAction,
  registerAllWebMcp,
  resetWebMcpSync,
  setWebMcpConfirm,
  syncWebMcp,
  toAnnotations,
  webMcpDescription,
  webMcpSchema,
} from "./webmcp.ts";

function fakeAction(overrides: Partial<AnyAction> = {}): AnyAction {
  return {
    name: "fake_action",
    title: "Fake",
    description: "A test action.",
    inputSchema: {
      type: "object",
      properties: { slug: { type: "string" } },
      required: ["slug"],
    },
    readOnly: false,
    consequential: false,
    untrusted: false,
    run: async (input: unknown) => input,
    ...overrides,
  } as AnyAction;
}

// Test seam for the module's globalThis feature-detection.
const testScope = globalThis as unknown as Record<string, unknown>;

function stubDocument(modelContext: unknown): void {
  testScope["document"] = { modelContext };
}

function unstub(): void {
  delete testScope["document"];
  delete testScope["navigator"];
}

afterEach(() => {
  unstub();
  resetWebMcpSync();
  setWebMcpConfirm(null);
  setConsoleBridge(null);
});

describe("registry", () => {
  test("action names are unique and WebMCP-valid", () => {
    const names = allActions.map((a) => a.name);
    expect(new Set(names).size).toBe(names.length);
    expect(names.length).toBeGreaterThan(40);
    for (const name of names) {
      expect(name).toMatch(/^[A-Za-z0-9_.-]{1,128}$/);
    }
  });

  test("every action carries agent-facing metadata and a run function", () => {
    for (const action of allActions) {
      expect(action.title.length).toBeGreaterThan(0);
      expect(action.description.length).toBeGreaterThan(0);
      expect(typeof action.inputSchema).toBe("object");
      expect(typeof action.run).toBe("function");
      expect(typeof action.readOnly).toBe("boolean");
      expect(typeof action.consequential).toBe("boolean");
      expect(typeof action.untrusted).toBe("boolean");
    }
  });

  test("navigation actions are registered alongside the rest", () => {
    for (const name of ["console_state", "open_app", "open_view"]) {
      expect(findAction(name)).not.toBeNull();
    }
  });
});

describe("annotations", () => {
  test("flags map to the matching hints", () => {
    expect(toAnnotations(findAction("delete_app")!)).toEqual({
      readOnlyHint: false,
      consequentialHint: true,
      untrustedContentHint: false,
    });
    expect(toAnnotations(findAction("query_logs")!)).toEqual({
      readOnlyHint: true,
      consequentialHint: false,
      untrustedContentHint: true,
    });
    expect(toAnnotations(findAction("set_visibility")!)).toEqual({
      readOnlyHint: false,
      consequentialHint: false,
      untrustedContentHint: false,
    });
  });
});

describe("slug defaults", () => {
  test("missing slug/app fills from the current app", () => {
    expect(applyDefaults(findAction("list_secrets")!, {}, "groceries")).toEqual({
      slug: "groceries",
    });
    expect(
      applyDefaults(findAction("list_collaborators")!, {}, "groceries"),
    ).toEqual({ app: "groceries" });
  });

  test("explicit values win; null app leaves input alone", () => {
    expect(
      applyDefaults(findAction("list_secrets")!, { slug: "other" }, "groceries"),
    ).toEqual({ slug: "other" });
    expect(applyDefaults(findAction("list_secrets")!, {}, null)).toEqual({});
  });

  test("non-object input starts empty", () => {
    expect(applyDefaults(findAction("get_account")!, undefined, "groceries")).toEqual(
      {},
    );
  });

  test("schema copy drops the defaulted keys from required, source untouched", () => {
    const action = findAction("delete_app")!;
    expect(webMcpSchema(action)).toMatchObject({ required: ["confirm"] });
    expect(action.inputSchema).toMatchObject({
      required: ["slug", "confirm"],
    });
    expect(webMcpSchema(findAction("list_connections")!)).toMatchObject({
      type: "object",
    });
  });

  test("description notes the default only when one exists", () => {
    expect(webMcpDescription(findAction("list_secrets")!)).toContain("current");
    expect(webMcpDescription(findAction("get_account")!)).not.toContain(
      "current",
    );
  });
});

describe("confirm gate", () => {
  test("consequential runs wait for Confirm; Cancel runs nothing", async () => {
    let ran = 0;
    const action = fakeAction({
      consequential: true,
      run: async (input: unknown) => {
        ran += 1;
        return input;
      },
    });
    const ctx = { getSlug: () => "groceries" };
    setWebMcpConfirm(null);
    await expect(executeAction(action, {}, ctx)).rejects.toThrow();
    expect(ran).toBe(0);
    setWebMcpConfirm(async () => {});
    const out = await executeAction(action, {}, ctx);
    expect(ran).toBe(1);
    expect(out).toEqual({ slug: "groceries" });
    setWebMcpConfirm(async () => {
      throw new Error('"fake_action" was cancelled — nothing ran.');
    });
    await expect(executeAction(action, {}, ctx)).rejects.toThrow("cancelled");
    expect(ran).toBe(1);
  });

  test("abort rejects without running", async () => {
    let ran = 0;
    let confirmed = false;
    const action = fakeAction({
      consequential: true,
      run: async () => {
        ran += 1;
        return {};
      },
    });
    setWebMcpConfirm(async () => {
      confirmed = true;
    });
    const controller = new AbortController();
    controller.abort();
    await expect(
      executeAction(action, {}, { getSlug: () => "groceries" }, controller.signal),
    ).rejects.toThrow("abort");
    expect(ran).toBe(0);
    expect(confirmed).toBe(false);
  });

  test("server user_message passes through as the rejection", async () => {
    const action = fakeAction({
      run: async () => {
        throw new Error("Free owners cannot set public: upgrade to Pro.");
      },
    });
    await expect(
      executeAction(action, {}, { getSlug: () => "groceries" }),
    ).rejects.toThrow("Free owners cannot set public");
  });

  test("the presented schema drops defaulted keys, source untouched", () => {
    const action = findAction("rollback")!;
    const presented = webMcpSchema(action);
    const required: unknown = "required" in presented ? presented.required : [];
    expect(Array.isArray(required) && !required.includes("slug")).toBe(true);
    expect(action.inputSchema).toMatchObject({ required: ["slug", "sha"] });
  });
});

describe("registration", () => {
  test("every action registers with annotations; cleanup aborts the set", () => {
    const calls: { tool: Record<string, unknown>; signal?: AbortSignal }[] = [];
    stubDocument({
      registerTool: (tool: Record<string, unknown>, opts?: { signal?: AbortSignal }) => {
        calls.push({ tool, signal: opts?.signal });
      },
    });
    const cleanup = registerAllWebMcp({ getSlug: () => "groceries" });
    expect(cleanup).not.toBeNull();
    expect(calls.length).toBe(allActions.length);
    const names = calls.map((c) => String(c.tool["name"])).sort();
    expect(names).toEqual(allActions.map((a) => a.name).sort());
    const rollback = calls.find((c) => c.tool["name"] === "rollback")!;
    expect(rollback.tool["annotations"]).toEqual({
      readOnlyHint: false,
      consequentialHint: true,
      untrustedContentHint: true,
    });
    expect(calls[0].signal?.aborted).toBe(false);
    cleanup!();
    expect(calls[0].signal?.aborted).toBe(true);
  });

  test("navigator.modelContext is the fallback; absent means null", () => {
    expect(registerAllWebMcp({ getSlug: () => null })).toBeNull();
    testScope["navigator"] = {
      modelContext: { registerTool: () => {} },
    };
    expect(registerAllWebMcp({ getSlug: () => null })).not.toBeNull();
  });

  test("sync re-registers only on identity change and aborts on sign-out", () => {
    let count = 0;
    const signals: (AbortSignal | undefined)[] = [];
    stubDocument({
      registerTool: (_tool: unknown, opts?: { signal?: AbortSignal }) => {
        count += 1;
        signals.push(opts?.signal);
      },
    });
    const ctx = { getSlug: () => null as string | null };
    expect(syncWebMcp("ana@example.com", ctx)).toBe(true);
    expect(count).toBe(allActions.length);
    expect(syncWebMcp("ana@example.com", ctx)).toBe(true);
    expect(count).toBe(allActions.length);
    expect(syncWebMcp("ben@example.com", ctx)).toBe(true);
    expect(count).toBe(allActions.length * 2);
    expect(signals[0]?.aborted).toBe(true);
    expect(syncWebMcp(null, ctx)).toBe(true);
    expect(signals[allActions.length]?.aborted).toBe(true);
  });
});

describe("console bridge", () => {
  const apps: ConsoleApp[] = [
    {
      slug: "groceries",
      url: "https://groceries.outer.now",
      role: "owner",
      visibility: "unlisted",
      status: "running",
      last_request_at: null,
    },
  ];
  const route: Route = {
    app: "groceries",
    view: "overview",
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
  };

  test("actions reject before the island mounts", async () => {
    await expect(consoleActions.consoleState.run({} as never)).rejects.toThrow(
      "not ready",
    );
  });

  test("state, open_app and open_view drive the island", async () => {
    const navigated: Partial<Route>[] = [];
    setConsoleBridge({
      getEmail: () => "ana@example.com",
      getRoute: () => route,
      getApps: () => apps,
      navigate: (next) => {
        navigated.push(next);
      },
    });
    const state = await consoleActions.consoleState.run({} as never);
    expect(state.email).toBe("ana@example.com");
    expect(state.slug).toBe("groceries");
    expect(state.apps).toEqual([
      { slug: "groceries", role: "owner", status: "running" },
    ]);
    const opened = await consoleActions.openApp.run({});
    expect(opened).toEqual({ slug: "groceries", view: "overview" });
    expect(navigated.at(-1)).toEqual({ app: "groceries", view: "overview" });
    await expect(consoleActions.openApp.run({ slug: "nope" })).rejects.toThrow(
      'Unknown app "nope"',
    );
    const viewed = await consoleActions.openView.run({ view: "deploys" });
    expect(viewed).toEqual({ slug: "groceries", view: "deploys" });
    await expect(consoleActions.openView.run({ view: "nope" })).rejects.toThrow(
      "Unknown view",
    );
  });
});

describe("palette", () => {
  test("runnable means context-only inputs", () => {
    expect(isRunnable(findAction("list_secrets")!)).toBe(true);
    expect(isRunnable(findAction("list_connections")!)).toBe(true);
    expect(isRunnable(findAction("console_state")!)).toBe(true);
    expect(isRunnable(findAction("rollback")!)).toBe(false);
    expect(isRunnable(findAction("set_secret")!)).toBe(false);
    expect(isRunnable(findAction("code_read_file")!)).toBe(false);
  });
  test("input-needing actions map to their views", () => {
    expect(actionView(findAction("rollback")!)).toBe("deploys");
    expect(actionView(findAction("set_secret")!)).toBe("secrets");
    expect(actionView(findAction("rename_app")!)).toBe("settings");
    expect(actionView(findAction("d1_rows")!)).toBe("d1");
  });

  test("domains: refresh is runnable, add navigates to settings", () => {
    expect(isRunnable(findAction("refresh_domains")!)).toBe(true);
    expect(isRunnable(findAction("list_domains")!)).toBe(true);
    expect(isRunnable(findAction("remove_domain")!)).toBe(true);
    expect(isRunnable(findAction("add_domain")!)).toBe(false);
    expect(actionView(findAction("add_domain")!)).toBe("settings");
    expect(findAction("remove_domain")!.consequential).toBe(true);
  });
  test("substring matching ranks title hits first", () => {
    expect(matchScore("Delete app delete_app", "del")).toBeGreaterThanOrEqual(0);
    expect(matchScore("Delete app delete_app", "zzz")).toBe(-1);
    expect(matchScore("Delete app", "")).toBe(0);
    const atStart = matchScore("Logs query", "logs");
    const later = matchScore("Query logs", "logs");
    expect(atStart).toBeLessThan(later);
  });
});
