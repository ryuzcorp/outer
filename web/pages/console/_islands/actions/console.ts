// Console navigation actions: agents need to see and drive the console
// itself, so these three join every other `actions/*.ts` registry in
// `index.ts` and are exposed through the palette and WebMCP like the rest.
//
// They reach the Console island's `navigate` and route state through the
// bridge the island builds on mount and hands down in the execute context:
// no module state, per the repo's explicit-dependencies rule.
import {
  ACCOUNT_VIEWS,
  DATA_VIEWS,
  VIEWS,
  toAccountRoute,
  type AccountTabId,
  type Route,
  type ViewId,
} from "../common.tsx";
import type { ActionContext, ConsoleAction, ConsoleBridge } from "./settings.ts";

function needBridge(ctx: ActionContext | undefined): ConsoleBridge {
  if (!ctx?.bridge) {
    throw new Error("The console is not ready yet — open it and try again.");
  }
  return ctx.bridge;
}

const VIEW_IDS: ViewId[] = [
  ...VIEWS.map((v) => v.id as ViewId),
  ...DATA_VIEWS.map((v) => v.id as ViewId),
  ...ACCOUNT_VIEWS.map((v) => v.id as ViewId),
];

function isViewId(raw: unknown): raw is ViewId {
  return typeof raw === "string" && (VIEW_IDS as string[]).includes(raw);
}

export interface ConsoleStateOutput {
  email: string;
  slug: string | null;
  view: ViewId;
  acctab: AccountTabId;
  params: Record<string, string | number | null>;
  apps: { slug: string; role: string; status: string }[];
}

const consoleState: ConsoleAction<Record<string, never>, ConsoleStateOutput> = {
  name: "console_state",
  title: "Console state",
  description:
    "The console itself: the signed-in email, the current app slug, view and route params, and the app list with role and status. Read-only; call it first to learn what is open.",
  inputSchema: { type: "object", properties: {} },
  readOnly: true,
  consequential: false,
  untrusted: false,
  async run(_input, ctx?: ActionContext): Promise<ConsoleStateOutput> {
    const b = needBridge(ctx);
    const route = b.getRoute();
    return {
      email: b.getEmail(),
      slug: route.app,
      view: route.view,
      acctab: route.acctab,
      params: {
        range: route.range,
        sha: route.sha,
        codeTab: route.codeTab,
        db: route.db || null,
        table: route.table || null,
      },
      apps: b
        .getApps()
        .map((a) => ({ slug: a.slug, role: a.role, status: a.status })),
    };
  },
};

export interface OpenAppInput {
  slug?: string;
  view?: string;
}

const openApp: ConsoleAction<OpenAppInput, { slug: string; view: ViewId }> = {
  name: "open_app",
  title: "Open app",
  description:
    "Switch the console to an app, optionally on a view (defaults to overview). The slug defaults to the current app. Returns the opened {slug, view}.",
  inputSchema: {
    type: "object",
    properties: {
      slug: { type: "string", description: "The app slug (defaults to the current app)." },
      view: {
        type: "string",
        description: "A console view id (defaults to overview).",
        enum: VIEW_IDS as unknown as string[],
      },
    },
  },
  readOnly: true,
  consequential: false,
  untrusted: false,
  async run(input, ctx?: ActionContext): Promise<{ slug: string; view: ViewId }> {
    const b = needBridge(ctx);
    const slug = input.slug ?? b.getRoute().app;
    if (!slug) {
      throw new Error("No app is open and no slug was given.");
    }
    if (!b.getApps().some((a) => a.slug === slug)) {
      const known = b.getApps().map((a) => a.slug).join(", ");
      throw new Error(`Unknown app "${slug}". Open one of: ${known || "(no apps yet)"}.`);
    }
    let view: ViewId = input.view === undefined ? "overview" : input.view as ViewId;
    if (input.view !== undefined && !isViewId(input.view)) {
      throw new Error(`Unknown view "${input.view}".`);
    }
    const mapped = toAccountRoute(view);
    const target: Partial<Route> = mapped
      ? { app: slug, view: mapped.view, acctab: mapped.acctab }
      : { app: slug, view };
    b.navigate(target);
    if (mapped) {
      view = mapped.view;
    }
    return { slug, view };
  },
};

export interface OpenViewInput {
  view: string;
  sha?: string | null;
  codeTab?: "files" | "changes" | null;
  db?: string;
  table?: string;
}

const openView: ConsoleAction<OpenViewInput, { slug: string | null; view: ViewId; acctab: AccountTabId }> = {
  name: "open_view",
  title: "Open view",
  description:
    "Navigate within the current app: views, the Account hub tabs (account, usage, connections, invites), admin, and D1/Code deep links (db, table, sha, codeTab). The app is the current one; pass an app-scoped view with no app open rejects. Returns the opened {slug, view, acctab}.",
  inputSchema: {
    type: "object",
    properties: {
      view: {
        type: "string",
        description: "A console view id.",
        enum: VIEW_IDS as unknown as string[],
      },
      sha: { type: "string", description: "Code view: pin this commit sha." },
      codeTab: { type: "string", enum: ["files", "changes"] },
      db: { type: "string", description: "D1 view: the binding to show." },
      table: { type: "string", description: "D1 view: the table to show." },
    },
    required: ["view"],
  },
  readOnly: true,
  consequential: false,
  untrusted: false,
  async run(input, ctx?: ActionContext): Promise<{ slug: string | null; view: ViewId; acctab: AccountTabId }> {
    const b = needBridge(ctx);
    if (!isViewId(input.view)) {
      throw new Error(`Unknown view "${input.view}".`);
    }
    const route = b.getRoute();
    const accountView = (ACCOUNT_VIEWS.map((v) => v.id) as string[]).includes(input.view);
    if (!accountView && !route.app) {
      throw new Error("No app is open — open_app first, or pick an account view.");
    }
    // Agents send values outside the declared schema (registerTool does
    // not enforce it), so check the free-form fields before they reach
    // the route and the views.
    const mapped = toAccountRoute(input.view);
    const next: Partial<Route> = mapped
      ? { view: mapped.view, acctab: mapped.acctab }
      : { view: input.view };
    if (input.sha !== undefined) {
      if (input.sha !== null && typeof input.sha !== "string") {
        throw new Error("sha must be a string.");
      }
      next.sha = input.sha;
    }
    if (input.codeTab !== undefined) {
      if (input.codeTab !== null && input.codeTab !== "files" && input.codeTab !== "changes") {
        throw new Error(`Unknown codeTab "${String(input.codeTab)}".`);
      }
      next.codeTab = input.codeTab;
    }
    if (input.db !== undefined) {
      if (typeof input.db !== "string") {
        throw new Error("db must be a string.");
      }
      next.db = input.db;
    }
    if (input.table !== undefined) {
      if (typeof input.table !== "string") {
        throw new Error("table must be a string.");
      }
      next.table = input.table;
    }
    b.navigate(next);
    return { slug: route.app, view: mapped?.view ?? input.view, acctab: mapped?.acctab ?? route.acctab };
  },
};

export const actions: ConsoleAction<unknown, unknown>[] = [
  consoleState as ConsoleAction<unknown, unknown>,
  openApp as ConsoleAction<unknown, unknown>,
  openView as ConsoleAction<unknown, unknown>,
];

export const consoleActions = { consoleState, openApp, openView };
