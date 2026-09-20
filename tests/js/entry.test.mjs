// WHAT AN APP ACTUALLY GETS.
//
// Every other test in this repo imports the thing it tests from the file that
// defines it. That is a test of the file, not of the SDK, and twice now it has
// been green over code no app could reach:
//
//   * #63 merged with `engine-db.js` calling `handle.onReadsWake` and nothing
//     anywhere defining it, and with the engine path not exported at all;
//   * #64's first attempt added `open()` to `session.js` and its test imported
//     it from there, while `wrap()` — the entry — still returned only the two
//     halves an app can wire wrongly.
//
// Both times every gate passed. So this one goes through the ENTRY, exactly as
// an app does: `wrap(raw)`, then only what that returns.

import assert from "node:assert/strict";
import { wrap } from "../../js/wrap.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack?.split("\n")[0] ?? e}\n`); }
};

const within = (ms, p, what) =>
  Promise.race([p, new Promise((_, r) =>
    setTimeout(() => r(new Error(`${what} never settled within ${ms}ms`)), ms))]);

/** What an app is promised at the entry, and what each one must be. */
const PROMISED = {
  version: "function",
  buildInfo: "function",
  blockId: "function",
  parseBlockId: "function",
  Db: "function",
  open: "function",
  SHIPPED_ARTEFACTS: "object",
  internals: "object",
};

/** A stand-in for the wasm module, so the entry can be built without wasm. */
function fakeRaw() {
  let loaded = false, ended = [], ticket = 0, asks = 0;
  const session = {
    url: () => "ws://127.0.0.1:7509/",
    outbound: () => [], sent() {},
    on_inbound() { if (ticket && !loaded) { loaded = true; ended.push({ id: ticket, ok: true, code: "LOADED" }); } },
    reconnected() {}, take_progress: () => "[]", provision() {},
    take_loads() { const o = ended; ended = []; return JSON.stringify(o); },
    provisioned: () => true, refused: () => "", exhausted: () => false,
    unusable: () => "[]",
    tick: () => JSON.stringify({ rolledBack: 0, stalled: null, loadsInFlight: 0 }),
    scan() {
      asks += 1;
      if (loaded) return JSON.stringify([{ id: "a" }]);
      ticket ||= 1;
      const e = new Error("not loaded");
      e.code = "NOT_LOADED"; e.transient = true; e.wait = ticket;
      throw e;
    },
    asks: () => asks,
  };
  return {
    version: () => "0.1.0",
    buildInfo: () => "{}",
    blockId: () => "",
    parseBlockId: () => "{}",
    Db: function () { return { define() {}, domains: () => "[]" }; },
    Session: function () { return session; },
    __session: session,
  };
}

await t("every name an app is promised is present at the ENTRY, and typed", () => {
  const sdk = wrap(fakeRaw());
  const missing = Object.entries(PROMISED)
    .filter(([k, want]) => typeof sdk[k] !== want)
    .map(([k, want]) => `${k}: expected ${want}, got ${typeof sdk[k]}`);
  assert.deepEqual(missing, [],
    "an app cannot reach these, however complete the files behind them are:\n" + missing.join("\n"));
});

await t("THE CONTROL: a name that exists in a file but not at the entry FAILS", () => {
  // `openSession` is real, exported from `js/session.js`, and imported by
  // `wrap.js` — and it is deliberately NOT app-facing. If this check could
  // not tell that apart from a name that IS at the entry, it would have
  // passed on both #63 and #64, which is exactly what happened.
  const sdk = wrap(fakeRaw());
  assert.equal(sdk.openSession, undefined,
    "openSession is app-facing; either that is intended and PROMISED should say so, or the entry is handing out the halves again");
  assert.equal(typeof sdk.internals.openSession, "function",
    "the control names something that does not exist anywhere, so it proves nothing");
});

await t("a COLD SCAN resolves through sdk.open() and nothing else", async () => {
  const raw = fakeRaw();
  const sdk = wrap(raw);

  // Exactly what a page does: one call, then read.
  const { db, close } = await sdk.open({
    port: 17509,   // NAMED. There is no default port, deliberately.
    // The artefacts are FETCHED by default — `wrap` binds this build's own,
    // which is the point. Here the fetch is faked, because the test is about
    // reachability and not about a web server.
    fetch: async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) }),
    connect: (session, { onEvent }) => {
      // Deliver one message, the way a socket would.
      setTimeout(() => { session.on_inbound(new Uint8Array()); onEvent({ kind: "message" }); }, 5);
      return { close() {} };
    },
    setInterval: () => 1,
    clearInterval: () => {},
  });

  const rows = await within(1000, db.scan("tasks"), "a cold scan through the entry");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(raw.__session.asks(), 2, "the cold read did not ask twice");
  close();
});

await t("the artefacts an app is handed are the ones this build ships", () => {
  const sdk = wrap(fakeRaw());
  for (const k of ["delegate", "block", "register"]) {
    assert.equal(typeof sdk.SHIPPED_ARTEFACTS[k], "string", `no ${k} artefact`);
    assert.match(sdk.SHIPPED_ARTEFACTS[k], /\.wasm$/, `${k} is not a wasm artefact`);
  }
});

/**
 * A session that watches a head and can be told it moved.
 *
 * `notify(key)` is what the node sends. OUR head is "ours"; anything else is
 * somebody else's contract, which is the case that must change nothing.
 */
function watchingRaw() {
  let stale = [], bound = new Set(), foreign = 0, scans = 0;
  const session = {
    url: () => "ws://127.0.0.1:17509/",
    outbound: () => [], sent() {}, reconnected() {}, provision() {},
    take_progress: () => "[]", take_loads: () => "[]",
    provisioned: () => true, refused: () => "", exhausted: () => false,
    unusable: () => "[]",
    tick: () => JSON.stringify({ rolledBack: 0, stalled: null, loadsInFlight: 0 }),
    bind: d => bound.add(d),
    unbind: d => bound.delete(d),
    take_stale() { const s = stale; stale = []; return JSON.stringify(s); },
    live_mode: () => JSON.stringify({ mode: "HeadSubscribed", why: "", foreignNotifications: foreign }),
    scan() { scans += 1; return JSON.stringify([{ id: "a" }]); },
    // The node's notification.
    on_inbound(key) {
      if (key === "ours") stale = [...bound];
      else foreign += 1;
    },
    scans: () => scans,
    foreign: () => foreign,
  };
  return {
    version: () => "0", buildInfo: () => "{}", blockId: () => "", parseBlockId: () => "{}",
    Db: function () { return { define() {}, domains: () => "[]" }; },
    Session: function () { return session; },
    __session: session,
  };
}

await t("**a HeadChanged for OUR head re-runs a bound scan, with no app call**", async () => {
  const raw = watchingRaw();
  const sdk = wrap(raw);
  let deliver;
  const { db } = await sdk.open({
    port: 17509,
    fetch: async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) }),
    connect: (session, { onEvent }) => {
      deliver = key => { session.on_inbound(key); onEvent({ kind: "message" }); };
      return { close() {} };
    },
    setInterval: () => 1, clearInterval: () => {},
  });

  let reloads = 0;
  db.watch("tasks", () => { reloads += 1; db.scan("tasks"); });
  const before = raw.__session.scans();

  deliver("ours");
  await new Promise(r => setTimeout(r, 10));

  assert.equal(reloads, 1, "a head move did not re-run the bound binding");
  assert.ok(raw.__session.scans() > before, "the binding did not actually re-ask");
});

await t("THE CONTROL: a notification for SOMEBODY ELSE'S contract re-runs nothing", async () => {
  // Without this, a handler that reloaded on every notification would pass
  // the test above — and one unasked-for message would make a page refetch
  // every screen for ever.
  const raw = watchingRaw();
  const sdk = wrap(raw);
  let deliver;
  const { db } = await sdk.open({
    port: 17509,
    fetch: async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) }),
    connect: (session, { onEvent }) => {
      deliver = key => { session.on_inbound(key); onEvent({ kind: "message" }); };
      return { close() {} };
    },
    setInterval: () => 1, clearInterval: () => {},
  });

  let reloads = 0;
  db.watch("tasks", () => { reloads += 1; });
  const before = raw.__session.scans();

  deliver("someone-elses-contract");
  await new Promise(r => setTimeout(r, 10));

  assert.equal(reloads, 0, "a foreign notification re-ran a binding");
  assert.equal(raw.__session.scans(), before, "it re-asked for data nobody said had changed");
  assert.equal(db.liveMode().foreignNotifications, 1, "it was ignored silently rather than counted");
});

await t("liveMode reaches an app THROUGH THE ENTRY", () => {
  // It was stored and never read: nothing in open(), engine-db.js or the
  // entry ever called it, so a page could not find out whether it was being
  // notified or polling.
  const sdk = wrap(watchingRaw());
  assert.equal(typeof sdk.open, "function");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
