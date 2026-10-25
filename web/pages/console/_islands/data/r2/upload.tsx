// R2 writes: the folder modal and the upload button.
// Ported from Noite apps/noite/src/lib/storage/r2/upload.tsx @ 58a977a.
// Outer differences: the upload streams through the console PUT route (same
// guard as the reads), and New folder is a daisyUI modal like the delete
// confirms — Noite's version is a details/popover closed through a live
// ref, which has no equivalent in the console islands.

import { atom } from "ilha";

import { FolderIcon, UploadIcon } from "../../icons.tsx";

/** Create-folder modal: one name, validated here and again by the server
 * (which stores a zero-byte marker object at `<prefix><name>/`). */
export function NewFolderButton({
  busy,
  disabled,
  onCreate,
}: {
  busy: boolean;
  disabled: boolean;
  onCreate: (name: string) => void;
}) {
  const open = atom(false);
  const name = atom("");

  const cleaned = name().trim();

  const invalid =
    cleaned !== "" &&
    (cleaned.includes("/") || cleaned === ".." || cleaned === ".");

  const valid = cleaned !== "" && !invalid;

  const close = () => {
    open.set(false);
    name.set("");
  };

  return (
    <>
      <button
        type="button"
        class="btn btn-sm btn-ghost gap-1"
        aria-label="Create folder"
        title="Create folder"
        disabled={disabled}
        onclick={() => {
          open.set(true);
        }}
      >
        <FolderIcon />
        <span class="hidden sm:inline">New folder</span>
      </button>
      {open() ? (
        <div class="modal modal-open" role="dialog" aria-modal="true" aria-label="Create folder">
          <div class="modal-box min-w-0">
            <h3 class="m-0 text-base font-bold">New folder</h3>
            <label class="flex flex-col gap-1 py-3 text-sm" for="r2-new-folder">
              Folder name
              <input
                id="r2-new-folder"
                class="input input-sm"
                type="text"
                placeholder="photos"
                value={name()}
                oninput={(event) => {
                  name.set(event.currentTarget.value);
                }}
              />
            </label>
            {invalid ? (
              <p class="text-error m-0 text-xs">
                A folder name cannot contain a slash.
              </p>
            ) : null}
            <div class="modal-action">
              <button type="button" class="btn btn-ghost" onclick={close}>
                Cancel
              </button>
              <button
                type="button"
                class="btn btn-neutral"
                disabled={busy || disabled || !valid}
                onclick={() => {
                  if (!valid) {
                    return;
                  }
                  onCreate(cleaned);
                  close();
                }}
              >
                Create
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </>
  );
}

/** Upload button: a hidden multi-file input (uncontrolled — a file input
 * cannot be driven by value) inside its label, so clicking the button
 * opens the picker with no DOM lookup. The input remounts after every
 * pick (`key`), so picking the same file twice fires again. */
export function UploadButton({
  busy,
  disabled,
  onUpload,
}: {
  busy: boolean;
  disabled: boolean;
  onUpload: (files: File[]) => void;
}) {
  const pick = atom(0);

  if (disabled) {
    return (
      <span
        class="btn btn-sm btn-neutral btn-disabled gap-1"
        title="Uploads need the app owner's session"
        aria-disabled="true"
      >
        <UploadIcon />
        Upload
      </span>
    );
  }

  return (
    <label
      class={`btn btn-sm btn-neutral gap-1 ${busy ? "btn-disabled" : ""}`}
      title="Upload files"
      aria-disabled={busy}
    >
      <input
        key={pick()}
        class="hidden"
        type="file"
        multiple
        aria-label="Upload files"
        onchange={(event) => {
          const files = [...(event.currentTarget.files ?? [])];
          pick.set(pick() + 1);
          if (files.length > 0) {
            onUpload(files);
          }
        }}
      />
      <UploadIcon />
      Upload
    </label>
  );
}
