// Build-time probe: attempts every exfiltration a tenant build script can
// try. Each attempt records { ok: false } when the sandbox held (EACCES, a
// dropped env name, a refused connection, an OOM-killed child) and
// { ok: true, detail } when it leaked. The privileged harness
// (cargo test -- --ignored hostile) runs this through run_sandboxed as an
// app uid in a real cgroup leaf and fails unless every must-fail probe
// stayed contained. Borrowed from Noite's
// apps/noite/test/hostile/build-probe.js: the cgroup positive probe and the
// fail-closed "real targets only" rule come from there; the sibling paths,
// canary and listener targets are Outer's layout (work/, fleets/, outer).
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { spawn, spawnSync } from "node:child_process";

const out = {};
const need = (name) => {
  const value = process.env[name];
  if (!value) {
    console.error(`hostile build probe needs ${name} (run it through the harness)`);
    process.exit(2);
  }
  return value;
};

const deny = (name, fn) => {
  try {
    out[name] = { detail: String(fn()).slice(0, 200), ok: true };
  } catch (error) {
    out[name] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
  }
};

const denyAsync = async (name, fn) => {
  try {
    out[name] = { detail: String(await fn()).slice(0, 200), ok: true };
  } catch (error) {
    out[name] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
  }
};

// App-supplied env with reserved names must never arrive: the harness passes
// OUTER_SECRET and friends as app env, and run_sandboxed must drop them.
for (const key of ["OUTER_SECRET", "STRIPE_SECRET_KEY", "AWS_SECRET_ACCESS_KEY", "CLOUDFLARE_API_TOKEN"]) {
  out[`env:reserved-dropped:${key}`] =
    process.env[key] && process.env[key] !== "unset-by-harness"
      ? { detail: "present", ok: true }
      : { detail: "absent", ok: false };
}
out["env:reserved-dropped"] = Object.entries(out).some(
  ([k, v]) => k.startsWith("env:reserved-dropped:") && v.ok,
)
  ? { detail: "a reserved name leaked through", ok: true }
  : { detail: "reserved names dropped", ok: false };

// /proc/self/environ must not carry platform secret names either.
deny("env:host-environ", () => {
  const raw = readFileSync("/proc/self/environ", "utf-8");
  const hit = ["OUTER_SECRET", "STRIPE_SECRET_KEY", "CLOUDFLARE_API_TOKEN"].filter((k) =>
    raw.includes(`${k}=`)
  );
  if (hit.length > 0) return `leaked ${hit.join(",")}`;
  throw new Error("no platform secrets in environ");
});

// Cross-app reads: the harness points these at real sibling dirs (0700,
// owned by another uid) so a failure proves isolation, not absence.
const siblingWork = need("HOSTILE_SIBLING_WORK");
const siblingData = need("HOSTILE_SIBLING_DATA");
deny("read:sibling-worktree", () => {
  const leaves = readdirSync(siblingWork);
  for (const leaf of leaves.slice(0, 3)) {
    try {
      readFileSync(`${siblingWork}/${leaf}`, "utf-8");
      return `read ${leaf}`;
    } catch {
      // Unreadable leaf: keep looking for a readable one.
    }
  }
  throw new Error(`siblings listed=${leaves.length} readable=0`);
});
deny("read:sibling-data", () => {
  const leaves = readdirSync(siblingData);
  const target = `${siblingData}/${leaves[0]}`;
  return readFileSync(target, "utf-8").slice(0, 80);
});

// Host secrets: a root-only canary stands in for outer.sqlite and host keys.
const canary = need("HOSTILE_CANARY_PATH");
deny("read:host-secrets", () => readFileSync(canary, "utf-8"));

// Writes outside the work dir must all fail. (/tmp is sticky and
// world-writable by design: an app writing its own file there is normal,
// so it is not probed. The sibling and data-root attempts must refuse.)
const marker = `hostile-escape-${process.getuid?.() ?? "noid"}`;
deny("write:outside-workdir", () => {
  const attempts = [
    `${process.cwd()}/../${marker}`,
    `${siblingWork}/${marker}`,
    `${need("HOSTILE_DATA_DIR")}/${marker}`,
  ];
  const succeeded = [];
  for (const target of attempts) {
    try {
      writeFileSync(target, "escape");
      succeeded.push(target);
    } catch {
      // Refused: contained for this path.
    }
  }
  if (succeeded.length > 0) return `wrote ${succeeded.join(", ")}`;
  throw new Error(`${attempts.length} writes refused`);
});

// Fork bomb: a burst of short-lived children. The leaf's pids.max (or
// RLIMIT_NPROC without a leaf) must refuse part of the burst; if every
// spawn succeeds the host has no process bound.
await denyAsync("exec:fork-bomb", async () => {
  const N = 300;
  const kids = [];
  let refused = 0;
  for (let i = 0; i < N; i++) {
    try {
      const kid = spawn(process.execPath, ["-e", "setTimeout(()=>{},300);"], {
        stdio: "ignore",
      });
      // A failed fork throws here; a late failure arrives as 'error'.
      kid.on("error", () => {
        refused++;
      });
      kids.push(kid);
    } catch {
      refused++;
    }
  }
  // Let the burst settle so late 'error' events surface, then reap: none
  // may outlive the probe.
  await new Promise((resolve) => setTimeout(resolve, 1500));
  for (const kid of kids) {
    try {
      kid.kill("SIGKILL");
    } catch {
      // Already gone.
    }
  }
  if (refused > 0) throw new Error(`${refused}/${N} spawns refused`);
  return `${N}/${N} spawns succeeded`;
});

// Memory exhaustion: a child touching 64 MiB pages up to 1.5 GiB. Past a
// 512 MiB leaf it must die (OOM-killed); reporting the full allocation is
// the leak.
await denyAsync("exec:memory-exhaust", async () => {
  const child = spawn(
    process.execPath,
    [
      "-e",
      "const pages=[];for(let i=0;i<24;i++){const b=Buffer.alloc(64*1024*1024);b.fill(i%251);pages.push(b);console.log(`chunk ${i}`);}console.log('allocated=1536MiB');",
    ],
    { stdio: ["ignore", "pipe", "pipe"] }
  );
  let tail = "";
  child.stdout?.on("data", (c) => {
    tail = `${c}`.trim().split("\n").pop() ?? tail;
  });
  const code = await new Promise((resolve) => {
    const timer = setTimeout(() => {
      try {
        child.kill("SIGKILL");
      } catch {
        // Gone already.
      }
      resolve("timeout");
    }, 90000);
    child.on("close", (c, signal) => {
      clearTimeout(timer);
      resolve(signal ?? c);
    });
  });
  if (tail.includes("allocated=1536MiB")) return "child allocated 1536MiB";
  throw new Error(`child stopped early (${code}, last: ${tail || "none"})`);
});

// Positive control: the step really runs inside the leaf the harness made,
// with the limits the harness set. Throws (contained=false is a harness
// failure, not a leak) when cgroups are inactive or the leaf is uncapped.
try {
  const leaf = need("HOSTILE_CGROUP_LEAF");
  const memoryMax = readFileSync(`${leaf}/memory.max`, "utf-8").trim();
  const pidsMax = readFileSync(`${leaf}/pids.max`, "utf-8").trim();
  const self = readFileSync("/proc/self/cgroup", "utf-8").trim();
  const expectMemory = need("HOSTILE_EXPECT_MEMORY_MAX");
  if (memoryMax !== expectMemory) {
    throw new Error(`memory.max=${memoryMax} want ${expectMemory}`);
  }
  if (pidsMax === "max") throw new Error("pids.max uncapped");
  if (!self.includes(leaf.split("/").pop())) {
    throw new Error(`self not in leaf (${self})`);
  }
  out["cgroup:leaf-limited"] = { detail: `memory.max=${memoryMax} pids.max=${pidsMax}`, ok: true };
} catch (error) {
  out["cgroup:leaf-limited"] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
}

// Egress: SMTP ports and blocked ranges must refuse. The loopback target is
// a listener the harness proves serves (from its own uid), so a refusal
// from the sandbox uid proves the nft policy, not an absent server.
const loopbackTarget = need("HOSTILE_LOOPBACK_TARGET");
await denyAsync("fetch:smtp", async () => {
  const results = [];
  for (const port of [25, 465, 587]) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/`, {
        signal: AbortSignal.timeout(2000),
      });
      results.push(`${port}=${res.status}`);
    } catch {
      results.push(`${port}=refused`);
    }
  }
  if (results.every((r) => r.endsWith("refused"))) {
    throw new Error(results.join(" "));
  }
  return results.join(" ");
});
await denyAsync("fetch:blocked-ranges", async () => {
  const targets = [
    "http://169.254.169.254/",
    "http://10.0.0.1/",
    "http://192.168.1.1/",
    "http://100.64.0.1/",
    "http://[fc00::1]/",
    "http://[::1]/",
  ];
  const reached = [];
  for (const target of targets) {
    try {
      const res = await fetch(target, { signal: AbortSignal.timeout(2000) });
      reached.push(`${target}=${res.status}`);
    } catch {
      // Refused or timed out: contained for this target.
    }
  }
  if (reached.length > 0) return reached.join(" ");
  throw new Error(`${targets.length} targets refused`);
});
await denyAsync("fetch:loopback-target", async () => {
  const res = await fetch(loopbackTarget, { signal: AbortSignal.timeout(3000) });
  return `status ${res.status}`;
});

// Positive control: ordinary internet egress still works.
try {
  const res = await fetch("https://example.com/", {
    signal: AbortSignal.timeout(8000),
  });
  if (!res.ok) throw new Error(`status ${res.status}`);
  out["fetch:internet"] = { detail: `status ${res.status}`, ok: true };
} catch (error) {
  out["fetch:internet"] = { detail: String(error?.message ?? error).slice(0, 200), ok: false };
}

const report = { phase: "build", probes: out };
mkdirSync("dist", { recursive: true });
writeFileSync("dist/probe.json", `${JSON.stringify(report, null, 2)}\n`);
appendFileSync("dist/probe.log", `build probes ${JSON.stringify(out)}\n`);
console.log(JSON.stringify(report));
