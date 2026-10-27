// R2 credential-scope probe: runs with the app's *scoped* temporary
// credentials and tries to step outside its own prefix. Only runs when the
// harness (or the integrator) provides HOSTILE_S3_* env: endpoint, bucket,
// key, secret, optional session token, own prefix and a sibling prefix.
// Borrowed from Noite's apps/noite/test/hostile/release-probe.js (SigV4
// over node:crypto, fail closed: anything but a 403/401 refusal counts as
// a leak, including a 404 or a transport error).
import { createHash, createHmac } from "node:crypto";

const out = {};
const need = (name) => {
  const value = process.env[name];
  if (!value) {
    console.error(`hostile r2 probe needs ${name}`);
    process.exit(2);
  }
  return value;
};

const endpoint = need("HOSTILE_S3_ENDPOINT").replace(/\/$/, "");
const bucket = need("HOSTILE_S3_BUCKET");
const region = process.env.HOSTILE_S3_REGION ?? "auto";
const access = need("HOSTILE_S3_KEY");
const secret = need("HOSTILE_S3_SECRET");
const token = process.env.HOSTILE_S3_TOKEN ?? "";
const ownPrefix = need("HOSTILE_S3_PREFIX").replace(/^\//, "");
const siblingPrefix = (process.env.HOSTILE_S3_SIBLING ?? "fleets/hostile-neighbour/").replace(/^\//, "");

const sha256hex = (data) => createHash("sha256").update(data).digest("hex");
const hmac = (key, data) => createHmac("sha256", key).update(data).digest();

const s3 = async (method, key, query = "", body = "") => {
  const now = new Date();
  const stamp = now.toISOString().replace(/[-:]/g, "").slice(0, 15) + "Z";
  const date = stamp.slice(0, 8);
  const payloadHash = sha256hex(body);
  const url = new URL(`${endpoint}/${bucket}/${key}${query}`);
  const headers = {
    host: url.host,
    "x-amz-content-sha256": payloadHash,
    "x-amz-date": stamp,
  };
  if (token) headers["x-amz-security-token"] = token;
  const signed = Object.keys(headers).sort();
  const canonical =
    `${method}\n/${bucket}/${key}\n${query.slice(1)}\n` +
    signed.map((k) => `${k}:${headers[k]}\n`).join("") +
    `\n${signed.join(";")}\n${payloadHash}`;
  const scope = `${date}/${region}/s3/aws4_request`;
  const toSign = `AWS4-HMAC-SHA256\n${stamp}\n${scope}\n${sha256hex(canonical)}`;
  const signingKey = hmac(hmac(hmac(hmac(`AWS4${secret}`, date), region), "s3"), "aws4_request");
  const signature = createHmac("sha256", signingKey).update(toSign).digest("hex");
  headers.Authorization =
    `AWS4-HMAC-SHA256 Credential=${access}/${scope}, SignedHeaders=${signed.join(";")}, Signature=${signature}`;
  const res = await fetch(url, {
    method,
    headers,
    body: body ? body : undefined,
    signal: AbortSignal.timeout(15000),
  });
  return res.status;
};

const denied = async (name, method, key, query = "", body = "") => {
  try {
    const status = await s3(method, key, query, body);
    out[name] =
      status === 403 || status === 401
        ? { detail: `status ${status}`, ok: false }
        : { detail: `status ${status}`, ok: true };
  } catch (error) {
    // Fail closed: a transport error proves nothing, so it counts as a leak.
    out[name] = { detail: String(error?.message ?? error).slice(0, 200), ok: true };
  }
};
// Positive control: the scoped key round-trips inside its own prefix.
try {
  const key = `${ownPrefix}hostile-probe.json`;
  const put = await s3("PUT", key, "", JSON.stringify({ probe: true }));
  if (put !== 200) throw new Error(`put status ${put}`);
  const get = await s3("GET", key);
  if (get !== 200) throw new Error(`get status ${get}`);
  const del = await s3("DELETE", key);
  if (del !== 204 && del !== 200) throw new Error(`delete status ${del}`);
  out["r2:own-prefix"] = { detail: "round-trip ok", ok: true };
} catch (error) {
  out["r2:own-prefix"] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
}

await denied("r2:cross-prefix", "GET", `${siblingPrefix}hostile-probe.json`);
await denied("r2:cross-prefix-put", "PUT", `${siblingPrefix}hostile-escape.json`, "", "{}");
await denied("r2:bucket-root", "GET", "", "?list-type=2&prefix=&max-keys=1");

const report = { phase: "release", probes: out };
// The worker serves this file at /probes alongside the build probes.
try {
  const { mkdirSync, writeFileSync } = await import("node:fs");
  mkdirSync("dist", { recursive: true });
  writeFileSync("dist/release-probe.json", `${JSON.stringify(report, null, 2)}\n`);
} catch {
  // Stdout still carries the report for the harness.
}
console.log(JSON.stringify(report));
