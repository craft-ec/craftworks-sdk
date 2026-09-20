// THE REFRESH PATH: what makes a binding show data it did not have.
//
// A binding's `reload` used to read the LOCAL COPY and return early when its
// root had not moved. The copy's root moves only when a delta or a page
// arrives — and nothing sent `ChangesSince`, so the root never moved, a
// loaded range was always answered from cache, and a tab that made no write
// could never see another's. The engine had the mechanism and
// `CachedStore::on_delta` had the mechanism; no line joined them.
//
// Driven through the ENTRY with a fake socket playing the engine, because
// that is the path an app takes and the one every previous gate missed.

import assert from "node:assert/strict";
import { wrap } from "../../js/wrap.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const settle = () => new Promise(r => setTimeout(r, 10));

/**
 * A session that records what CROSSED and can be handed an answer.
 *
 * `asks` counts `ChangesSince` requests, which is the measurement: a binding
 * that never asks cannot be told.
 */
function engineRaw() {
  let rows = [], changed = [], bound = new Set(), asks = 0, foreign = 0;
  const asked = new Set();
  let pending = null;        // a delta the engine is holding, until asked
  const session = {
    url: () => "ws://127.0.0.1:17509/",
    outbound: () => [], sent() {}, reconnected() {}, provision() {},
    take_progress: () => "[]", take_loads: () => "[]",
    provisioned: () => true, refused: () => "", exhausted: () => false, unusable: () => "[]",
    tick: () => JSON.stringify({ rolledBack: 0, stalled: null, loadsInFlight: 0 }),
    tick_ms: () => 7777,
    flush() {},
    bind: d => bound.add(d),
    unbind: d => bound.delete(d),
    // THE ENGINE ONLY ANSWERS A QUESTION IT WAS ASKED.
    //
    // The first version of this fake set `changed` directly, so a delta
    // arrived whether or not anything had sent `ChangesSince` — and deleting
    // the send left all five tests green. A fake that answers unasked
    // questions tests nothing about asking.
    refresh_domain(d) { asks += 1; asked.add(d); },
    scan: () => JSON.stringify(rows),
    root: () => `root-${rows.length}`,
    put: () => { rows = [...rows, { id: `p${rows.length}`, updated: 1 }]; return "{}"; },
    live_mode: () => JSON.stringify({ mode: "HeadSubscribed", why: "", foreignNotifications: foreign }),
    take_stale() {
      // Deliver a held delta ONLY if that domain asked since it was held.
      if (pending && asked.has(pending.domain)) {
        rows = pending.newRows;
        changed = [pending.domain];
        asked.delete(pending.domain);
        pending = null;
      }
      const c = changed; changed = [];
      return JSON.stringify(c);
    },
    /// The engine HOLDS a delta. It is delivered only to a domain that
    /// actually asked — which is the whole thing under test.
    delta(domain, newRows) { pending = { domain, newRows }; },
    emptyDelta() { pending = null; },
    asks: () => asks,
  };
  return {
    version: () => "0", buildInfo: () => "{}", blockId: () => "", parseBlockId: () => "{}",
    Db: function () { return { define() {}, domains: () => "[]" }; },
    Session: function () { return session; },
    __s: session,
  };
}

async function openDb(raw) {
  const sdk = wrap(raw);
  let deliver;
  const { db } = await sdk.open({
    port: 17509,
    fetch: async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(4) }),
    connect: (_s, { onEvent }) => {
      deliver = () => onEvent({ kind: "message" });
      return { close() {}, pump() {} };
    },
    setInterval: () => 1, clearInterval: () => {},
  });
  return { db, deliver };
}

await t("**a head move makes a live binding ASK, and the delta shows**", async () => {
  const raw = engineRaw();
  const { db, deliver } = await openDb(raw);
  const b = db.bind("tasks", { live: true });
  await settle();
  const asksBefore = raw.__s.asks();

  // The node says the head moved; the engine answers with one new row.
  raw.__s.delta("tasks", [{ id: "from-A", updated: 2 }]);
  deliver();
  await settle();

  assert.ok(raw.__s.asks() > asksBefore,
    "nothing sent ChangesSince, so the engine was never asked what changed — " +
    "a binding that does not ask cannot be told");
  assert.deepEqual(b.getSnapshot(), [{ id: "from-A", updated: 2 }],
    "the binding did not show the row the delta carried — with no app call, which is the point");
});

await t("THE CONTROL: no answer, no change, and liveMode says so", async () => {
  // Without this, a binding that re-rendered on every notification would
  // pass the test above whether or not a delta ever arrived.
  const raw = engineRaw();
  const { db, deliver } = await openDb(raw);
  const b = db.bind("tasks", { live: true });
  await settle();
  const before = b.getSnapshot();

  raw.__s.emptyDelta();      // the engine answers: nothing moved
  deliver();
  await settle();

  assert.strictEqual(b.getSnapshot(), before,
    "an empty delta re-rendered the binding; every screen would re-render on every notification about anything");
  assert.equal(typeof db.liveMode().why, "string");
});

await t("a PLAIN binding takes out no watch and is not re-run by a head move", async () => {
  const raw = engineRaw();
  const { db, deliver } = await openDb(raw);
  const b = db.bind("tasks", { live: false });
  await settle();
  const before = b.getSnapshot();

  raw.__s.delta("tasks", [{ id: "from-A", updated: 2 }]);
  deliver();
  await settle();

  assert.strictEqual(b.getSnapshot(), before,
    "a plain binding was refreshed by a head move; that is what `live` is for");
});

await t("a person's OWN write shows at once, without waiting for the network", async () => {
  const raw = engineRaw();
  const { db } = await openDb(raw);
  const b = db.bind("tasks", { live: false });
  await settle();
  let fired = 0;
  b.subscribe(() => { fired += 1; });

  await db.put("tasks", { title: "mine" });
  await settle();

  assert.equal(b.getSnapshot().length, 1,
    "a write changed base+pending but not the root, so the component never re-rendered on its own write");
  assert.ok(fired > 0, "no listener was called for the client's own write");
});

await t("the snapshot is referentially stable when nothing changed", async () => {
  const raw = engineRaw();
  const { db } = await openDb(raw);
  const b = db.bind("tasks", { live: true });
  await settle();
  const a1 = b.getSnapshot();
  await b.reload();
  assert.strictEqual(b.getSnapshot(), a1,
    "a reload with nothing to show handed back a new array; a caller re-rendering on identity would re-render for ever");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
