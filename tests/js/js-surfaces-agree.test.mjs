// THE TWO JAVASCRIPT SURFACES MUST AGREE TOO.
//
// `web/tests/surfaces_agree.rs` compares the two WASM objects. That is half
// the boundary: an app does not hold a wasm object, it holds what `wrap()`
// and `engineDb()` return — and `bind` lives there, on neither wasm side.
//
// So `engineDb` had no `bind` at all, and a published project threw
// `db.bind is not a function` the moment it rendered. Every gate was green:
// the Rust one compared the wrong pair of objects, and no JavaScript one
// existed. "Publish switches the backend and the app code does not change"
// has to be true of the object a component actually holds.
//
// AND THE NAME IS NOT THE SURFACE. This file then compared names only, and
// the very next instance of the same failure walked straight through it: the
// engine-backed `schema` and `count` are ASYNC — they read over the network —
// and the in-memory ones answered values. An app got a Promise where it
// expected an object, and a Promise is truthy, so it passed an
// `if (!schema)` guard and arrived at `schema.fields.map` as `undefined.map`.
//
// Measured against a real node (sdk#87): four defects in the builder, three
// of which replaced the whole app with an exception string while the button
// said "Published", and one of which printed "— [object Promise] records" to
// a person — a wrong number, no crash, nothing would have reported it.
//
// So the second gate below compares what each method ANSWERS WITH: a promise
// or a value. An app must not be able to tell which backend it has from the
// shape of an answer.

import assert from "node:assert/strict";
import { engineDb } from "../../js/engine-db.js";
import { wrap } from "../../js/wrap.js";

let failures = 0;
/**
 * AWAITED. It was not, and an async test's rejection was swallowed: the
 * helper wrote `ok` and moved on while the assertions were still pending.
 *
 * Found by mutation — the state-comparison test below reported `ok` against
 * code with the fix REMOVED, which is a test that cannot fail. Every call is
 * awaited now, so a synchronous test is unaffected and an async one is
 * actually run.
 */
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};

/** A wasm stand-in: enough for `wrap` and `engineDb` to be constructed. */
const fakeRaw = () => ({
  version: () => "0", buildInfo: () => "{}", blockId: () => "", parseBlockId: () => "{}",
  Db: function () {
    return {
      define() {}, schema: () => "null", domains: () => "[]",
      put: () => "{}", update: () => "{}", get: () => "null", delete: () => false,
      scan: () => "[]", count: () => 0, root: () => "", stats: () => "{}",
    };
  },
  Session: function () { return {}; },
});

const fakeSession = () => ({
  session: {
    schema: () => "null", domains: () => "[]", put: () => "{}", update: () => "{}",
    get: () => "null", delete: () => false, scan: () => "[]", count: () => 0,
    root: () => "", stats: () => "{}", define: () => undefined,
    preload: () => 0, trace: () => "null", trace_on: () => undefined,
    take_loads: () => "[]", take_stale: () => "[]", live_mode: () => "{}",
    bind: () => undefined, unbind: () => undefined,
    refresh_domain: () => undefined, take_stale: () => "[]",
  },
});

/** Methods an object offers, own and inherited, excluding the constructor. */
const surfaceOf = o => {
  const names = new Set();
  for (let p = o; p && p !== Object.prototype; p = Object.getPrototypeOf(p)) {
    for (const k of Object.getOwnPropertyNames(p)) {
      if (k === "constructor") continue;
      const d = Object.getOwnPropertyDescriptor(p, k);
      if (typeof d?.value === "function") names.add(k);
    }
  }
  return names;
};

/** On the engine side only: there is no engine behind the in-memory one. */
const ENGINE_ONLY = new Set(["preload", "trace", "traceOn", "watch", "liveMode", "drain"]);

await t("**the engine-backed db offers every method the in-memory one does**", () => {
  const memory = surfaceOf(wrap(fakeRaw()).Db.prototype ?? new (wrap(fakeRaw()).Db)());
  const engine = surfaceOf(engineDb(fakeSession()));

  const missing = [...memory].filter(m => !engine.has(m));
  assert.deepEqual(
    missing, [],
    `an app that switched backend would break on exactly these: ${missing.join(", ")}.\n` +
    "This is the gate that was missing when `bind` was absent and a published " +
    "project threw `db.bind is not a function` on its first render.");
});

await t("THE CONTROL: the reader really does find methods", () => {
  // Without this, a `surfaceOf` that returned an empty set would make the
  // test above pass against any two objects at all.
  const engine = surfaceOf(engineDb(fakeSession()));
  for (const m of ["put", "get", "scan", "bind"]) {
    assert.ok(engine.has(m), `the reader did not find ${m}, so it finds nothing`);
  }
});

await t("and the engine side's extras are declared, not accidental", () => {
  const memory = surfaceOf(wrap(fakeRaw()).Db.prototype ?? new (wrap(fakeRaw()).Db)());
  const engine = surfaceOf(engineDb(fakeSession()));
  const extra = [...engine].filter(m => !memory.has(m) && !ENGINE_ONLY.has(m));
  assert.deepEqual(
    extra, [],
    `the engine surface grew ${extra.join(", ")}, which an app on an unpublished ` +
    "project would not have. Add them to the in-memory one, or to ENGINE_ONLY with a reason.");
});

// ---------------------------------------------------------------------------
// SHAPE, not just name.
// ---------------------------------------------------------------------------

/** Plausible arguments, so every method can actually be called. */
const ARGS = {
  define: ["d", { type: "T", fields: [] }],
  schema: ["d"], domains: [], count: ["d"], get: ["d", "x"], delete: ["d", "x"],
  put: ["d", {}], update: ["d", "x", {}], scan: ["d"],
  root: [], stats: [], bind: ["d"],
};

/** Is this a promise? The only question the two surfaces must agree on. */
const isThenable = v => typeof v?.then === "function";

/**
 * What a method answers with, or why it could not be asked.
 *
 * A method that THROWS is reported as such rather than skipped: "could not
 * check" is a third outcome, and in a log it is indistinguishable from a pass.
 */
const shapeOf = (obj, name) => {
  if (!(name in ARGS)) return { kind: "no-args-known" };
  try {
    const v = obj[name](...ARGS[name]);
    if (isThenable(v)) v.catch(() => {});      // nothing here awaits; do not warn
    return { kind: isThenable(v) ? "promise" : "value" };
  } catch (e) {
    return { kind: "threw", why: String(e?.message ?? e) };
  }
};

await t("**both surfaces answer the same SHAPE, not just the same names**", () => {
  const memory = new (wrap(fakeRaw()).Db)();
  const engine = engineDb(fakeSession());
  const shared = [...surfaceOf(memory)].filter(m => surfaceOf(engine).has(m)).sort();

  const differ = [], unchecked = [];
  for (const name of shared) {
    const a = shapeOf(memory, name), b = shapeOf(engine, name);
    if (a.kind === "no-args-known") { unchecked.push(name); continue; }
    if (a.kind === "threw" || b.kind === "threw") {
      unchecked.push(`${name} (threw: ${a.why ?? b.why})`);
      continue;
    }
    if (a.kind !== b.kind) differ.push(`${name}: in-memory ${a.kind}, engine ${b.kind}`);
  }

  assert.deepEqual(differ, [],
    "an app can tell which backend it has from the shape of an answer:\n  " +
    differ.join("\n  ") +
    "\nA caller written against one breaks on the other, and a Promise is " +
    "truthy — so it passes a falsy guard and fails later, somewhere else.");
  assert.deepEqual(unchecked, [],
    `these could not be compared at all: ${unchecked.join(", ")}. ` +
    "A method this gate cannot ask is a method it does not check, and in a " +
    "green log that is indistinguishable from one it checked. Give it " +
    "arguments in ARGS, or say why it is exempt.");
  console.log(`      ${shared.length} shared methods, all compared by shape`);
});

await t("THE CONTROL: a method that is sync on one side and async on the other FAILS", () => {
  // Without this, a `shapeOf` that returned the same kind for everything —
  // a broken thenable check, an early return — would report perfect agreement
  // over two surfaces that agree about nothing.
  const memory = new (wrap(fakeRaw()).Db)();
  const engine = engineDb(fakeSession());
  // `count` is async on both. Make the in-memory one answer a value.
  const patched = Object.create(memory);
  patched.count = () => 0;

  const a = shapeOf(patched, "count"), b = shapeOf(engine, "count");
  assert.equal(a.kind, "value", "the control did not actually make it synchronous");
  assert.equal(b.kind, "promise", "the engine side is not async, so there is nothing to detect");
  assert.notEqual(a.kind, b.kind, "a sync-vs-async pair was not detected as a difference");
});

await t("THE CONTROL: the shape reader is not simply calling everything a promise", () => {
  // `root` and `stats` are synchronous on BOTH: a root is a value this client
  // already holds. If the reader called them promises, the gate above would
  // pass by agreeing on the wrong answer everywhere.
  const memory = new (wrap(fakeRaw()).Db)();
  for (const name of ["root", "stats"]) {
    assert.equal(shapeOf(memory, name).kind, "value",
      `${name} was read as a promise; the reader cannot tell the two apart`);
  }
});

// ---------------------------------------------------------------------------
// A WRITE REACHING THE NETWORK IS A CHANGE THE SCREEN MUST SEE.
// ---------------------------------------------------------------------------

/**
 * A row whose STATE moved, and nothing else.
 *
 * `PENDING -> CLEAN` is what happens when a write reaches the network. It
 * changes neither `id` nor `updated` — `updated` is the record's own
 * timestamp, and a write state is not a content change — so a snapshot
 * comparison of those two says "the same rows" and the component never
 * re-renders.
 *
 * MEASURED against a real node: a row sat on screen saying "saving" for 70
 * seconds while `db.scan()` returned it CLEAN the whole time. The data was
 * published within a second. Only the screen was wrong, which is the worst
 * version of this: a person is told their data is unsaved when it is safely
 * on the network, and closing the tab then feels like losing it.
 */
const ROW = (state) => ({ id: "a", updated: 7, fields: { t: "x" }, state });

await t("**a row whose write STATE changed is a new snapshot, on the engine side**", async () => {
  let state = "PENDING";
  const session = {
    ...fakeSession().session,
    scan: () => JSON.stringify([ROW(state)]),
    root: () => "node:same",
  };
  const db = engineDb({ session });
  const b = db.bind("notes");
  await b.reload();
  const first = b.getSnapshot();
  assert.equal(first[0].state, "PENDING");

  let told = 0;
  b.subscribe(() => { told += 1; });
  state = "CLEAN";                      // the write reached the network
  await b.reload();

  assert.notEqual(b.getSnapshot(), first,
    "the snapshot was not replaced when the row's write state changed, so a " +
    "component holding it renders `saving` for ever over data that is safely " +
    "on the network");
  assert.equal(b.getSnapshot()[0].state, "CLEAN", "the new snapshot has the old state");
  assert.equal(told, 1, "nobody was told, so nothing re-renders");
});

await t("THE CONTROL: a reload that changes NOTHING keeps the same snapshot", () => {
  // The referential stability this comparison exists for. Without this, a
  // `same` that always returned false would pass the test above and make
  // useSyncExternalStore re-render on every render, for ever.
  const session = {
    ...fakeSession().session,
    scan: () => JSON.stringify([ROW("CLEAN")]),
    root: () => "node:same",
  };
  const db = engineDb({ session });
  const b = db.bind("notes");
  return b.reload().then(async () => {
    const first = b.getSnapshot();
    let told = 0;
    b.subscribe(() => { told += 1; });
    await b.reload();
    assert.equal(b.getSnapshot(), first, "an unchanged reload replaced the snapshot");
    assert.equal(told, 0, "an unchanged reload told a listener");
  });
});

await t("a BINDING from the engine has the shape a component holds", () => {
  const b = engineDb(fakeSession()).bind("tasks", { live: false });
  for (const m of ["getSnapshot", "subscribe", "reload"]) {
    assert.equal(typeof b[m], "function", `a binding has no ${m}`);
  }
  // Referentially stable: a caller re-rendering on identity change would
  // otherwise re-render for ever, whatever the data did.
  assert.strictEqual(b.getSnapshot(), b.getSnapshot(), "the snapshot is a new object each call");
  assert.equal(b.live, false);
  assert.equal(engineDb(fakeSession()).bind("tasks", { live: true }).live, true);
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall passing\n");
process.exit(failures ? 1 : 0);
