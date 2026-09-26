// A TAB'S PASS, PER ASSET (js/keep-tab.js; KEEPER.md §12, sdk#479 P3). One named scenario per invariant or cell the
// architect's attack added, then a SEEDED MODEL: random event sequences over the real `step`, with I1-I6 and the
// impossible-cell count checked after every step. Each known-broken version (the §12 list) is a tools/mutant.sh
// mutant, and the test that turns it red is named in its failure.
import assert from "node:assert/strict";
import { slotFree, step, tab } from "../../js/keep-tab.js";

let failures = 0;
const t = async (name, f) => {
  try { await f(); process.stdout.write(`  ok  ${name}\n`); } catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const CAD = 1000;
const world = (bg = 0) => ({ backgroundInFlight: bg, rootOf: tgt => `root-${tgt}`, cadenceMs: CAD });
/** Run events, returning the tab and every effect, in order. */
const run = (t0, events, w = world()) => {
  let cur = t0;
  const effects = [];
  for (const ev of events) {
    const r = step(cur, ev, typeof w === "function" ? w() : w);
    cur = r.tab;
    effects.push(...r.effects);
  }
  return { tab: cur, effects };
};
const kind = (tb, x) => tb.assets.get(x)?.k;
const activeCount = tb => [...tb.assets.values()].filter(s => s.k === "asking" || s.k === "running").length;

await t("**I6: three assets all Due → exactly ONE asks; the others wait queued; each runs in turn**", () => {
  let { tab: tb, effects } = run(tab(["a", "b", "c"], 0), [{ e: "due", now: 0 }]);
  assert.deepEqual(effects, [{ do: "askLock", target: "a" }]);
  assert.deepEqual(["a", "b", "c"].map(x => kind(tb, x)), ["asking", "queued", "queued"]);
  ({ tab: tb, effects } = run(tb, [{ e: "granted", target: "a" }, { e: "ended", target: "a", report: {}, now: 5 }]));
  assert.equal(kind(tb, "a"), "idle");
  assert.equal(kind(tb, "b"), "asking", "the freed slot did not go to the oldest queued asset");
  assert.equal(activeCount(tb), 1);
});

await t("**I6's second half: a cancelled pass's PUT still on the wire keeps the TAB's slot, so B stays queued until it ends**", () => {
  let { tab: tb } = run(tab(["a", "b"], 0), [{ e: "due", now: 0 }, { e: "granted", target: "a" }]);
  assert.equal(kind(tb, "b"), "queued");
  // Cancel A mid-PUT: A's page still has one Background op on the wire.
  ({ tab: tb } = run(tb, [{ e: "cancel", target: "a" }], world(1)));
  assert.equal(kind(tb, "a"), "idle");
  assert.equal(kind(tb, "b"), "queued", "B started while A's PUT held its page's lane: two background ops on the node");
  // The PUT is answered: a poke re-runs the scheduler.
  ({ tab: tb } = run(tb, [{ e: "poke" }], world(0)));
  assert.equal(kind(tb, "b"), "asking");
});

await t("THE CONTROL: with no background op on the wire, a cancel frees the slot at once", () => {
  const { tab: tb } = run(tab(["a", "b"], 0), [{ e: "due", now: 0 }, { e: "granted", target: "a" }, { e: "cancel", target: "a" }]);
  assert.equal(kind(tb, "b"), "asking");
});

await t("**a full pass writes back; an incremental one never does (I4)**", () => {
  let { effects } = run(tab(["a"], 0), [{ e: "due", now: 0 }, { e: "granted", target: "a" }, { e: "ended", target: "a", report: { r: 1 }, now: 1 }]);
  assert.deepEqual(effects.filter(e => e.do === "writeBack"), [{ do: "writeBack", target: "a", report: { r: 1 } }]);
  ({ effects } = run(tab(["a"], 99), [{ e: "head", target: "a", root: "r2" }, { e: "granted", target: "a" }, { e: "ended", target: "a", report: {}, now: 1 }]));
  assert.equal(effects.some(e => e.do === "auditSince"), true, "THE SETUP: no incremental pass ran");
  assert.equal(effects.some(e => e.do === "writeBack"), false, "an incremental pass wrote back");
});

await t("**the cadence never touches a running pass (I3)**; a Due during it changes nothing", () => {
  const { tab: tb, effects } = run(tab(["a"], 0), [{ e: "due", now: 0 }, { e: "granted", target: "a" }, { e: "due", now: 5000 }]);
  assert.equal(kind(tb, "a"), "running");
  assert.equal(effects.filter(e => e.do === "audit").length, 1, "a second pass was started on a running asset");
});

await t("**only a FULL ask's refusal moves the full cadence; an incremental refusal leaves it**", () => {
  let { tab: tb } = run(tab(["a"], 0), [{ e: "due", now: 0 }, { e: "heldElsewhere", target: "a", now: 10 }]);
  assert.equal(tb.assets.get("a").nextFullAt, 10 + CAD);
  ({ tab: tb } = run(tab(["a"], 500), [{ e: "head", target: "a", root: "r" }, { e: "heldElsewhere", target: "a", now: 10 }]));
  assert.equal(tb.assets.get("a").nextFullAt, 500, "an incremental ask's refusal moved the full cadence");
});

await t("**AuditNow upgrades a waiting incremental request to full (never lost)**", () => {
  for (const setup of [[], [{ e: "due", now: -1 }]]) {
    // queued (slot held by another asset) and asking (slot free)
    const base = setup.length ? run(tab(["x", "a"], 0), [{ e: "due", now: 0 }]).tab : tab(["a"], 999);
    const { tab: tb } = run(base, [{ e: "head", target: "a", root: "r" }, { e: "auditNow", target: "a" }]);
    assert.equal(tb.assets.get("a").kind.full, true, `${tb.assets.get("a").k}: the person's full request was dropped`);
  }
});

await t("**a head move during a pass is remembered (the OLDEST root) and runs as an incremental after it**", () => {
  const { tab: tb, effects } = run(tab(["a"], 0), [
    { e: "due", now: 0 }, { e: "granted", target: "a" }, { e: "head", target: "a", root: "r2" }, { e: "head", target: "a", root: "r3" },
    { e: "ended", target: "a", report: {}, now: 1 }, { e: "granted", target: "a" },
  ]);
  assert.deepEqual(effects.find(e => e.do === "auditSince"), { do: "auditSince", target: "a", oldRoot: "root-a" });
  assert.equal(kind(tb, "a"), "running");
});

await t("**Removed**: a running pass is cancelled and released; an asking asset's late answer is counted, never acted on", () => {
  let { tab: tb, effects } = run(tab(["a"], 0), [{ e: "due", now: 0 }, { e: "granted", target: "a" }, { e: "removed", target: "a" }]);
  assert.deepEqual(effects.slice(-2), [{ do: "cancelAudit", target: "a" }, { do: "release", target: "a" }]);
  assert.equal(tb.assets.has("a"), false);
  ({ tab: tb, effects } = run(tab(["a"], 0), [{ e: "due", now: 0 }, { e: "removed", target: "a" }, { e: "granted", target: "a" }]));
  assert.equal(tb.impossible, 1);
  assert.equal(effects.some(e => e.do === "audit"), false, "a late grant started a pass on a removed asset");
});

await t("**ONE WRITER: only `step` changes a tab's asset states** (no other function in js/ writes `.assets`)", async () => {
  const { readFileSync, readdirSync } = await import("node:fs");
  const dir = new URL("../../js/", import.meta.url);
  const writes = /\bassets\.(set|delete|clear)\(/;
  for (const f of readdirSync(dir).filter(n => n.endsWith(".js"))) {
    const src = readFileSync(new URL(f, dir), "utf8");
    if (f !== "keep-tab.js") { assert.ok(!writes.test(src), `${f} writes a tab's asset states`); continue; }
    // Inside keep-tab.js every write is inside step(): the text after `export function step(` holds them all.
    const outside = src.slice(0, src.indexOf("export function step("));
    assert.ok(!writes.test(outside), "keep-tab.js writes asset states outside step()");
    assert.ok(writes.test(src.slice(src.indexOf("export function step("))), "THE CONTROL: step() writes no asset state, so this check reads nothing");
  }
});

// ---- THE MODEL: seeded random sequences, a simulated driver, I1-I6 + the impossible count after every step ----
function rng(seed) {
  let s = seed >>> 0;
  return () => ((s = (s * 1664525 + 1013904223) >>> 0) / 2 ** 32);
}
function model(seed, steps = 400) {
  const r = rng(seed), pick = a => a[Math.floor(r() * a.length)];
  const targets = ["a", "b", "c"];
  let tb = tab(targets, 0), now = 0, bg = 0;
  const locks = new Set(), asks = [], passes = new Set();
  for (let i = 0; i < steps; i++) {
    let ev;
    const roll = r();
    if (asks.length && roll < 0.25) {
      const tgt = asks.shift();
      if (!tb.assets.has(tgt)) { ev = { e: pick(["granted", "heldElsewhere"]), target: tgt, now }; }
      else if (r() < 0.7 && !locks.has(tgt)) { locks.add(tgt); ev = { e: "granted", target: tgt }; }
      else ev = { e: "heldElsewhere", target: tgt, now };
    } else if (passes.size && roll < 0.45) {
      const tgt = pick([...passes]); ev = { e: "ended", target: tgt, report: {}, now };
    } else if (roll < 0.55) { now += Math.floor(r() * 1500); ev = { e: "due", now }; }
    else if (roll < 0.65) ev = { e: "head", target: pick(targets), root: `r${i}` };
    else if (roll < 0.72) ev = { e: "auditNow", target: pick(targets) };
    else if (roll < 0.8) { ev = { e: "cancel", target: pick(targets) }; if (tb.assets.get(ev.target)?.k === "running" && r() < 0.5) bg = 1; }
    else if (roll < 0.85) ev = { e: pick(["removed", "added"]), target: pick(targets), now };
    else { bg = 0; ev = { e: "poke" }; }
    const before = tb;
    const { tab: next, effects } = step(tb, ev, { backgroundInFlight: bg, rootOf: x => `root-${x}`, cadenceMs: CAD });
    for (const e of effects) {
      if (e.do === "askLock") asks.push(e.target);
      if (e.do === "audit" || e.do === "auditSince") passes.add(e.target);
      if (e.do === "cancelAudit") passes.delete(e.target);
      if (e.do === "release") { locks.delete(e.target); passes.delete(e.target); }
      // I4: a write-back only for a pass that ended as FULL.
      if (e.do === "writeBack") assert.equal(before.assets.get(e.target)?.kind?.full, true, `seed ${seed} step ${i}: an incremental pass wrote back`);
    }
    tb = next;
    // I6: at most one asset asking or running.
    assert.ok(activeCount(tb) <= 1, `seed ${seed} step ${i}: ${activeCount(tb)} assets asking/running`);
    // I6's second half: no new ask while a background op is on the wire.
    if (bg > 0) assert.equal(effects.some(e => e.do === "askLock"), false, `seed ${seed} step ${i}: an ask while a background op was on the wire`);
    // I2: a running asset holds its lock; a pass exists only in running.
    for (const [x, s] of tb.assets) {
      assert.equal(s.k === "running", passes.has(x), `seed ${seed} step ${i}: ${x} ${s.k} vs pass ${passes.has(x)}`);
      if (s.k === "running") assert.ok(locks.has(x), `seed ${seed} step ${i}: ${x} runs without its lock`);
    }
    // The cadence is the FULL pass's schedule: a refused INCREMENTAL ask leaves it where it was.
    if (ev.e === "heldElsewhere" && before.assets.get(ev.target)?.k === "asking" && !before.assets.get(ev.target).kind.full) {
      assert.equal(tb.assets.get(ev.target).nextFullAt, before.assets.get(ev.target).nextFullAt, `seed ${seed} step ${i}: an incremental refusal moved the full cadence`);
    }
    // The person's AuditNow is never lost: afterwards the asset runs, or waits with a FULL request.
    if (ev.e === "auditNow" && tb.assets.has(ev.target)) {
      const s = tb.assets.get(ev.target);
      assert.ok(s.k === "running" || ((s.k === "queued" || s.k === "asking") && s.kind.full), `seed ${seed} step ${i}: AuditNow left ${ev.target} ${s.k}${s.kind ? (s.kind.full ? " full" : " incremental") : ""}`);
    }
    // I3: a due never takes a running pass out of running.
    if (ev.e === "due") for (const [x, s] of before.assets) if (s.k === "running") assert.equal(tb.assets.get(x).k, "running", `seed ${seed} step ${i}: a due touched a running pass`);
  }
  // The driver only answers what was asked, except lock answers for removed assets, so impossible counts only those.
  return tb.impossible;
}

await t("**THE MODEL: 200 seeded sequences × 400 steps, I2/I3/I4/I6, the cadence and AuditNow after every step**", () => {
  for (let seed = 1; seed <= 200; seed++) model(seed);
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall ok\n");
process.exit(failures ? 1 : 0);
