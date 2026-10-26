// Pure query builder for the admin All Apps directory (no JSX, no ilha,
// so bun can unit-test it like the D1 value rules).
export interface AdminAppsParams {
  q?: string;
  state?: string;
  plan?: string;
  limit?: number;
  before?: string | null;
}

/** Query builder: skips empty filters. */
export function adminAppsQuery(params: AdminAppsParams): string {
  const qs = new URLSearchParams();
  if (params.q !== undefined && params.q !== "") qs.set("q", params.q);
  if (params.state !== undefined && params.state !== "") qs.set("state", params.state);
  if (params.plan !== undefined && params.plan !== "") qs.set("plan", params.plan);
  if (params.limit !== undefined) qs.set("limit", String(params.limit));
  if (params.before !== undefined && params.before !== null) qs.set("before", params.before);
  const text = qs.toString();
  return text === "" ? "" : `?${text}`;
}
