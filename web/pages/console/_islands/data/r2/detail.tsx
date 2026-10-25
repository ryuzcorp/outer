// R2 detail panel: preview plus metadata, download, copy URL and delete.
// Ported from Noite apps/noite/src/lib/storage/r2/{detail,preview}.tsx @
// 58a977a. Outer differences: fetches go through the registered
// data-r2 actions, the delete confirm is the shared daisyUI modal (the
// contract requires a modal; Noite uses an inline two-step), and text
// larger than the preview cap explains itself instead of silently
// truncating.

import { atom, watch } from "ilha";

import {
  r2Actions,
  r2RawUrl,
  type R2Object,
} from "../../actions/data-r2.ts";
import { CopyButton } from "../../common.tsx";
import {
  DownloadIcon,
  TrashIcon,
} from "../../icons.tsx";
import {
  ConfirmModal,
  DataEmptyState,
  DetailPanel,
  errorMessage,
  formatAgo,
  formatBytes,
  formatDateTime,
  prettyJson,
  previewKind,
} from "../shared.tsx";

/** Bounded text/JSON preview of one object (the server sends at most 256 KiB
 * and null for a binary or oversized body). */
function R2TextPreview({
  slug,
  bucket,
  object,
}: {
  slug: string;
  bucket: string;
  object: R2Object;
}) {
  const data = atom<{ text: string | null; truncated: boolean } | null>(null);
  const loadError = atom("");
  // Fetch once on mount (watch.once, never from render: island bodies
  // re-run on every atom update, so a render-driven fetch loops forever).
  watch.once(() => {
    if (typeof window === "undefined") {
      return;
    }
    void (async () => {
      try {
        const file = await r2Actions.file.run({ slug, bucket, key: object.key });
        data.set({ text: file.text, truncated: file.truncated });
      } catch (error) {
        loadError.set(errorMessage(error));
      }
    })();
  });

  if (loadError()) {
    return <p class="text-error m-0 text-sm">{loadError()}</p>;
  }

  if (data() === null) {
    return <span class="skeleton h-24 w-full" />;
  }

  if (data()!.text === null) {
    return (
      <p class="m-0 text-sm opacity-60">
        {data()!.truncated
          ? "This object is larger than the preview cap — download it to inspect the body."
          : "This object is not UTF-8 text, so there is nothing to preview."}
      </p>
    );
  }

  const isJson = (object.contentType ?? "").includes("json");
  const body = isJson ? prettyJson(data()!.text!) : data()!.text!;

  return (
    <div class="flex min-w-0 flex-col gap-1">
      <pre class="bg-base-200 dark:bg-base-300/50 m-0 max-h-64 overflow-auto rounded-lg p-2 text-xs whitespace-pre-wrap">
        {body}
      </pre>
      {data()!.truncated ? (
        <p class="m-0 text-xs opacity-60">
          Preview truncated — download the file for the whole body.
        </p>
      ) : null}
    </div>
  );
}

/** The one object the detail panel shows: preview, record, actions. */
export function R2DetailPanel({
  slug,
  bucket,
  busy,
  canWrite,
  object,
  onDelete,
}: {
  slug: string;
  bucket: string;
  busy: boolean;
  canWrite: boolean;
  object: R2Object;
  onDelete: (key: string) => void;
}) {
  const confirming = atom(false);
  const url = r2RawUrl(slug, bucket, object.key);
  const kind = previewKind(object.contentType);

  return (
    <DetailPanel title={object.name}>
      {kind === "image" ? (
        <div class="border-base-300 flex items-center justify-center rounded-lg border p-2">
          <img
            alt={object.name}
            class="max-h-64 object-contain"
            loading="lazy"
            src={url}
          />
        </div>
      ) : null}
      {kind === "text" ? (
        <R2TextPreview
          key={object.key}
          slug={slug}
          bucket={bucket}
          object={object}
        />
      ) : null}
      {kind === "binary" ? (
        <p class="m-0 text-sm opacity-60">
          No preview for this type — download it to inspect the bytes.
        </p>
      ) : null}
      <dl class="m-0 grid min-w-0 grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        <dt class="opacity-60">Type</dt>
        <dd class="m-0 min-w-0 truncate font-mono text-xs" title={object.contentType ?? undefined}>
          {object.contentType ?? "—"}
        </dd>
        <dt class="opacity-60">Size</dt>
        <dd class="m-0">{formatBytes(object.size)}</dd>
        <dt class="opacity-60">Last modified</dt>
        <dd class="m-0" title={formatDateTime(object.lastModified)}>
          {formatAgo(object.lastModified)}
        </dd>
        <dt class="opacity-60">ETag</dt>
        <dd class="m-0 min-w-0 truncate font-mono text-xs" title={object.etag ?? undefined}>
          {object.etag ?? "—"}
        </dd>
      </dl>
      <div class="mt-auto flex flex-wrap items-center gap-2">
        <a
          class="btn btn-sm btn-neutral gap-1"
          download={object.name}
          href={url}
        >
          <DownloadIcon />
          Download
        </a>
        <CopyButton label={`Copy URL for ${object.name}`} value={url} />
        {canWrite ? (
          <button
            type="button"
            class="btn btn-sm btn-ghost text-error gap-1"
            onclick={() => {
              confirming.set(true);
            }}
          >
            <TrashIcon />
            Delete
          </button>
        ) : null}
      </div>
      {confirming() ? (
        <ConfirmModal
          busy={busy}
          consequence={`“${object.key}” is deleted from R2. This cannot be undone.`}
          onCancel={() => {
            confirming.set(false);
          }}
          onConfirm={() => {
            confirming.set(false);
            onDelete(object.key);
          }}
          title={`Delete ${object.name}?`}
        />
      ) : null}
    </DetailPanel>
  );
}

export function DetailsPlaceholder() {
  return (
    <DetailPanel title="Details">
      <DataEmptyState
        hint="Select a file to preview it, download it or copy its URL."
        title="Nothing selected"
      />
    </DetailPanel>
  );
}
