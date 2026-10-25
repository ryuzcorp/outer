// Shared fetch for registered console actions: same-origin JSON, and a
// missing or expired web session sends the human through the Rust sign-in
// flow. Failures throw `ConsoleApiError` whose message prefers the server's
// `user_message`, so components can render it in an alert as-is.
export class ConsoleApiError extends Error {
  status: number;
  code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ConsoleApiError";
    this.status = status;
    this.code = code;
  }
}

export async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  signal?: AbortSignal,
): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: {
      accept: "application/json",
      ...(body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
    credentials: "same-origin",
  });
  if (res.status === 401) {
    globalThis.location.assign("/console/sign-in");
    throw new ConsoleApiError(401, "unauthorized", "signed out");
  }
  const data: unknown = await res.json().catch(() => null);
  const record =
    typeof data === "object" && data !== null
      ? (data as Record<string, unknown>)
      : null;
  if (!res.ok) {
    const code =
      typeof record?.["error"] === "string"
        ? (record["error"] as string)
        : `request failed: ${res.status}`;
    const message =
      typeof record?.["user_message"] === "string"
        ? (record["user_message"] as string)
        : typeof record?.["message"] === "string"
          ? (record["message"] as string)
          : `request failed: ${res.status}`;
    throw new ConsoleApiError(res.status, code, message);
  }
  return data as T;
}
