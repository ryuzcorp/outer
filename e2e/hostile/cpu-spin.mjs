// CPU-spin probe: a hostile tenant busy-looping on every core. The harness
// (`hostile_cpu_spin_is_capped_and_neighbour_keeps_p95`) or
// `scripts/hostile-probe-vps.sh` runs this inside a cgroup leaf whose
// cpu.max is the build quota (`OUTER_BUILD_CPU_MAX`, default `"200000
// 100000"`): the spinner mirrors a hostile build. The probe spawns one busy
// child per CPU, then measures its own usage from the leaf's `cpu.stat`
// against the quota.
//
// Verdict follows the fail-closed convention for must-fail probes:
// contained (usage within quota plus tolerance) reports `ok: false`; a leak
// (the loop burned more than the quota allows, i.e. the cap did not apply)
// reports `ok: true`, which fails the suite.
//
// Threshold: usage at most 120% of the quota over the window. cgroup v2
// enforces cpu.max per period (100 ms), so over a ~15 s window (150+
// periods) boundary effects are under 2%; spawn/stop skew and cpu.stat
// sampling add a few percent more. 20% headroom absorbs all of that without
// masking a missing cap: uncapped, N busy children burn N CPUs — 4x the
// default two-CPU build quota with 8 children on the 8-core VPS the suite
// runs on. (On a 2-core box an uncapped 2-thread spin would exactly match
// the quota, so the gate is VPS-only by design.)
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { cpus } from "node:os";

const out = {};
const need = (name) => {
  const value = process.env[name];
  if (!value) {
    console.error(`hostile cpu-spin probe needs ${name} (run it through the harness)`);
    process.exit(2);
  }
  return value;
};

// One busy child: loops math past the window end and prints its checksum, so
// the JIT cannot eliminate the loop. `process.execPath -e` runs on bun and
// node alike, and children inherit the leaf, so the quota covers them all.
const CHILD_SRC = `
const end = Date.now() + Number(process.env.HOSTILE_SPIN_CHILD_MS || "15000");
let x = 1;
while (Date.now() < end) { x += Math.sqrt(x + ((Date.now() & 255) + 1)); }
console.log(x);
`;

const stat = (leaf, field) => {
  const line = readFileSync(`${leaf}/cpu.stat`, "utf-8")
    .split("\n")
    .find((l) => l.startsWith(`${field} `));
  if (!line) throw new Error(`cpu.stat has no ${field}`);
  return Number(line.split(/\s+/)[1]);
};

const leaf = need("HOSTILE_CGROUP_LEAF");
const spinMs = Number(process.env.HOSTILE_SPIN_MS || "15000");
const goFile = process.env.HOSTILE_SPIN_GO || "";
// The harness starts spinner and neighbour together: the neighbour takes its
// unloaded baseline first, then the harness drops the GO file and the spin
// starts. Waiting avoids polluting the baseline.
if (goFile) {
  const deadline = Date.now() + 30000;
  while (!existsSync(goFile)) {
    if (Date.now() > deadline) {
      out["exec:cpu-spin"] = { detail: "harness never signalled GO", ok: true };
      const stuck = { phase: "cpu", probes: out };
      mkdirSync("dist", { recursive: true });
      writeFileSync("dist/cpu-spin.json", `${JSON.stringify(stuck, null, 2)}\n`);
      console.log(JSON.stringify(stuck));
      process.exit(0);
    }
    await new Promise((r) => setTimeout(r, 100));
  }
}

const cpuMax = readFileSync(`${leaf}/cpu.max`, "utf-8").trim().split(/\s+/);
const quota = Number(cpuMax[0]);
const period = Number(cpuMax[1]);
// `max <period>` means uncapped: any usage fits, which the harness treats as
// a setup failure (the leaf must carry the quota), not a pass.
try {
  // At most 8 busy children: enough to saturate the quota on the 8-core VPS
  // the suite runs on (uncapped, 8 threads burn 4x the default two-CPU build
  // quota), while bounding runtime footprint so the leaf's memory.max —
  // sized for the experiment, not the assertion — never OOM-kills a spinner
  // and turns a CPU test into a memory one.
  const ncpus = Math.min(cpus().length, 8);
  const usageStart = stat(leaf, "usage_usec");
  const throttledStart = stat(leaf, "nr_throttled");
  const wallStart = Date.now();
  const children = [];
  for (let i = 0; i < ncpus; i++) {
    const child = spawn(process.execPath, ["-e", CHILD_SRC], {
      env: { ...process.env, HOSTILE_SPIN_CHILD_MS: String(spinMs) },
      stdio: ["ignore", "pipe", "pipe"],
    });
    children.push(
      new Promise((resolve) => {
        let output = "";
        child.stdout.on("data", (d) => (output += d));
        child.on("close", (code) => resolve({ code, output: output.trim().slice(-32) }));
      }),
    );
  }
  const results = await Promise.all(children);
  const wallMs = Date.now() - wallStart;
  const usageMs = (stat(leaf, "usage_usec") - usageStart) / 1000;
  const throttled = stat(leaf, "nr_throttled") - throttledStart;
  const dead = results.filter((r) => r.code !== 0).length;
  if (dead > 0) throw new Error(`${dead}/${ncpus} spinner children exited nonzero`);
  if (quota !== quota || period !== period || quota <= 0 || period <= 0) {
    throw new Error(`cpu.max=${cpuMax.join(" ")} carries no quota (uncapped leaf?)`);
  }
  const allowedMs = (wallMs * quota) / period;
  const ratio = usageMs / allowedMs;
  const detail =
    `usage ${usageMs.toFixed(0)}ms of ${allowedMs.toFixed(0)}ms allowed ` +
    `(ratio ${ratio.toFixed(2)}, quota ${quota}/${period}, throttled ${throttled})`;
  // Contained: usage within quota plus tolerance. `ok: false` is the
  // fail-closed convention: a must-fail probe reports false when held.
  out["exec:cpu-spin"] =
    ratio <= 1.2 ? { detail: `capped: ${detail}`, ok: false } : { detail: `ESCAPED: ${detail}`, ok: true };
} catch (error) {
  out["exec:cpu-spin"] = { detail: String(error?.message ?? error).slice(0, 200), ok: true };
}

const report = { phase: "cpu", probes: out };
mkdirSync("dist", { recursive: true });
writeFileSync("dist/cpu-spin.json", `${JSON.stringify(report, null, 2)}\n`);
appendFileSync("dist/cpu-spin.log", `cpu spin ${JSON.stringify(out)}\n`);
console.log(JSON.stringify(report));
