// The engine-backed database wrapper: promises, stable codes, and a read that
// actually waits for the data.
//
// No wasm and no socket. The fake session behaves the way the real one does —
// a NOT_LOADED carries a TICKET, the data arrives on a LATER TASK, and the
// ticket is only reported ended once it has. That last part is the whole
// point: the first version of this wrapper awaited a resolved promise and
// asked again in a microtask, before any message could have arrived, and its
// test passed because the fake scripted the second call to succeed.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { engineDb, DbError } from "../../js/engine-db.js";

let failures = 0;

/**
 * Every test gets a DEADLINE.
 *
 * The failure this exists for is not slowness. A wrapper that retries without
 * a bound does not fail — it HANGS, and a suite with no deadline cannot tell
 * that apart from a slow machine: it simply never finishes, and in CI that
 * reads as a stuck runner rather than as a broken retry.
 *
 * Found by mutation: removing the ticket check from `once` (so a NOT_LOADED
 * with nothing to wait on is retried for ever) wedged this file instead of
 * failing it. Retries and bounds are most of what this file tests, so a
 * missing bound has to be a FAILING test.
 */
const DEADLINE_MS = 5_000;
const t = async (name, fn) => {
  let timer;
  try {
    await Promise.race([
      fn(),
      new Promise((_, bad) => {
        timer = setTimeout(() => bad(new Error(
          `did not finish within ${DEADLINE_MS}ms — a retry with no bound hangs rather than failing`)), DEADLINE_MS);
      }),
    ]);
    process.stdout.write(`  ok  ${name}\n`);
  }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack?.split("\n")[0] ?? e}\n`); }
  finally { clearTimeout(timer); }
};

/**
 * A session whose data ARRIVES LATER.
 *
 * `afterMs` is when the load completes. Until then the read throws
 * NOT_LOADED with a ticket, exactly as the Rust side does, and `take_loads`
 * reports nothing. A wrapper that re-asks without waiting sees the second
 * NOT_LOADED and fails — which is the behaviour the old test could not see.
 */
const lateSession = ({ afterMs = 20, rows = [{ id: "a" }], fail = false } = {}) => {
  let loaded = false;
  let asks = 0;
  let requests = 0;
  let ended = [];
  let nextTicket = 1;
  const byRange = new Map();
  const s = {
    asks: () => asks,
    requests: () => requests,
    scan(domain = "tasks") {
      asks += 1;
      if (loaded) return JSON.stringify(rows);
      // ONE request per range, as the session does.
      let ticket = byRange.get(domain);
      if (ticket === undefined) {
        ticket = nextTicket++;
        byRange.set(domain, ticket);
        requests += 1;
        setTimeout(() => {
          loaded = !fail;
          ended.push({ id: ticket, ok: !fail, code: fail ? "UNAVAILABLE" : "LOADED" });
          s.pump();               // the page, woken by a message arriving
        }, afterMs);
      }
      const e = new Error("not loaded");
      e.code = "NOT_LOADED";
      e.transient = true;
      e.wait = ticket;
      throw e;
    },
    put() { return s.scan(); },
    create_at(domain = "tasks") { return s.scan(domain); },
    // A COLD DEFINE. It reads the existing schema before it can check the new
    // one against it, so until that range is loaded it fails exactly as a
    // read does — with a ticket.
    define(domain = "tasks") { s.scan(domain); return undefined; },
    update(domain = "tasks") { return s.scan(domain); },
    delete(domain = "tasks") { s.scan(domain); return true; },
    take_loads() { const out = ended; ended = []; return JSON.stringify(out); },
    pump: () => {},
  };
  return s;
};

await t("a read waits for the data and then answers — TWO asks, one request", async () => {
  const s = lateSession({ afterMs: 20 });
  const db = engineDb(s);
  s.pump = db.drain;
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.asks(), 2, "the read did not ask exactly twice");
  assert.equal(s.requests(), 1, "the read issued more than one request for one range");
});

await t("CONCURRENT reads of one unloaded range issue ONE request", async () => {
  const s = lateSession({ afterMs: 20 });
  const db = engineDb(s);
  s.pump = db.drain;
  const all = await Promise.all([db.scan("tasks"), db.scan("tasks"), db.scan("tasks")]);
  for (const rows of all) assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.requests(), 1,
    "three components reading one range asked the node three times; that multiplies every first paint");
});

await t("THE CONTROL: a loaded read asks ONCE and requests nothing", async () => {
  const s = lateSession({ afterMs: 0 });
  const db = engineDb(s);
  s.pump = db.drain;
  await db.scan("tasks");                     // warm it
  const asksBefore = s.asks(), reqBefore = s.requests();
  const rows = await db.scan("tasks");
  assert.deepEqual(rows, [{ id: "a" }]);
  assert.equal(s.asks() - asksBefore, 1, "a loaded read asked twice");
  assert.equal(s.requests() - reqBefore, 0,
    "a loaded read requested a range it already had; without this the test above proves nothing");
});

await t("a load that ENDS without delivering rejects — it is not 'not yet'", async () => {
  const s = lateSession({ afterMs: 10, fail: true });
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(() => db.scan("tasks"), e => {
    assert.ok(e instanceof DbError);
    assert.equal(e.code, "UNAVAILABLE");
    return true;
  });
});

await t("a NOT_LOADED with no ticket rejects rather than hanging", async () => {
  const s = {
    scan() { const e = new Error("x"); e.code = "NOT_LOADED"; throw e; },
    take_loads: () => "[]",
  };
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => e.code === "NOT_LOADED");
});

await t("an error a load cannot fix is NOT waited on", async () => {
  let asks = 0;
  const s = {
    scan() { asks += 1; const e = new Error("too big"); e.code = "TOO_LARGE"; e.transient = false; throw e; },
    take_loads: () => "[]",
  };
  const db = engineDb(s);
  await assert.rejects(() => db.scan("tasks"), e => e.code === "TOO_LARGE");
  assert.equal(asks, 1, "a TOO_LARGE was retried; no load makes a record smaller");
});

// ---------------------------------------------------------------------------
// A WRITE THAT COULD NOT READ IS NOT AN INVALID WRITE.
//
// This block replaces a test that asserted the opposite — "a WRITE is never
// retried, whatever it says" — whose stated fear was that a retried write can
// be applied twice. That fear is answered rather than ignored: in `Db` every
// read a write makes happens BEFORE the single write that mutates, and that
// write applies the whole edit or none of it. A NOT_LOADED therefore leaves
// the tree untouched, so asking again repeats the attempt and not the effect.
//
// What the old rule cost (sdk#89): a cold `define` returned a ticketless
// NOT_LOADED that nothing could retry, and a published app kept a form on
// screen, a button saying "Published", and refused every write with `domain
// has no schema; define it first` — permanently, because the next mount ran
// the same cold define. Measured against a real node: 120 of 120 refused.
// ---------------------------------------------------------------------------

await t("**a COLD DEFINE succeeds, with no preload**", async () => {
  // The user-visible bug, as a test. Nothing warms the range first: the
  // define itself queues the load, waits for it, and asks once more.
  const s = lateSession({ afterMs: 20 });
  const db = engineDb(s);
  s.pump = db.drain;
  await db.define("tasks", { type: "Task", fields: [] });
  assert.equal(s.asks(), 2, "the define did not ask exactly twice");
  assert.equal(s.requests(), 1, "it issued more than one request for one range");
});

await t("a cold PUT, CREATE_AT, UPDATE and DELETE do the same — they all read first", async () => {
  // `need_schema` is on all three paths, so none of them is a special case.
  for (const [name, call] of [
    ["put", db => db.put("tasks", {})],
    // sdk#149: a cold slot is NOT absent — createAt waits to read it too.
    ["createAt", db => db.createAt("tasks", "a", {})],
    ["update", db => db.update("tasks", "a", {})],
    ["delete", db => db.delete("tasks", "a")],
  ]) {
    const s = lateSession({ afterMs: 10 });
    const db = engineDb(s);
    s.pump = db.drain;
    await call(db);
    assert.equal(s.asks(), 2, `a cold ${name} did not wait for the range it needed`);
  }
});

await t("a load that never answers ends the write UNAVAILABLE, bounded", async () => {
  // Not a hang and not an infinite retry: the load ENDS, having failed, and
  // the write is told so.
  const s = lateSession({ afterMs: 10, fail: true });
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(
    () => db.define("tasks", { type: "Task", fields: [] }),
    e => e.code === "UNAVAILABLE",
    "a define whose range never loaded did not end UNAVAILABLE");
  assert.ok(s.asks() <= 2, `it asked ${s.asks()} times for a range that was never going to load`);
});

await t("THE CONTROL: an INVALID write is still refused, and NOT retried", async () => {
  // The half of the old rule that was right, kept. Without this, routing
  // writes through the retry would look correct while quietly re-sending a
  // write that no load can ever make valid.
  // IT CARRIES A TICKET. That is the whole point of the control: with no
  // ticket the retry stops for a second reason, and the test would pass
  // whether or not the wrapper still checks the code at all. Found by
  // mutation — deleting the code check left this green.
  let asks = 0;
  const s = {
    define() {
      asks += 1;
      const e = new Error("field `title` must be text");
      e.code = "REFUSED";
      e.transient = false;
      e.wait = 1;              // something TO wait on, deliberately
      throw e;
    },
    take_loads: () => JSON.stringify([{ id: 1, ok: true, code: "LOADED" }]),
  };
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(() => db.define("tasks", { type: "Task", fields: [] }), e => e.code === "REFUSED");
  assert.equal(asks, 1,
    "an invalid write was re-sent although its code says no load can help; " +
    "the wrapper is deciding on the ticket rather than on what went wrong");
});

await t("and the retry is BOUNDED even when every attempt hands back a ticket", async () => {
  // A session that always says "not loaded, wait for this" and always
  // completes the load — progress on every hop, and never an answer. Nothing
  // but MAX_HOPS stops it.
  //
  // Without this, removing the hop bound was caught by no test at all: the
  // other fakes stop issuing NOT_LOADED once loaded, so the loop ends for a
  // reason that has nothing to do with the bound.
  let asks = 0, ticket = 0;
  const ended = [];
  const s = {
    define() {
      asks += 1;
      ticket += 1;
      const t = ticket;
      setTimeout(() => { ended.push({ id: t, ok: true, code: "LOADED" }); s.pump(); }, 1);
      const e = new Error("not loaded");
      e.code = "NOT_LOADED"; e.transient = true; e.wait = t;
      throw e;
    },
    take_loads() { const out = ended.splice(0); return JSON.stringify(out); },
    pump: () => {},
  };
  const db = engineDb(s);
  s.pump = db.drain;
  await assert.rejects(
    () => db.define("tasks", { type: "Task", fields: [] }),
    e => e.code === "NOT_LOADED",
    "a write that is always told to wait never gave up");
  assert.ok(asks <= 10,
    `it asked ${asks} times; the hop bound is what stops a chain that makes ` +
    "progress for ever, and an unbounded one hangs rather than fails");
});

await t("THE CONTROL: a ticketless NOT_LOADED on a write still surfaces", async () => {
  // `park` hands back no ticket when the span was already loaded and the read
  // still cannot be answered — asking again would answer identically. The
  // write must be told, not sent round.
  let asks = 0;
  const s = {
    define() { asks += 1; const e = new Error("not loaded"); e.code = "NOT_LOADED"; e.transient = true; throw e; },
    take_loads: () => "[]",
  };
  const db = engineDb(s);
  await assert.rejects(() => db.define("tasks", { type: "Task", fields: [] }), e => e.code === "NOT_LOADED");
  assert.equal(asks, 1, "a write with no ticket to wait on was retried anyway");
});

await t("a write the store REFUSED rejects with its code, whether it may be retried, and the bound it met — never resolves as made (sdk#180)", async () => {
  // Rust built this: `{ code, message, transient, retryable, cap }`. The
  // wrapper must carry every field a caller acts on — a handoff waits for a
  // confirmation and retries on `retryable`, and shows `cap`.
  let asks = 0;
  const refusal = { code: "NO_ROOM", message: "not written: 256 writes are already waiting for an answer", transient: false, retryable: true, cap: 256 };
  const s = { create_at() { asks += 1; throw { ...refusal }; }, put() { asks += 1; throw { ...refusal }; },
              update() { asks += 1; throw { ...refusal }; }, delete() { asks += 1; throw { ...refusal }; } };
  const db = engineDb(s);
  for (const [name, call] of [["createAt", () => db.createAt("tasks", "a".repeat(32), {})], ["put", () => db.put("tasks", {})],
                              ["update", () => db.update("tasks", "a".repeat(32), {})], ["delete", () => db.delete("tasks", "a".repeat(32))]]) {
    await assert.rejects(call, e => {
      assert.equal(e.name, "DbError", `${name}: not a DbError`);
      assert.equal(e.code, "NO_ROOM", `${name}: the code`);
      assert.equal(e.retryable, true, `${name}: retryable was lost`);
      assert.equal(e.cap, 256, `${name}: the cap was lost`);
      assert.equal(e.transient, false, `${name}: a reload does not make room`);
      return true;
    }, `${name} resolved for a write the store refused`);
  }
  assert.equal(asks, 4, "a refused write was re-sent by the wrapper: waiting for room is the caller's decision");
  // A refusal that is NOT retryable says so.
  const s2 = { put() { throw { code: "NO_SESSION", message: "no session", transient: false, retryable: false }; } };
  await assert.rejects(() => engineDb(s2).put("tasks", {}), e => e.code === "NO_SESSION" && e.retryable === false && !("cap" in e));
});

await t("nothing in the wrapper branches on a message, or waits on a timer", () => {
  const src = readFileSync(new URL("../../js/engine-db.js", import.meta.url), "utf8");
  const code = src.split("\n").filter(l => !l.trim().startsWith("//"));
  const onMessage = code.filter(l =>
    /\.message\s*(===|==|!==|!=)/.test(l) || /\.message\.(includes|startsWith|match|indexOf)/.test(l));
  assert.deepEqual(onMessage, [], `these branch on a message:\n${onMessage.join("\n")}`);
  // A timer here would either spin or answer late, and neither is a fact
  // about the data. The wake-up is an event.
  const timers = code.filter(l => /setTimeout|setInterval|Promise\.resolve\(\)/.test(l));
  assert.deepEqual(timers, [], `these wait on a clock instead of on the answer:\n${timers.join("\n")}`);
  assert.ok(/\.code\s*!==\s*"NOT_LOADED"/.test(src), "the wrapper does not branch on a code at all");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
