// R2 credential-scope probe: runs with the app's *scoped* temporary
// credentials and tries to step outside its own grant. Only runs when the
// harness (or the integrator) provides HOSTILE_S3_* env: endpoint, bucket,
// key, secret, optional session token, own prefix, a sibling prefix, a
// read-only deploy key pair, and optionally the writable state prefix
// inside its own grant (`HOSTILE_S3_DATA_PREFIX`, default `<own prefix>cells/`)
// and the deploy prefix/object (`HOSTILE_S3_DEPLOY_PREFIX` default
// `<own prefix>deploy/`, `HOSTILE_S3_DEPLOY_OBJECT` default
// `<deploy prefix>hostile-probe.json`, which the integrator uploads).
//
// Two grants, minted the way outer's store/credentials.rs mints them:
// - the *state* credential (HOSTILE_S3_KEY/SECRET): read-write over the
//   state prefixes and NOT over `deploy/` or `deploy-blobs/`;
// - the *deploy* credential (HOSTILE_S3_DEPLOY_KEY/SECRET): read-only over
//   `deploy/` and `deploy-blobs/` (the node's CELLD_DEPLOY_AWS_* grant).
// So the state positive control round-trips inside a state prefix and a
// write to the deployment pointer must be refused, while the deploy grant
// reads `deploy/` and a write with it is refused too.
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
const dataPrefix = (process.env.HOSTILE_S3_DATA_PREFIX ?? `${ownPrefix}cells/`).replace(/^\//, "");
// The node's read-only deployment grant (`CELLD_DEPLOY_AWS_*`): it must
// read `deploy/` and `deploy-blobs/` but can never publish. The object the
// GET probe reads is uploaded by the integrator with the bucket root keys.
const deployAccess = need("HOSTILE_S3_DEPLOY_KEY");
const deploySecret = need("HOSTILE_S3_DEPLOY_SECRET");
const deployToken = process.env.HOSTILE_S3_DEPLOY_TOKEN ?? "";
const deployPrefix = (process.env.HOSTILE_S3_DEPLOY_PREFIX ?? `${ownPrefix}deploy/`).replace(/^\//, "");
const deployObject = (process.env.HOSTILE_S3_DEPLOY_OBJECT ?? `${deployPrefix}hostile-probe.json`).replace(/^\//, "");
const stateGrant = { access, secret, token };
const deployGrant = { access: deployAccess, secret: deploySecret, token: deployToken };

const sha256hex = (data) => createHash("sha256").update(data).digest("hex");
const hmac = (key, data) => createHmac("sha256", key).update(data).digest();

const s3 = async (method, key, query = "", body = "", grant = stateGrant) => {
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
  if (grant.token) headers["x-amz-security-token"] = grant.token;
  const signed = Object.keys(headers).sort();
  const canonical =
    `${method}\n/${bucket}/${key}\n${query.slice(1)}\n` +
    signed.map((k) => `${k}:${headers[k]}\n`).join("") +
    `\n${signed.join(";")}\n${payloadHash}`;
  const scope = `${date}/${region}/s3/aws4_request`;
  const toSign = `AWS4-HMAC-SHA256\n${stamp}\n${scope}\n${sha256hex(canonical)}`;
  const signingKey = hmac(hmac(hmac(hmac(`AWS4${grant.secret}`, date), region), "s3"), "aws4_request");
  const signature = createHmac("sha256", signingKey).update(toSign).digest("hex");
  headers.Authorization =
    `AWS4-HMAC-SHA256 Credential=${grant.access}/${scope}, SignedHeaders=${signed.join(";")}, Signature=${signature}`;
  const res = await fetch(url, {
    method,
    headers,
    body: body ? body : undefined,
    signal: AbortSignal.timeout(15000),
  });
  return res.status;
};

const denied = async (name, method, key, query = "", body = "", grant = stateGrant) => {
  try {
    const status = await s3(method, key, query, body, grant);
    out[name] =
      status === 403 || status === 401
        ? { detail: `status ${status}`, ok: false }
        : { detail: `status ${status}`, ok: true };
  } catch (error) {
    // Fail closed: a transport error proves nothing, so it counts as a leak.
    out[name] = { detail: String(error?.message ?? error).slice(0, 200), ok: true };
  }
};
// Positive control: the scoped key round-trips inside its own state prefix.
try {
  const key = `${dataPrefix}hostile-probe.json`;
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

// The deployment artifacts are read-only for a fleet: the node's
// `CELLD_DEPLOY_AWS_*` grant must list and get `deploy/` and can never
// publish. The positive read proves the grant is the deploy one, not a
// state key; the refused write is the boundary under test.
try {
  const listQuery = `?list-type=2&max-keys=1&prefix=${encodeURIComponent(deployPrefix)}`;
  const list = await s3("GET", "", listQuery, "", deployGrant);
  if (list !== 200) throw new Error(`list status ${list}`);
  const get = await s3("GET", deployObject, "", "", deployGrant);
  if (get !== 200) throw new Error(`get status ${get}`);
  out["r2:deploy-ro-read"] = { detail: "list+get ok", ok: true };
} catch (error) {
  out["r2:deploy-ro-read"] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
}
await denied("r2:deploy-ro-write", "PUT", deployObject, "", "{}", deployGrant);

// The deployment pointer and asset bodies are read-only for a fleet: a
// credential copied out of a node must not publish a deployment.
await denied("deploy:pointer-rewrite", "PUT", `${ownPrefix}deploy/current.json`, "", "{}");
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
