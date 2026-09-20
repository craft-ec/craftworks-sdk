// `open()`: the one call a page makes, and the wiring it does for it.
//
// A parked read is woken by `drain`, and `drain` must be called on every
// message AND on every tick. Miss the first and a cold read waits for ever;
// miss the second and a read whose data never arrives waits for ever too.
// Neither failure says anything — the screen simply does not fill — so
// neither can be left to an app to remember.
//
// No node and no browser: the socket and the clock are injected, which is
// what lets the wiring be tested at all rather than taken on trust.

import assert from "node:assert/strict";
import { open } from "../../js/session.js";

/**
 * A deadline on every wait.
 *
 * The failure this file exists to catch is a read that NEVER settles, and
 * without a deadline that shows up as node's "unsettled top-level await"
 * warning and exit 13 — a real failure, but one that names no test and reads
 * like a crash. With it, the gate says which read hung and why that matters.
 */
const within = (ms, p, what) =>
  Promise.race([
    p,
    new Promise((_, reject) =>
      setTimeout(() => reject(new Error(`${what} never settled within ${ms}ms — a page would show a screen that never fills, and nothing saying why`)), ms)),
  ]);

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack?.split("\n")[0] ?? e}\n`); }
};

/**
 * A fake wasm `Session`: a cold range, a ticket, and data that arrives only
 * when a message is delivered — or never, if the socket is dead.
 */
function fakeSession({ deliverAfterMs = null } = {}) {
  let loaded = false, ended = [], asks = 0, ticket = 0, opened = 0;
  const self = {
    // What the page drives.
    url: () => "ws://127.0.0.1:7509/v1/contract/command?encodingProtocol=native",
    outbound: () => [],
    sent() {},
    on_inbound() { if (self.pendingDelivery) { loaded = true; ended.push({ id: ticket, ok: true, code: "LOADED" }); self.pendingDelivery = false; } },
    reconnected() {},
    take_progress: () => "[]",
    take_loads() { const o = ended; ended = []; return JSON.stringify(o); },
    provisioned: () => true,
    refused: () => "",
    exhausted: () => false,
    pendingDelivery: false,
    // The TICK. This is where a load nobody answered is given up on — in the
    // real session, `loads.time_out`.
    tick() {
      if (!loaded && ticket && self.giveUp) {
        ended.push({ id: ticket, ok: false, code: "UNAVAILABLE" });
        self.giveUp = false;
      }
      return JSON.stringify({ rolledBack: 0, stalled: null, loadsInFlight: 0 });
    },
    giveUp: false,
    asks: () => asks,
    opens: () => opened,
    // The data surface.
    scan() {
      asks += 1;
      if (loaded) return JSON.stringify([{ id: "a" }]);
      if (!ticket) { ticket = 1; if (deliverAfterMs !== null) self.pendingDelivery = true; }
      const e = new Error("not loaded");
      e.code = "NOT_LOADED"; e.transient = true; e.wait = ticket;
      throw e;
    },
  };
  return self;
}

/**
 * A `connect` that never opens a socket. `deliver()` plays a message through
 * the same path the real one uses: hand the bytes to the session, then tell
 * the page a message arrived.
 */
function fakeConnect(onEventRef) {
  return (session, { onEvent }) => {
    onEventRef.deliver = () => { session.on_inbound(new Uint8Array()); onEvent({ kind: "message" }); };
    return { close() {} };
  };
}

/** A clock the test drives: `fire()` runs the interval body once. */
function fakeClock() {
  const c = { body: null, fire: () => c.body?.() };
  c.setInterval = fn => { c.body = fn; return 1; };
  c.clearInterval = () => { c.body = null; };
  return c;
}

await t("a page that calls only open() and scan() resolves a COLD read", async () => {
  const session = fakeSession({ deliverAfterMs: 0 });
  const ref = {};
  const clock = fakeClock();
  const { db } = await open(function () { return session; }, {
    connect: fakeConnect(ref),
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
  });

  const reading = db.scan("tasks");
  // The answer arrives the way it really does: on the task that handles a
  // message. Nothing in the page called `drain`.
  setTimeout(() => ref.deliver(), 5);
  assert.deepEqual(await within(1000, reading, "a cold read"), [{ id: "a" }]);
  assert.equal(session.asks(), 2);
});

await t("A DEAD SOCKET rejects UNAVAILABLE through the TICK path", async () => {
  const session = fakeSession({ deliverAfterMs: null });
  const ref = {};
  const clock = fakeClock();
  const { db } = await open(function () { return session; }, {
    connect: fakeConnect(ref),
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
  });

  const reading = db.scan("tasks");
  // No message EVER arrives. The only thing that can end this read is the
  // tick giving up on the load — the path nothing else exercises, because
  // every other test delivers a message.
  setTimeout(() => { session.giveUp = true; clock.fire(); }, 5);
  await assert.rejects(() => within(1000, reading, "a read on a dead socket"), e => {
    assert.equal(e.code, "UNAVAILABLE",
      "the read ended for some reason other than the tick giving up on its load");
    return true;
  });
});

await t("THE CONTROL: the same setup WITH a message resolves", async () => {
  const session = fakeSession({ deliverAfterMs: 0 });
  const ref = {};
  const clock = fakeClock();
  const { db } = await open(function () { return session; }, {
    connect: fakeConnect(ref),
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
  });
  const reading = db.scan("tasks");
  setTimeout(() => ref.deliver(), 5);
  assert.deepEqual(await within(1000, reading, "the control read"), [{ id: "a" }],
    "the dead-socket test proves nothing unless this one passes: it would " +
    "also 'pass' against a wiring that resolves nothing at all");
});

await t("open() hands back a db WITHOUT the page ever seeing drain", async () => {
  const session = fakeSession({ deliverAfterMs: 0 });
  const ref = {};
  const clock = fakeClock();
  const handle = await open(function () { return session; }, {
    connect: fakeConnect(ref),
    setInterval: clock.setInterval,
    clearInterval: clock.clearInterval,
  });
  assert.ok(handle.db, "open() did not return a database");
  assert.ok(typeof handle.close === "function", "open() returned no way to close");
  // The db still HAS drain — `engineDb` is exported for tests that drive the
  // parts — but a page using open() never has to call it.
  assert.equal(typeof handle.db.drain, "function");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
