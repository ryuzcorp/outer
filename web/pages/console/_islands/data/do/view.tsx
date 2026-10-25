// The Durable Object viewer: class picker, instance search, instance list,
// and a detail panel that renders the object's own `?read=1` response.
// Ported from Noite apps/noite/src/lib/storage/do/panel.tsx @ 58a977a.
// Outer differences: fetches go through the registered data-do actions,
// the probe runs on selection (not embedded per row — a class with many
// instances lists fast while the fleet still answers each probe
// best-effort), and the empty copy states that celld has no generic DO
// storage API: apps opt in to previews by implementing `?read=1`.

import { atom, watch } from "ilha";

import {
  doActions,
  type DoBinding,
  type DoInstance,
} from "../../actions/data-do.ts";
import { CopyButton } from "../../common.tsx";
import { DatabaseIcon, RefreshIcon, SearchIcon } from "../../icons.tsx";
import {
  DataBreadcrumb,
  DataEmptyState,
  DetailPanel,
  errorMessage,
  prettyJson,
} from "../shared.tsx";

/** One preview row per top-level JSON-object key; `null` when the response
 * is not a JSON object (a string, an array, a scalar or not JSON at all). */
function previewEntries(preview: string): { key: string; value: string }[] | null {
  let raw: unknown;
  try {
    raw = JSON.parse(preview);
  } catch {
    return null;
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return null;
  }
  return Object.entries(raw as Record<string, unknown>).map(([key, value]) => ({
    key,
    value: JSON.stringify(value, null, 2) ?? String(value),
  }));
}

/** The one-line state summary the list shows for a probed instance. */
function summarizePreview(preview: string | null): string {
  if (preview === null) {
    return "unavailable";
  }

  const entries = previewEntries(preview);

  if (entries === null) {
    return preview.replaceAll(/\s+/gu, " ").trim().slice(0, 80);
  }

  if (entries.length === 0) {
    return "{} (empty)";
  }

  return entries
    .map((entry) => `${entry.key}=${entry.value.replaceAll(/\s+/gu, " ")}`)
    .join(" · ")
    .slice(0, 80);
}

/** The selected instance's storage preview: a key/value table for a JSON
 * object, the pretty body for anything else, and an explanation when the
 * probe came back empty. */
function DoInstancePanel({
  instance,
  preview,
}: {
  instance: DoInstance;
  preview: string | null;
}) {
  const entries = preview === null ? null : previewEntries(preview);

  let body: unknown;

  if (preview === null) {
    body = (
      <p class="m-0 text-sm opacity-60">
        Preview unavailable — the instance is not running, or the object has
        no <code>?read=1</code> handler. Durable Object storage has no generic
        read API: the app opts in to previews by answering that route.
      </p>
    );
  } else if (entries === null) {
    body = (
      <>
        <div class="flex justify-end">
          <CopyButton label="Copy the instance preview" value={preview} />
        </div>
        <pre class="bg-base-200 dark:bg-base-300/50 m-0 max-h-[32rem] overflow-auto rounded-lg p-2 text-xs whitespace-pre-wrap">
          {prettyJson(preview)}
        </pre>
      </>
    );
  } else {
    body = (
      <div class="min-w-0 overflow-x-auto">
        <table class="table-xs table">
          <thead>
            <tr>
              <th>Key</th>
              <th>Value</th>
              <th class="w-20" />
            </tr>
          </thead>
          <tbody>
            {entries.map((entry) => (
              <tr key={entry.key}>
                <td class="align-top font-mono text-xs whitespace-nowrap">
                  {entry.key}
                </td>
                <td class="align-top">
                  <pre class="m-0 max-h-64 overflow-auto text-xs whitespace-pre-wrap">
                    {entry.value}
                  </pre>
                </td>
                <td class="text-right align-top">
                  <CopyButton label={`Copy ${entry.key}`} value={entry.value} />
                </td>
              </tr>
            ))}
            {entries.length === 0 ? (
              <tr>
                <td class="opacity-60" colspan={3}>
                  The object reports an empty state.
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    );
  }

  return (
    <DetailPanel title={instance.id}>
      <p class="m-0 flex min-w-0 items-center gap-2 text-xs opacity-60">
        <span class="badge badge-ghost badge-sm shrink-0">Scope</span>
        <span class="truncate font-mono" title={instance.scope}>
          {instance.scope}
        </span>
      </p>
      {body}
      <p class="m-0 mt-auto text-xs opacity-60">
        Read-only. celld keeps an instance's storage in its cell; this preview
        is the app's own <code>?read=1</code> response.
      </p>
    </DetailPanel>
  );
}

/** One Durable Object class: instance search, the list, and the preview
 * panel for the selected instance. */
function DoBrowser({ slug, binding }: { slug: string; binding: DoBinding }) {
  const instances = atom<DoInstance[] | null>(null);
  const loadError = atom("");
  const query = atom("");
  const selected = atom<string | null>(null);
  const preview = atom<string | null>(null);
  const probing = atom(false);
  // Fetch once on mount (watch.once, never from render: island bodies
  // re-run on every atom update, so a render-driven fetch loops forever).

  const load = async (): Promise<void> => {
    loadError.set("");
    try {
      const res = await doActions.listInstances.run({
        slug,
        class: binding.class,
      });
      instances.set(res.instances);
    } catch (error) {
      loadError.set(errorMessage(error));
    }
  };

  watch.once(() => {
    if (typeof window === "undefined") {
      return;
    }
    void load();
  });

  const select = (id: string | null) => {
    selected.set(id);
    preview.set(null);
    if (id === null) {
      return;
    }
    const found = (instances() ?? []).find((instance) => instance.id === id);
    if (!found) {
      return;
    }
    probing.set(true);
    void (async () => {
      try {
        const res = await doActions.probe.run({ slug, scope: found.scope });
        if (selected() === id) {
          preview.set(res.preview);
        }
      } catch {
        // Best-effort: a failed probe is a blank preview, like a null one.
        if (selected() === id) {
          preview.set(null);
        }
      } finally {
        probing.set(false);
      }
    })();
  };

  if (loadError() && instances() === null) {
    return <p class="text-error m-0 p-4 text-sm">{loadError()}</p>;
  }

  if (instances() === null) {
    return (
      <div class="flex flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  const needle = query().trim().toLowerCase();
  const all = instances()!;
  const matches =
    needle === ""
      ? all
      : all.filter(
          (instance) =>
            instance.id.toLowerCase().includes(needle) ||
            instance.scope.toLowerCase().includes(needle),
        );

  const selectedInstance = all.find((instance) => instance.id === selected()) ?? null;

  let body: unknown;

  if (all.length === 0) {
    body = (
      <DataEmptyState
        hint="An instance is created the first time the object is called. There is no generic storage API to browse — previews come from the app's own ?read=1 handler."
        title="No instances yet"
      />
    );
  } else if (matches.length === 0) {
    body = (
      <DataEmptyState
        hint="No instance id or scope matches this search."
        title="No matches"
      />
    );
  } else {
    body = (
      <div class="min-h-0 flex-1 overflow-auto">
        <table class="table-sm table-pin-rows table w-full">
          <thead>
            <tr>
              <th class="whitespace-nowrap">Instance ID</th>
              <th class="whitespace-nowrap">Scope</th>
              <th class="whitespace-nowrap">State</th>
            </tr>
          </thead>
          <tbody>
            {matches.map((instance) => (
              <tr
                key={instance.id}
                class={`hover cursor-pointer ${selected() === instance.id ? "bg-base-200 dark:bg-base-300/50" : ""}`}
                onclick={() => {
                  select(instance.id);
                }}
              >
                <td class="max-w-52 truncate font-mono text-xs" title={instance.id}>
                  {instance.id}
                </td>
                <td class="max-w-52 truncate font-mono text-xs opacity-80" title={instance.scope}>
                  {instance.scope}
                </td>
                <td class="max-w-52 truncate text-xs opacity-60">
                  {selected() === instance.id && probing()
                    ? "probing…"
                    : summarizePreview(
                        selected() === instance.id ? preview() : null,
                      )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  }

  return (
    <div class="flex h-full min-h-0 w-full flex-col overflow-hidden">
      <div class="border-base-300 flex flex-wrap items-center gap-x-3 gap-y-1 border-b px-3 py-2">
        <DataBreadcrumb
          crumbs={[
            { label: "Durable Objects" },
            { label: binding.name },
          ]}
        />
        <span class="badge badge-sm ml-auto shrink-0" title={`Class ${binding.class}`}>
          DO
        </span>
      </div>
      <div class="border-base-300 flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <label class="input input-sm w-64 max-w-full">
          <SearchIcon />
          <input
            type="search"
            aria-label="Search instances"
            placeholder="Search instances…"
            value={query()}
            oninput={(event) => {
              query.set(event.currentTarget.value);
            }}
          />
        </label>
        <button
          type="button"
          class="btn btn-square btn-ghost btn-sm"
          aria-label="Refresh instances"
          title="Refresh"
          onclick={() => {
            void load();
          }}
        >
          <RefreshIcon />
        </button>
        <span class="badge badge-ghost badge-sm ml-auto">Read-only</span>
      </div>
      <div class="flex min-h-0 flex-1 flex-col lg:flex-row">
        <div class="flex min-h-0 min-w-0 flex-1 flex-col">
          {body}
          <div class="border-base-300 border-t px-3 py-2 text-sm opacity-60">
            {all.length} instance{all.length === 1 ? "" : "s"}
          </div>
        </div>
        {selectedInstance === null ? (
          <DetailPanel title="Details">
            <DataEmptyState
              hint="Select an instance to read the state its handler reports."
              title="Nothing selected"
            />
          </DetailPanel>
        ) : (
          <DoInstancePanel
            key={selectedInstance.id}
            instance={selectedInstance}
            preview={preview()}
          />
        )}
      </div>
      <p class="border-base-300 m-0 border-t px-3 py-2 text-xs opacity-60">
        Durable Object storage has no generic read API in celld. This viewer
        lists instances and shows each app's own <code>?read=1</code>{" "}
        response — implement that read-only branch to preview state.
      </p>
    </div>
  );
}

/** The DO entry: class picker, then the browser. Apps that declare no
 * bindings get the empty state with the docs link. */
export function DoView({ slug }: { slug: string }) {
  const classes = atom<DoBinding[] | null>(null);
  const loadError = atom("");
  const picked = atom<string | null>(null);
  // Fetch once on mount (watch.once, never from render: island bodies
  // re-run on every atom update, so a render-driven fetch loops forever).

  const load = async (): Promise<void> => {
    loadError.set("");
    try {
      const res = await doActions.listClasses.run({ slug });
      classes.set(res.classes);
    } catch (error) {
      loadError.set(errorMessage(error));
    }
  };

  watch.once(() => {
    if (typeof window === "undefined") {
      return;
    }
    void load();
  });

  if (loadError() && classes() === null) {
    return <p class="text-error m-0 p-4 text-sm">{loadError()}</p>;
  }

  if (classes() === null) {
    return (
      <div class="flex flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  if (classes()!.length === 0) {
    return (
      <section class="card card-border bg-base-100 m-4 min-w-0">
        <div class="card-body items-center gap-2 py-8 text-center">
          <span class="bg-base-200 flex h-11 w-11 items-center justify-center rounded-full opacity-70" aria-hidden="true">
            <DatabaseIcon />
          </span>
          <h3 class="m-0 text-base font-semibold">No Durable Objects yet</h3>
          <p class="m-0 max-w-md text-sm opacity-60">
            Declare a <code>durable_objects</code> binding in{" "}
            <code>void.config.ts</code> and deploy. This view lists each
            class's instances and previews whatever the app's{" "}
            <code>?read=1</code> handler returns.
          </p>
          <a class="btn btn-sm btn-neutral" href="/guides/storage">
            Read the storage guide
          </a>
        </div>
      </section>
    );
  }

  const current = classes()!.find((b) => b.class === picked()) ?? classes()![0];

  return (
    <div class="flex h-full min-h-0 flex-col">
      {classes()!.length > 1 ? (
        <div class="border-base-300 flex flex-wrap items-center gap-2 border-b px-3 py-2" role="group" aria-label="Class">
          {classes()!.map((b) => (
            <button
              key={b.class}
              type="button"
              class={`btn btn-sm ${b.class === current.class ? "btn-active" : "btn-ghost"}`}
              title={`Binding ${b.name}`}
              onclick={() => {
                picked.set(b.class);
              }}
            >
              {b.class}
            </button>
          ))}
        </div>
      ) : null}
      <div class="flex min-h-0 flex-1 flex-col" key={current.class}>
        <DoBrowser slug={slug} binding={current} />
      </div>
    </div>
  );
}
