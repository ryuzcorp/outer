// WebMCP exposure for the console action registry (spec §13.1): every
// registered action becomes a `document.modelContext.registerTool` tool
// with matching annotations, so an in-browser sidebar agent operates the
// console with the signed-in user's session and the same server checks.
//
// Rules, per the brief:
// - Feature detection: register only when `modelContext.registerTool`
//   exists. Prefer `document.modelContext`; fall back to
//   `navigator.modelContext` only when that is the only one present
//   (older origin-trial builds).
// - Lifecycle: one AbortController for the whole set; re-register when the
//   signed-in identity changes; abort everything on sign-out and unload.
// - Context: actions taking `slug` (or the collaborators' `app`) default
//   to the current app when omitted; the schema doesn't require it then.
// - Consequential actions confirm through the same daisyUI modal the UI
//   uses. `execute` waits for Confirm/Cancel; Cancel rejects, and abort
//   closes the modal. Never skipped, whatever the agent claims.
// - Errors reject with an Error carrying the server's `user_message`.
// - Untrusted results pass through as-is, flagged via the annotation.
import { allActions, type AnyAction } from "./actions/index.ts";

export interface WebMcpAnnotations {
  readOnlyHint: boolean;
  consequentialHint: boolean;
  untrustedContentHint: boolean;
}

export function toAnnotations(action: AnyAction): WebMcpAnnotations {
  return {
    readOnlyHint: action.readOnly,
    consequentialHint: action.consequential,
    untrustedContentHint: action.untrusted,
  };
}

/** Actions whose only context key is the app: `slug`, or `app` for collaborators. */
function contextKeys(action: AnyAction): string[] {
  const props = schemaProperties(action);
  return ["slug", "app"].filter((key) => Object.hasOwn(props, key));
}

function schemaProperties(action: AnyAction): Record<string, unknown> {
  const schema = action.inputSchema as Record<string, unknown>;
  if (!schema || typeof schema !== "object" || Array.isArray(schema)) {
    return {};
  }
  const props = schema["properties"];
  if (!props || typeof props !== "object" || Array.isArray(props)) {
    return {};
  }
  return props as Record<string, unknown>;
}

/**
 * Fill the app context key from the current app when the caller omitted
 * it. Pure, so the palette and the unit tests share it with WebMCP.
 */
export function applyDefaults(
  action: AnyAction,
  rawInput: unknown,
  slug: string | null,
): Record<string, unknown> {
  const input: Record<string, unknown> =
    rawInput && typeof rawInput === "object" && !Array.isArray(rawInput)
      ? { ...(rawInput as Record<string, unknown>) }
      : {};
  if (slug === null) {
    return input;
  }
  for (const key of contextKeys(action)) {
    if (input[key] === undefined || input[key] === null || input[key] === "") {
      input[key] = slug;
    }
  }
  return input;
}

/**
 * The schema as presented to WebMCP: a copy whose app context keys are
 * no longer required (they default), so agents omit them.
 */
export function webMcpSchema(action: AnyAction): Record<string, unknown> {
  const schema = action.inputSchema as Record<string, unknown>;
  const copy: Record<string, unknown> = JSON.parse(JSON.stringify(schema ?? {}));
  const required = copy["required"];
  const keys = contextKeys(action);
  if (Array.isArray(required) && keys.length > 0) {
    const rest = required.filter((name) => !keys.includes(name as string));
    if (rest.length === 0) {
      delete copy["required"];
    } else {
      copy["required"] = rest;
    }
  }
  return copy;
}

/** The description as presented to WebMCP: notes the current-app default. */
export function webMcpDescription(action: AnyAction): string {
  if (contextKeys(action).length === 0) {
    return action.description;
  }
  return `${action.description} The app defaults to the current one — omit it to act on what is open.`;
}

function asModelContext(value: unknown): ModelContextLike | null {
  if (value === null || typeof value !== "object") {
    return null;
  }
  if (!("modelContext" in value)) {
    return null;
  }
  const candidate: unknown = value.modelContext;
  if (candidate === null || typeof candidate !== "object") {
    return null;
  }
  if (!("registerTool" in candidate) || typeof candidate.registerTool !== "function") {
    return null;
  }
  // Checked: a modelContext object with a callable registerTool. The
  // call signature is the tool's own contract; misuse surfaces there.
  return candidate as ModelContextLike;
}

function detectModelContext(): ModelContextLike | null {
  const scope: unknown = globalThis;
  if (scope === null || typeof scope !== "object") {
    return null;
  }
  if ("document" in scope) {
    const found = asModelContext(scope.document);
    if (found) {
      return found;
    }
  }
  // Older origin-trial builds only: navigator.modelContext alone.
  if ("navigator" in scope) {
    return asModelContext(scope.navigator);
  }
  return null;
}

export type ConfirmFn = (
  action: AnyAction,
  input: Record<string, unknown>,
  signal?: AbortSignal,
) => Promise<void>;

let confirmHandler: ConfirmFn | null = null;

/** The Console island sets the daisyUI-modal confirm handler on mount. */
export function setWebMcpConfirm(fn: ConfirmFn | null): void {
  confirmHandler = fn;
}

function confirmWithAbort(
  action: AnyAction,
  input: Record<string, unknown>,
  signal?: AbortSignal,
): Promise<void> {
  const confirm = confirmHandler;
  if (!confirm) {
    return Promise.reject(
      new Error(
        `Action "${action.name}" is consequential and needs your confirmation in the console — open it and confirm there.`,
      ),
    );
  }
  if (!signal || signal.aborted) {
    if (signal?.aborted) {
      return Promise.reject(abortedError());
    }
    return confirm(action, input, signal);
  }
  return new Promise<void>((resolve, reject) => {
    const onAbort = (): void => {
      reject(abortedError());
    };
    signal.addEventListener("abort", onAbort, { once: true });
    confirm(action, input, signal).then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (err: unknown) => {
        signal.removeEventListener("abort", onAbort);
        reject(err);
      },
    );
  });
}

function abortedError(): Error {
  return new Error("The tool call was aborted.");
}

export interface ExecuteContext {
  getSlug: () => string | null;
}

/**
 * Run one action the WebMCP way: defaults, confirm gate, then `run`.
 * Shared with the ⌘K palette so both surfaces behave identically.
 */
export async function executeAction(
  action: AnyAction,
  rawInput: unknown,
  ctx: ExecuteContext,
  signal?: AbortSignal,
): Promise<unknown> {
  if (signal?.aborted) {
    throw abortedError();
  }
  const input = applyDefaults(action, rawInput, ctx.getSlug());
  if (action.consequential) {
    await confirmWithAbort(action, input, signal);
  }
  try {
    return await action.run(input, signal);
  } catch (err: unknown) {
    // The console `run` helpers already throw the server's
    // `user_message` as the message; pass it through unchanged.
    if (err instanceof Error) {
      throw err;
    }
    throw new Error(String(err));
  }
}

interface ModelContextLike {
  registerTool: (
    tool: {
      name: string;
      title: string;
      description: string;
      inputSchema: object;
      annotations: WebMcpAnnotations;
      execute: (input: unknown, opts?: { signal?: AbortSignal }) => Promise<unknown>;
    },
    opts?: { signal?: AbortSignal },
  ) => unknown;
}

/**
 * Register every console action as a WebMCP tool. Returns a cleanup that
 * aborts the whole set, or null when WebMCP is unavailable (plain
 * browsers, until the origin trial rides along).
 */
export function registerAllWebMcp(ctx: ExecuteContext): (() => void) | null {
  const mc = detectModelContext();
  if (!mc) {
    return null;
  }
  const controller = new AbortController();
  for (const action of allActions) {
    const current = action;
    try {
      const registered = mc.registerTool(
        {
          name: current.name,
          title: current.title,
          description: webMcpDescription(current),
          inputSchema: webMcpSchema(current),
          annotations: toAnnotations(current),
          execute: (input: unknown, opts?: { signal?: AbortSignal }) =>
            executeAction(current, input, ctx, opts?.signal),
        },
        { signal: controller.signal },
      );
      // Some builds return a promise: surface a rejection in the dev
      // console instead of failing the boot.
      if (registered instanceof Promise) {
        registered.catch((err: unknown) => {
          console.error(`[webmcp] register ${current.name} failed:`, err);
        });
      }
    } catch (err: unknown) {
      console.error(`[webmcp] register ${current.name} failed:`, err);
    }
  }
  return () => controller.abort();
}

let syncedKey: string | null = null;
let syncedCleanup: (() => void) | null = null;

/**
 * Re-register when the signed-in identity changes; abort everything on
 * sign-out (`key` null) and when the caller unloads. Returns whether
 * WebMCP is available at all.
 */
export function syncWebMcp(
  key: string | null,
  ctx: ExecuteContext,
): boolean {
  if (key === syncedKey) {
    return syncedCleanup !== null || detectModelContext() !== null;
  }
  if (syncedCleanup) {
    syncedCleanup();
    syncedCleanup = null;
  }
  syncedKey = key;
  if (key === null) {
    return detectModelContext() !== null;
  }
  syncedCleanup = registerAllWebMcp(ctx);
  return syncedCleanup !== null;
}

/** Forget sync state (tests): aborts any live set. */
export function resetWebMcpSync(): void {
  if (syncedCleanup) {
    syncedCleanup();
    syncedCleanup = null;
  }
  syncedKey = null;
}
