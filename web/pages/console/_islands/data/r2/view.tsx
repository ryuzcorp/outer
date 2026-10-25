// The R2 object browser: folder pages and detail panel for the app's single
// bucket. Ported from Noite apps/noite/src/lib/storage/r2/{panel,browser}.tsx @
// 58a977a. Outer differences: fetches go through the registered data-r2
// actions (components never fetch directly), folder state lives in the
// island instead of the URL (one view per app, not per bucket route), and
// both deletes confirm in the shared daisyUI modal.

import { atom, watch } from "ilha";

import {
  r2Actions,
  type R2Bucket,
  type R2Preview,
} from "../../actions/data-r2.ts";
import { DatabaseIcon } from "../../icons.tsx";
import {
  ConfirmModal,
  DataBreadcrumb,
  DataEmptyState,
  errorMessage,
  Toaster,
  useToasts,
} from "../shared.tsx";
import { DetailsPlaceholder, R2DetailPanel } from "./detail.tsx";
import { R2ColumnRows, R2ListRows } from "./rows.tsx";
import { r2Crumbs, R2_VIEW_LABELS, R2_VIEWS, toggledKey } from "./state.ts";
import type { R2View } from "./state.ts";
import { NewFolderButton, UploadButton } from "./upload.tsx";
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  RefreshIcon,
  SearchIcon,
} from "../../icons.tsx";

/** Largest object the browser sends (the server enforces the same cap). */
const R2_UPLOAD_LIMIT = 67_108_864;

function ViewSwitch({
  onPick,
  view,
}: {
  onPick: (view: R2View) => void;
  view: R2View;
}) {
  return (
    <div class="join" role="group" aria-label="View">
      {R2_VIEWS.map((option) => (
        <button
          key={option}
          type="button"
          class={`btn btn-sm join-item ${view === option ? "btn-active" : ""}`}
          aria-pressed={view === option}
          onclick={() => {
            onPick(option);
          }}
        >
          {R2_VIEW_LABELS[option]}
        </button>
      ))}
    </div>
  );
}

/** One page of a folder: toolbar, listing, pager and the detail panel. The
 * parent keys this by page, so each page fetches once for the cursor it was
 * keyed with. */
function R2Page({
  slug,
  bucket,
  cursor,
  hasPrev,
  notify,
  onNext,
  onPrev,
  onOpenFolder,
  prefix,
  setView,
  view,
}: {
  slug: string;
  bucket: string;
  cursor: string;
  hasPrev: boolean;
  notify: (text: string) => void;
  onNext: (next: string) => void;
  onPrev: () => void;
  onOpenFolder: (prefix: string) => void;
  prefix: string;
  setView: (view: R2View) => void;
  view: R2View;
}) {
  const data = atom<R2Preview | null>(null);
  const loadError = atom("");
  const search = atom("");
  const selected = atom<string | null>(null);
  const checked = atom<Set<string>>(new Set<string>(), { snapshot: false });
  const confirming = atom(false);
  const busy = atom(false);
  const failure = atom("");
  // Fetch once on mount (watch.once, never from render: island bodies
  // re-run on every atom update, so a render-driven fetch loops forever).

  const load = async (): Promise<void> => {
    loadError.set("");
    try {
      const page = await r2Actions.list.run({
        slug,
        bucket,
        prefix,
        cursor: cursor || undefined,
      });
      data.set(page);
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

  if (loadError() && data() === null) {
    return <p class="text-error m-0 p-4 text-sm">{loadError()}</p>;
  }

  if (data() === null) {
    return (
      <div class="flex flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  /** Run a write; `run` returns a problem message (null when it worked),
   * and the listing refreshes either way so a partial result is visible. */
  const write = async (
    run: () => Promise<string | null>,
    done: string,
  ): Promise<void> => {
    if (busy()) {
      return;
    }

    busy.set(true);
    failure.set("");

    try {
      const problem = await run();
      await load();

      if (problem === null) {
        notify(done);
      } else {
        failure.set(problem);
      }
    } catch (error) {
      failure.set(errorMessage(error));
    } finally {
      busy.set(false);
    }
  };

  const upload = (files: File[]) => {
    const uploadable = files.filter((file) => file.size <= R2_UPLOAD_LIMIT);
    const oversized = files.length - uploadable.length;
    void write(
      async () => {
        // One request per object (progress and failures stay per file), in
        // parallel so a multi-file selection is one round trip long.
        const attempts = await Promise.all(
          uploadable.map(async (file) => {
            try {
              await r2Actions.upload.run({
                slug,
                bucket,
                key: `${prefix}${file.name}`,
                file,
                contentType: file.type || "application/octet-stream",
              });
              return null;
            } catch (error) {
              return `${file.name}: ${errorMessage(error)}`;
            }
          }),
        );

        const failures = attempts.filter((message) => message !== null);
        const notes = [...failures];

        if (oversized > 0) {
          notes.unshift(
            `1 file exceeds the upload cap and was skipped.`,
          );
        }

        return notes.length > 0 ? notes.join("\n") : null;
      },
      `${uploadable.length} file${uploadable.length === 1 ? "" : "s"} uploaded`,
    );
  };

  const createFolder = (name: string) => {
    void write(async () => {
      await r2Actions.upload.run({
        slug,
        bucket,
        key: `${prefix}${name}/`,
        file: new Blob([]),
        contentType: "application/octet-stream",
      });
      return null;
    }, `Folder “${name}” created`);
  };

  const runDelete = (keys: string[]) => {
    void write(
      async () => {
        await r2Actions.deleteObjects.run({ slug, bucket, keys });
        checked.set(new Set());
        confirming.set(false);

        if (keys.includes(selected() ?? "")) {
          selected.set(null);
        }

        return null;
      },
      `${keys.length} object${keys.length === 1 ? "" : "s"} deleted`,
    );
  };

  const needle = search().trim().toLowerCase();
  const page = data()!;

  const folders =
    needle === ""
      ? page.folders
      : page.folders.filter((folder) =>
          folder.name.toLowerCase().includes(needle),
        );

  const files =
    needle === ""
      ? page.objects
      : page.objects.filter((object) =>
          object.name.toLowerCase().includes(needle),
        );

  const checkedKeys = [...checked()];
  const selectedObject = page.objects.find((o) => o.key === selected()) ?? null;

  let browser = (
    <DataEmptyState
      action={<UploadButton busy={busy()} disabled={false} onUpload={upload} />}
      hint={
        prefix === ""
          ? "Upload a file to this bucket to get started."
          : "This folder is empty — upload a file into it."
      }
      title="Nothing here yet"
    />
  );

  if (page.folders.length > 0 || page.objects.length > 0) {
    const rows =
      view === "list" ? (
        <R2ListRows
          canWrite
          checked={checked()}
          files={files}
          folders={folders}
          onOpen={(key) => {
            selected.set(key);
          }}
          onOpenFolder={onOpenFolder}
          onToggle={(key, on) => {
            checked.set(toggledKey(checked(), key, on));
          }}
          onToggleAll={(on) => {
            checked.set(
              on ? new Set(files.map((object) => object.key)) : new Set(),
            );
          }}
          selected={selected()}
        />
      ) : (
        <R2ColumnRows
          canWrite
          checked={checked()}
          files={files}
          folders={folders}
          onOpen={(key) => {
            selected.set(key);
          }}
          onOpenFolder={onOpenFolder}
          onToggle={(key, on) => {
            checked.set(toggledKey(checked(), key, on));
          }}
          selected={selected()}
        />
      );
    browser = rows;
  }

  return (
    <div class="flex min-h-0 flex-1 flex-col">
      <div class="border-base-300 flex flex-wrap items-center gap-2 border-b px-3 py-2">
        <label class="input input-sm w-56 max-w-full">
          <SearchIcon />
          <input
            type="search"
            aria-label="Search in this folder"
            placeholder="Search in this folder…"
            value={search()}
            oninput={(event) => {
              search.set(event.currentTarget.value);
            }}
          />
        </label>
        <button
          type="button"
          class="btn btn-square btn-ghost btn-sm"
          aria-label="Refresh"
          title="Refresh"
          onclick={() => {
            void load();
          }}
        >
          <RefreshIcon />
        </button>
        <ViewSwitch onPick={setView} view={view} />
        <div class="ml-auto flex items-center gap-2">
          <NewFolderButton busy={busy()} disabled={false} onCreate={createFolder} />
          <UploadButton busy={busy()} disabled={false} onUpload={upload} />
        </div>
      </div>
      {failure() ? (
        <p class="text-error m-0 px-3 py-1 text-sm whitespace-pre-wrap">
          {failure()}
        </p>
      ) : null}
      {checkedKeys.length > 0 ? (
        <div class="bg-base-200 dark:bg-base-300/50 flex flex-wrap items-center gap-2 px-3 py-1.5 text-sm">
          <span class="font-medium">{checkedKeys.length} selected</span>
          <button
            type="button"
            class="btn btn-sm btn-ghost"
            onclick={() => {
              confirming.set(true);
            }}
          >
            Delete
          </button>
          <button
            type="button"
            class="btn btn-sm btn-ghost ml-auto"
            onclick={() => {
              checked.set(new Set());
              confirming.set(false);
            }}
          >
            Clear
          </button>
        </div>
      ) : null}
      <div class="flex min-h-0 flex-1 flex-col lg:flex-row">
        <div class="flex min-h-0 min-w-0 flex-1 flex-col">
          {browser}
          <div class="border-base-300 flex flex-wrap items-center justify-between gap-2 border-t px-3 py-2 text-sm">
            <span class="opacity-60">
              {page.folders.length} folder{page.folders.length === 1 ? "" : "s"} ·{" "}
              {page.objects.length} file{page.objects.length === 1 ? "" : "s"}
            </span>
            <div class="flex items-center gap-1">
              <button
                type="button"
                class="btn btn-square btn-ghost btn-sm"
                aria-label="Previous page"
                disabled={!hasPrev}
                onclick={onPrev}
              >
                <ChevronLeftIcon />
              </button>
              <button
                type="button"
                class="btn btn-square btn-ghost btn-sm"
                aria-label="Next page"
                disabled={page.nextCursor === null}
                onclick={() => {
                  if (page.nextCursor !== null) {
                    onNext(page.nextCursor);
                  }
                }}
              >
                <ChevronRightIcon />
              </button>
            </div>
          </div>
        </div>
        {selectedObject === null ? (
          <DetailsPlaceholder />
        ) : (
          <R2DetailPanel
            key={selectedObject.key}
            slug={slug}
            bucket={bucket}
            busy={busy()}
            canWrite
            object={selectedObject}
            onDelete={(key) => {
              runDelete([key]);
            }}
          />
        )}
      </div>
      {confirming() ? (
        <ConfirmModal
          busy={busy()}
          consequence={`${checkedKeys.length} object${checkedKeys.length === 1 ? " is" : "s are"} deleted from R2. This cannot be undone.`}
          onCancel={() => {
            confirming.set(false);
          }}
          onConfirm={() => {
            confirming.set(false);
            runDelete(checkedKeys);
          }}
          title={`Delete ${checkedKeys.length} object${checkedKeys.length === 1 ? "" : "s"}?`}
        />
      ) : null}
    </div>
  );
}

/** One bucket folder: the page plus the cursor stack that walks the store
 * (the server returns a continuation token per page). Keyed by prefix in
 * R2Browser, so entering a folder starts at its first page. */
function R2Folder({
  slug,
  bucket,
  notify,
  onOpenFolder,
  prefix,
  setView,
  view,
}: {
  slug: string;
  bucket: string;
  notify: (text: string) => void;
  onOpenFolder: (prefix: string) => void;
  prefix: string;
  setView: (view: R2View) => void;
  view: R2View;
}) {
  const cursors = atom<string[]>([""]);
  const pageIndex = atom(0);
  const at = Math.min(pageIndex(), cursors().length - 1);

  return (
    <R2Page
      key={at}
      slug={slug}
      bucket={bucket}
      cursor={cursors()[at] ?? ""}
      hasPrev={at > 0}
      notify={notify}
      onNext={(next) => {
        cursors.set([...cursors().slice(0, at + 1), next]);
        pageIndex.set(at + 1);
      }}
      onPrev={() => {
        pageIndex.set(Math.max(at - 1, 0));
      }}
      onOpenFolder={onOpenFolder}
      prefix={prefix}
      setView={setView}
      view={view}
    />
  );
}

/** One R2 bucket: breadcrumb header, then the folder view. Owner-only by
 * construction (the server refuses anything else); every session here is
 * the owner's, so writes stay enabled. */
function R2Browser({ slug, bucket }: { slug: string; bucket: R2Bucket }) {
  const prefix = atom("");
  const view = atom<R2View>("list");
  const { notify, toasts } = useToasts();

  return (
    <div class="flex h-full min-h-0 w-full flex-col overflow-hidden">
      <div class="border-base-300 flex flex-wrap items-center gap-x-3 gap-y-1 border-b px-3 py-2">
        <DataBreadcrumb
          crumbs={r2Crumbs(bucket.bucket, prefix(), (next) => {
            prefix.set(next);
          })}
        />
        <span class="badge badge-sm ml-auto shrink-0" title={`Binding ${bucket.binding}`}>
          {bucket.binding}
        </span>
      </div>
      <R2Folder
        key={prefix()}
        slug={slug}
        bucket={bucket.bucket}
        notify={notify}
        onOpenFolder={(next) => {
          prefix.set(next);
        }}
        prefix={prefix()}
        setView={(next) => {
          view.set(next);
        }}
        view={view()}
      />
      <Toaster toasts={toasts()} />
    </div>
  );
}

/** The R2 entry: the single declared bucket, then the browser. Apps that
 * declare no buckets get the empty state with the docs link. */
export function R2View({ slug }: { slug: string }) {
  const buckets = atom<R2Bucket[] | null>(null);
  const loadError = atom("");
  // Fetch once on mount (watch.once, never from render: island bodies
  // re-run on every atom update, so a render-driven fetch loops forever).

  const load = async (): Promise<void> => {
    loadError.set("");
    try {
      const res = await r2Actions.listBuckets.run({ slug });
      buckets.set(res.buckets);
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

  if (loadError() && buckets() === null) {
    return <p class="text-error m-0 p-4 text-sm">{loadError()}</p>;
  }

  if (buckets() === null) {
    return (
      <div class="flex flex-col gap-2 p-4">
        <span class="skeleton h-8 w-full" />
        <span class="skeleton h-6 w-full" />
        <span class="skeleton h-6 w-full" />
      </div>
    );
  }

  if (buckets()!.length === 0) {
    return (
      <section class="card card-border bg-base-100 m-4 min-w-0">
        <div class="card-body items-center gap-2 py-8 text-center">
          <span class="bg-base-200 flex h-11 w-11 items-center justify-center rounded-full opacity-70" aria-hidden="true">
            <DatabaseIcon />
          </span>
          <h3 class="m-0 text-base font-semibold">No R2 buckets yet</h3>
          <p class="m-0 max-w-md text-sm opacity-60">
            Declare an <code>r2_buckets</code> binding in{" "}
            <code>void.config.ts</code> and deploy. Then browse, preview,
            upload and delete the files your app stores there.
          </p>
          <a class="btn btn-sm btn-neutral" href="/guides/storage">
            Read the storage guide
          </a>
        </div>
      </section>
    );
  }

  // At most one bucket per app (deploy validation refuses a second), so
  // the single binding is used directly: no picker, no list.
  return <R2Browser slug={slug} bucket={buckets()![0]} />;
}
