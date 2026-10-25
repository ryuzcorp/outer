// ⌘K palette (spec §13.1): a daisyUI `modal` searching apps, views and
// registered actions. It runs what it can from context and navigates (plus
// focuses the right field) for the rest; consequential actions go through
// the same confirm modal as WebMCP. New file, so the parallel polish
// branch's console.tsx edits stay trivial to merge.
import { atom } from "ilha";
import {
  ACCOUNT_VIEWS,
  DATA_VIEWS,
  VIEWS,
  type Route,
  type ViewId,
} from "./common.tsx";
import type { ConsoleApp } from "./api.ts";
import type { AnyAction } from "./actions/index.ts";
import { allActions } from "./actions/index.ts";
import { executeAction, type ExecuteContext } from "./webmcp.ts";

export interface PaletteProps {
  apps: ConsoleApp[];
  slug: string | null;
  view: ViewId;
  isAdmin: boolean;
  onNavigate: (next: Partial<Route>) => void;
  execCtx: ExecuteContext;
  onMutated: () => void;
  onClose: () => void;
}

export type PaletteItem =
  | { kind: "app"; key: string; title: string; sub: string }
  | { kind: "view"; key: string; title: string; sub: string; view: ViewId }
  | {
      kind: "action";
      key: string;
      title: string;
      sub: string;
      action: AnyAction;
      runnable: boolean;
    };

export interface ItemGroup {
  title: string;
  items: PaletteItem[];
}

/** Navigation primitives live in the Apps/Views sections, not Actions. */
const EXCLUDED_FROM_PALETTE: Record<string, true> = { open_app: true, open_view: true };

/**
 * Palette-runnable: every required input comes from context (the current
 * slug/app) or nothing is required at all. Everything else navigates to
 * its view instead of guessing arguments.
 */
export function isRunnable(action: AnyAction): boolean {
  const schema = action.inputSchema as { required?: unknown };
  const required = Array.isArray(schema.required) ? schema.required : [];
  return required.every((name) => name === "slug" || name === "app");
}

/** Where an action that needs real input sends the user. */
const ACTION_VIEWS: Record<string, ViewId> = {
  set_visibility: "settings",
  rename_app: "settings",
  delete_app: "settings",
  add_collaborator: "settings",
  remove_collaborator: "settings",
  add_domain: "settings",
  set_secret: "secrets",
  unset_secret: "secrets",
  rollback: "deploys",
  create_api_key: "connections",
  revoke_connection: "connections",
  revoke_api_key: "connections",
  delete_account: "account",
  sign_out_others: "account",
  resolve_error: "errors",
  ignore_error: "errors",
  reopen_error: "errors",
  bulk_triage: "errors",
  error_detail: "errors",
  query_logs: "logs",
  list_errors: "errors",
  d1_databases: "d1",
  d1_tables: "d1",
  d1_schema: "d1",
  d1_rows: "d1",
  d1_write: "d1",
  d1_delete_rows: "d1",
  r2_list_buckets: "r2",
  r2_list: "r2",
  r2_file: "r2",
  r2_delete: "r2",
  r2_upload: "r2",
  do_list_classes: "do",
  do_list_instances: "do",
  do_probe: "do",
  code_list_files: "code",
  code_read_file: "code",
  code_compare: "code",
  moderation_queue: "admin",
  admin_stats: "admin",
  admin_review: "admin",
  admin_suspend_app: "admin",
  admin_restore_app: "admin",
  admin_ban_account: "admin",
};

export function actionView(action: AnyAction): ViewId {
  return ACTION_VIEWS[action.name] ?? "overview";
}

/** After navigating, focus the field the action needs. Polled: the view renders after its data loads. */
function focusWhenReady(selectors: string[], clickText?: string): void {
  let tries = 0;
  const tick = (): void => {
    tries += 1;
    if (clickText && !document.querySelector('[role="dialog"][aria-label="Change name"]')) {
      const buttons = Array.from(document.querySelectorAll("button"));
      const opener = buttons.find((b) => b.textContent?.includes(clickText));
      opener?.click();
    }
    for (const select of selectors) {
      const element = document.querySelector(select);
      if (element instanceof HTMLElement) {
        element.focus();
        return;
      }
    }
    if (tries < 25) {
      window.setTimeout(tick, 100);
    }
  };
  window.setTimeout(tick, 50);
}

const FOCUS: Record<string, { selectors: string[]; clickText?: string }> = {
  set_secret: {
    selectors: ['section[aria-label="Add secret"] input[placeholder="API_KEY"]'],
  },
  rename_app: {
    selectors: ['[role="dialog"][aria-label="Change name"] input'],
    clickText: "Change name",
  },
  add_domain: {
    selectors: ['section[aria-label="Custom domain"] input[placeholder="shop.example.com"]'],
  },
};
/** Substring match on title + keywords; earlier title hits rank first. -1 misses. */
export function matchScore(haystack: string, query: string): number {
  const text = haystack.toLowerCase();
  const tokens = query.toLowerCase().split(/\s+/).filter((t) => t.length > 0);
  if (tokens.length === 0) {
    return 0;
  }
  let score = 0;
  for (const token of tokens) {
    const at = text.indexOf(token);
    if (at === -1) {
      return -1;
    }
    score += at === 0 ? 0 : at;
  }
  return score;
}

export function buildGroups(props: PaletteProps): ItemGroup[] {
  const appItems: PaletteItem[] = props.apps.map((a) => ({
    kind: "app" as const,
    key: `app:${a.slug}`,
    title: a.slug,
    sub: `${a.status}${a.role === "collaborator" ? " · shared" : ""}`,
  }));
  const appViewItems: PaletteItem[] =
    props.slug === null
      ? []
      : [...VIEWS, ...DATA_VIEWS].map((v) => ({
          kind: "view" as const,
          key: `view:${v.id}`,
          title: v.label,
          sub: `Go to ${v.label} in ${props.slug}`,
          view: v.id as ViewId,
        }));
  const accountItems: PaletteItem[] = ACCOUNT_VIEWS.filter(
    (v) => v.id !== "admin" || props.isAdmin,
  ).map((v) => ({
    kind: "view" as const,
    key: `view:${v.id}`,
    title: v.label,
    sub: `Go to ${v.label}`,
    view: v.id as ViewId,
  }));
  const actionItems: PaletteItem[] = allActions
    .filter((a) => !EXCLUDED_FROM_PALETTE[a.name] && (!a.adminOnly || props.isAdmin))
    .map((action) => {
      const runnable = isRunnable(action);
      return {
        kind: "action" as const,
        key: `action:${action.name}`,
        title: action.title,
        sub: runnable
          ? `Run on ${props.slug ?? "the current app"}`
          : `Open ${actionView(action)}`,
        action,
        runnable,
      };
    });
  return [
    { title: "Apps", items: appItems },
    { title: "Views", items: appViewItems },
    { title: "Account", items: accountItems },
    { title: "Actions", items: actionItems },
  ];
}

const RECENT_KEY = "outer-palette-recent";
const MAX_RECENT = 5;

export interface RecentEntry {
  kind: string;
  key: string;
}

export function loadRecents(): RecentEntry[] {
  try {
    const raw = window.localStorage.getItem(RECENT_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    if (!Array.isArray(parsed)) {
      return [];
    }
    return (parsed as RecentEntry[]).filter(
      (e) => e && typeof e.kind === "string" && typeof e.key === "string",
    );
  } catch {
    return [];
  }
}

export function saveRecent(entry: RecentEntry): void {
  const rest = loadRecents().filter((e) => e.key !== entry.key);
  try {
    window.localStorage.setItem(
      RECENT_KEY,
      JSON.stringify([entry, ...rest].slice(0, MAX_RECENT)),
    );
  } catch {
    // Private mode: recents just don't persist.
  }
}

function summarize(value: unknown): string {
  const text =
    typeof value === "string" ? value : JSON.stringify(value, null, 2) ?? "";
  return text.length > 2000 ? `${text.slice(0, 2000)}\n…(truncated)` : text;
}

/** Empty-query listing: Recents first (when any), then every group in group
 * order with nothing hidden or re-scored. Recent items leave their own
 * groups so keys stay unique. */
export function emptyQueryEntries(
  groups: ItemGroup[],
  recents: RecentEntry[],
): { group: string; item: PaletteItem }[] {
  const all = groups.flatMap((g) =>
    g.items.map((item) => ({ group: g.title, item })),
  );
  if (recents.length === 0) {
    return all;
  }
  const byKey = new Map(all.map((entry) => [entry.item.key, entry] as const));
  const seen = new Set<string>();
  const recentEntries: { group: string; item: PaletteItem }[] = [];
  for (const recent of recents) {
    const entry = byKey.get(recent.key);
    if (entry !== undefined && !seen.has(recent.key)) {
      seen.add(recent.key);
      recentEntries.push({ group: "Recents", item: entry.item });
    }
  }
  if (recentEntries.length === 0) {
    return all;
  }
  return [
    ...recentEntries,
    ...all.filter((entry) => !seen.has(entry.item.key)),
  ];
}

/** Search across groups: every group's hits stay together in group
 * order, ranked by score within the group, so headers never repeat. */
export function searchEntries(
  groups: ItemGroup[],
  q: string,
): { group: string; item: PaletteItem }[] {
  const order = new Map(groups.map((g, i) => [g.title, i] as const));
  const scored = groups.flatMap((g) =>
    g.items.map((item) => ({ group: g.title, item })),
  );
  return scored
    .map((entry) => ({
      entry,
      score: matchScore(
        `${entry.item.title} ${entry.item.key} ${entry.item.sub}`,
        q,
      ),
    }))
    .filter((s) => s.score >= 0)
    .sort(
      (a, b) =>
        (order.get(a.entry.group) ?? 0) - (order.get(b.entry.group) ?? 0) ||
        a.score - b.score,
    )
    .map((s) => s.entry);
}

export function Palette(props: PaletteProps) {
  const query = atom("");
  const active = atom(0);
  const busy = atom(false);
  const result = atom<string | null>(null);
  const failure = atom<string | null>(null);

  const flat = (): { group: string; item: PaletteItem }[] => {
    const groups = buildGroups(props);
    const q = query().trim();
    if (q === "") {
      return emptyQueryEntries(groups, loadRecents());
    }
    return searchEntries(groups, q);
  };
  const entries = (): { group: string; item: PaletteItem }[] => {
    const list = flat();
    const index = active();
    if (index >= list.length) {
      active.set(Math.max(0, list.length - 1));
    }
    return list;
  };

  function scrollActiveIntoView(): void {
    window.requestAnimationFrame(() => {
      document
        .getElementById(`palette-item-${active()}`)
        ?.scrollIntoView({ block: "nearest" });
    });
  }

  async function run(item: PaletteItem): Promise<void> {
    failure.set(null);
    if (item.kind === "app") {
      const slug = item.key.slice("app:".length);
      saveRecent({ kind: "app", key: item.key });
      props.onNavigate({ app: slug });
      props.onClose();
      return;
    }
    if (item.kind === "view") {
      saveRecent({ kind: "view", key: item.key });
      props.onNavigate({ view: item.view });
      props.onClose();
      return;
    }
    saveRecent({ kind: "action", key: item.key });
    if (!item.runnable) {
      const target = actionView(item.action);
      props.onNavigate({ view: target });
      props.onClose();
      const focus = FOCUS[item.action.name];
      if (focus) {
        focusWhenReady(focus.selectors, focus.clickText);
      }
      return;
    }
    if (busy()) {
      return;
    }
    busy.set(true);
    try {
      const out = await executeAction(item.action, {}, props.execCtx);
      // Read-only runs change nothing, so no refresh: the result stays
      // on screen. Anything else that isn't consequential (triage, key
      // creation) also shows its result — the views reload on visit.
      // Consequential runs refresh the console behind the palette, then
      // close it so the change is what the user sees.
      result.set(summarize(out));
      if (item.action.consequential) {
        props.onMutated();
        props.onClose();
      }
    } catch (err: unknown) {
      failure.set(err instanceof Error ? err.message : String(err));
    } finally {
      busy.set(false);
    }
  }

  const list = entries();
  const inputId = "palette-input";

  // Autofocus the input when the modal mounts.
  window.requestAnimationFrame(() => {
    document.getElementById(inputId)?.focus();
  });

  let lastGroup = "";
  return (
    <div
      class="modal modal-open"
      role="dialog"
      aria-modal="true"
      aria-label="Command palette"
    >
      <div class="modal-box max-h-[70dvh] flex min-w-0 flex-col p-0">
        <div class="min-w-0 p-4 pb-2">
          <input
            id={inputId}
            type="text"
            class="input input-bordered w-full min-w-0"
            placeholder="Search apps, views, actions…"
            role="combobox"
            aria-expanded="true"
            aria-controls="palette-list"
            aria-activedescendant={`palette-item-${active()}`}
            aria-label="Search apps, views and actions"
            value={query()}
            disabled={busy()}
            oninput={(e: Event) => {
              query.set((e.target as HTMLInputElement).value);
              active.set(0);
              result.set(null);
            }}
            onkeydown={(e: KeyboardEvent) => {
              const current = entries();
              if (e.key === "ArrowDown") {
                e.preventDefault();
                active.set(
                  current.length === 0
                    ? 0
                    : (active() + 1) % current.length,
                );
                scrollActiveIntoView();
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                active.set(
                  current.length === 0
                    ? 0
                    : (active() - 1 + current.length) % current.length,
                );
                scrollActiveIntoView();
              } else if (e.key === "Enter") {
                e.preventDefault();
                const item = current[active()]?.item;
                if (item) {
                  void run(item);
                }
              } else if (e.key === "Escape") {
                e.preventDefault();
                props.onClose();
              }
            }}
          />
        </div>
        <div class="min-w-0 flex-1 overflow-y-auto px-2 pb-2">
          {list.length === 0 && (
            <p class="min-w-0 px-2 py-4 text-sm opacity-60" role="status">
              {query().trim() === ""
                ? "Apps, views and runnable actions show up here."
                : `No match for "${query().trim()}".`}
            </p>
          )}
          <ul class="menu w-full min-w-0" id="palette-list" role="listbox" aria-label="Results">
            {list.map((entry, i) => {
              const header =
                entry.group !== lastGroup ? entry.group : null;
              lastGroup = entry.group;
              const selected = i === active();
              return (
                <li
                  key={entry.item.key}
                  role="option"
                  id={`palette-item-${i}`}
                  aria-selected={selected}
                  class="min-w-0"
                >
                  {header && (
                    <h2 class="menu-title text-xs font-medium uppercase tracking-wider">
                      {header}
                    </h2>
                  )}
                  <button
                    type="button"
                    tabindex={-1}
                    class={`min-w-0 text-left ${selected ? "menu-active" : ""}`}
                    onclick={() => void run(entry.item)}
                    onmousemove={() => {
                      if (active() !== i) {
                        active.set(i);
                      }
                    }}
                  >
                    <span class="min-w-0 flex-1 truncate" title={entry.item.title}>
                      {entry.item.title}
                    </span>
                    <span class="min-w-0 max-w-[45%] truncate text-xs opacity-60" title={entry.item.sub}>
                      {entry.item.kind === "action" && !entry.item.runnable
                        ? `→ ${entry.item.sub}`
                        : entry.item.sub}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
          {failure() && (
            <div class="alert alert-error alert-soft m-2 min-w-0" role="alert">
              <span class="min-w-0 break-words">{failure()}</span>
            </div>
          )}
          {result() && (
            <pre class="mockup-code m-2 min-w-0 overflow-x-auto text-xs" role="status">
              {result()}
            </pre>
          )}
        </div>
        <button
          type="button"
          class="modal-backdrop"
          aria-label="Close"
          onclick={props.onClose}
        >
          Close
        </button>
      </div>
    </div>
  );
}
