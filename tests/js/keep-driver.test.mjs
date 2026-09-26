// THE TAB'S PASS DRIVER (js/keep-driver.js; KEEPER.md §12, sdk#479 P3): keep-tab.js's effects carried out against a
// fake page, a fake clock and a fake LockManager with navigator.locks' `ifAvailable` semantics. The two controls the
// core dev named: two tabs WITHOUT locks both run and both end correctly; with NO cadence timer no pass ever starts.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { keepDriver } from "../../js/keep-driver.js";

let failures = 0;
const t = async (name, f) => {
  try { await f(); process.stdout.write(`  ok  ${name}\n`); } catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const flush = async () => { for (let i = 0; i < 20; i++) await new Promise(r => setImmediate(r)); };
const CAD = 1000;

function fakeClock() {
  let now = 0;
  const timers = [];
  return {
    now: () => now,
    // A non-positive period would never advance (a hang, not a failure): refused by name.
    every(ms, fn) { if (!(ms > 0)) throw new Error(`fake clock: a timer every ${ms} ms`); const tm = { ms, fn, next: now + ms, live: true }; timers.push(tm); return () => { tm.live = false; }; },
    advance(ms) { const end = now + ms; for (;;) { const due = timers.filter(x => x.live && x.next <= end).sort((a, b) => a.next - b.next)[0]; if (!due) break; now = due.next; due.next += due.ms; due.fn(); } now = end; },
    timers,
  };
}

/** A page per tab: a pass runs until the test ends it (`finish`), or ends at once when `instant` (a signer-less pass). */
function fakePage({ instant = false } = {}) {
  const running = new Map();
  const calls = [];
  return {
    calls,
    bg: 0,
    audit(x) { calls.push(["audit", x]); running.set(x, instant ? "done" : "running"); },
    auditSince(x, old) { calls.push(["auditSince", x, old]); running.set(x, instant ? "done" : "running"); },
    auditProgress: x => (running.get(x) === "running" ? { asked: 1, of: 2 } : null),
    cancelAudit(x) { calls.push(["cancelAudit", x]); running.set(x, "done"); },
    takeAudit(x) { const was = running.get(x); running.delete(x); return was ? { full: true, target: x } : null; },
    rootOf: x => `root-${x}`,
    backgroundInFlight() { return this.bg; },
    finish(x) { running.set(x, "done"); },
    count: kind => calls.filter(c => c[0] === kind).length,
  };
}

/** navigator.locks, `ifAvailable` only: a held name answers null; a granted lock is held until the callback's promise settles. */
function fakeLocks() {
  const held = new Set();
  let grants = 0;
  return {
    held,
    get grants() { return grants; },
    request(name, opts, cb) {
      assert.equal(opts.ifAvailable, true, "the driver asks without ifAvailable: it would queue behind another tab");
      queueMicrotask(() => {
        if (held.has(name)) { cb(null); return; }
        held.add(name);
        grants += 1;
        Promise.resolve(cb({ name })).finally(() => held.delete(name));
      });
    },
  };
}

function driver(o) {
  const writes = [];
  const d = keepDriver({ targets: ["a"], writeBack: (x, r) => writes.push([x, r]), cadenceMs: CAD, tickMs: 100, locks: null, ...o });
  return { d, writes };
}

await t("**CONTROL: with NO cadence timer, nothing starts on its own however long the tab is open**", async () => {
  const clock = fakeClock(), page = fakePage();
  driver({ page, clock, tickMs: null, targets: ["a", "b"] });
  clock.advance(100 * CAD);
  await flush();
  assert.equal(clock.timers.length, 0, "a timer was started with tickMs null");
  assert.equal(page.count("audit"), 0, "a pass started with no cadence");
  // The same tab WITH the timer: its first tick starts exactly ONE pass (I6).
  const clock2 = fakeClock(), page2 = fakePage();
  driver({ page: page2, clock: clock2, targets: ["a", "b"] });
  clock2.advance(100);
  await flush();
  assert.equal(page2.count("audit"), 1, "the cadence did not start one pass (or started two)");
});

await t("**CONTROL: two tabs WITHOUT locks both run the same asset, both end, each writes back its full pass once**", async () => {
  const clock = fakeClock();
  const [p1, p2] = [fakePage(), fakePage()];
  const { d: d1, writes: w1 } = driver({ page: p1, clock });
  const { d: d2, writes: w2 } = driver({ page: p2, clock });
  clock.advance(100);
  await flush();
  assert.deepEqual([p1.count("audit"), p2.count("audit")], [1, 1], "without locks both tabs run (a per-browser convenience, not correctness)");
  assert.deepEqual([d1.state("a"), d2.state("a")], ["running", "running"]);
  clock.advance(300); // the cadence never ends a running pass (I3)
  await flush();
  assert.deepEqual([p1.count("audit"), p2.count("audit")], [1, 1], "a due while running started a second pass");
  p1.finish("a"); d1.poke(); p2.finish("a"); d2.poke();
  await flush();
  assert.deepEqual([d1.state("a"), d2.state("a")], ["idle", "idle"]);
  assert.deepEqual([w1.length, w2.length], [1, 1], "each full pass writes back exactly once (idempotent repair: both are right)");
  for (const d of [d1, d2]) assert.deepEqual(d.stats(), { impossible: 0, unhandled: 0, held: [] });
});

await t("**with navigator.locks: of two tabs only ONE runs; the other stands down and moves its full cadence**", async () => {
  const clock = fakeClock(), locks = fakeLocks();
  const [p1, p2] = [fakePage(), fakePage()];
  const { d: d1 } = driver({ page: p1, clock, locks });
  const { d: d2 } = driver({ page: p2, clock, locks });
  clock.advance(100);
  await flush();
  assert.equal(p1.count("audit") + p2.count("audit"), 1, "two tabs ran with the lock held");
  const [runner, other, otherPage] = p1.count("audit") ? [d1, d2, p2] : [d2, d1, p1];
  assert.equal(other.state("a"), "idle");
  assert.deepEqual(runner.stats().held, ["a"], "the runner does not hold its lock (I2)");
  // The runner ends: its lock is given back (I2).
  (runner === d1 ? p1 : p2).finish("a"); runner.poke();
  await flush();
  assert.equal(locks.held.size, 0, "a pass ended and its lock was kept");
  // Both tabs' next full pass is a cadence away; whichever asks first runs, the other stands down again: still ONE.
  const runnerPage = runner === d1 ? p1 : p2;
  clock.advance(CAD);
  await flush();
  assert.equal(p1.count("audit") + p2.count("audit"), 2, "not exactly one more pass across the two tabs");
  // The runner's tab closes: its lock is given back, and at the next cadence the other tab takes the asset over.
  runnerPage.finish("a"); runner.poke();
  await flush();
  runner.close();
  const before = otherPage.count("audit");
  clock.advance(CAD);
  await flush();
  assert.equal(otherPage.count("audit"), before + 1, "the other tab never took over after the runner's tab closed");
});

await t("**I2: a lock granted to an asset REMOVED while asking is given straight back**", async () => {
  const clock = fakeClock(), page = fakePage();
  let grant;
  const locks = { request(name, opts, cb) { grant = () => cb({ name }); } };
  const { d } = driver({ page, clock, locks });
  clock.advance(100);
  await flush();
  assert.equal(d.state("a"), "asking");
  d.remove("a");
  let released = false;
  Promise.resolve(grant()).then(() => { released = true; });
  await flush();
  assert.equal(released, true, "a lock granted to a removed asset is held for ever");
  assert.equal(page.count("audit"), 0);
  assert.equal(d.stats().impossible, 1, "the answer to nothing is counted (a)");
});

await t("**a signer-less pass is decided at START (PageIo::audit): ended at once, written back, the slot freed**", async () => {
  const clock = fakeClock(), page = fakePage({ instant: true });
  const { d, writes } = driver({ page, clock, targets: ["a", "b"] });
  clock.advance(100);
  await flush();
  assert.deepEqual(page.calls.map(c => c[1]), ["a", "b"], "the freed slot did not go to the next queued asset");
  assert.deepEqual(writes.map(w => w[0]), ["a", "b"]);
  assert.deepEqual([d.state("a"), d.state("b")], ["idle", "idle"]);
});

await t("**Cancel: cancel_audit, the lock released, idle; a head move mid-pass runs INCREMENTAL after, with no write-back**", async () => {
  const clock = fakeClock(), page = fakePage(), locks = fakeLocks();
  const { d, writes } = driver({ page, clock, locks });
  clock.advance(100);
  await flush();
  d.headMoved("a");
  page.finish("a"); d.poke();
  await flush();
  assert.deepEqual(page.calls.at(-1), ["auditSince", "a", "root-a"], "the head move was not run as an incremental pass from the root the pass started at");
  assert.equal(writes.length, 1, "only the FULL pass writes back (I4)");
  d.cancel("a");
  await flush();
  assert.deepEqual(page.calls.at(-1), ["cancelAudit", "a"]);
  assert.equal(d.state("a"), "idle");
  assert.equal(locks.held.size, 0, "a cancelled pass kept its lock");
});

await t("**I6's seam: a background op on the wire holds the tab's slot; a poke after it leaves starts the pass**", async () => {
  const clock = fakeClock(), page = fakePage();
  page.bg = 1;
  const { d } = driver({ page, clock });
  clock.advance(100);
  await flush();
  assert.equal(d.state("a"), "queued");
  page.bg = 0;
  d.poke();
  await flush();
  assert.equal(d.state("a"), "running");
});

await t("**close(): no more passes, every held lock given back**", async () => {
  const clock = fakeClock(), page = fakePage(), locks = fakeLocks();
  const { d } = driver({ page, clock, locks });
  clock.advance(100);
  await flush();
  d.close();
  await flush();
  assert.equal(locks.held.size, 0);
  clock.advance(10 * CAD);
  await flush();
  assert.equal(page.count("audit"), 1);
});

await t("**every dependency is required: a missing one is refused by name (builder#73)**", () => {
  const base = { targets: [], page: fakePage(), writeBack: () => {}, locks: null, clock: fakeClock(), cadenceMs: CAD, tickMs: null };
  for (const k of Object.keys(base)) {
    const o = { ...base };
    delete o[k];
    assert.throws(() => keepDriver(o), new RegExp(k), `${k} was defaulted`);
  }
  keepDriver(base); // THE CONTROL: all given, it builds
});

await t("**ONE WRITER: the driver never writes the tab's state; only `step`'s result replaces it**", () => {
  const src = readFileSync(new URL("../../js/keep-driver.js", import.meta.url), "utf8").replace(/\/\/.*$/gm, "");
  const writes = src.match(/\bt\s*=\s*[^=][^;]*;/g) ?? [];
  assert.deepEqual(writes.map(w => w.trim()), ["t = newTab(targets, clock.now());", "t = r.tab;"], "a second writer of the tab state");
  assert.equal(/\.assets\.(set|delete)\(|\.k\s*=[^=]/.test(src), false, "the driver edits an asset's state directly");
  // THE CONTROL: the reader finds real code (a missing file must not pass as "no writes").
  assert.ok(src.includes("function carryOut"), "the source read is empty");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall ok\n");
process.exit(failures ? 1 : 0);
