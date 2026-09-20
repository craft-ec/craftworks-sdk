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

import assert from "node:assert/strict";
import { engineDb } from "../../js/engine-db.js";
import { wrap } from "../../js/wrap.js";

let failures = 0;
const t = (name, fn) => {
  try { fn(); process.stdout.write(`  ok  ${name}\n`); }
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

t("**the engine-backed db offers every method the in-memory one does**", () => {
  const memory = surfaceOf(wrap(fakeRaw()).Db.prototype ?? new (wrap(fakeRaw()).Db)());
  const engine = surfaceOf(engineDb(fakeSession()));

  const missing = [...memory].filter(m => !engine.has(m));
  assert.deepEqual(
    missing, [],
    `an app that switched backend would break on exactly these: ${missing.join(", ")}.\n` +
    "This is the gate that was missing when `bind` was absent and a published " +
    "project threw `db.bind is not a function` on its first render.");
});

t("THE CONTROL: the reader really does find methods", () => {
  // Without this, a `surfaceOf` that returned an empty set would make the
  // test above pass against any two objects at all.
  const engine = surfaceOf(engineDb(fakeSession()));
  for (const m of ["put", "get", "scan", "bind"]) {
    assert.ok(engine.has(m), `the reader did not find ${m}, so it finds nothing`);
  }
});

t("and the engine side's extras are declared, not accidental", () => {
  const memory = surfaceOf(wrap(fakeRaw()).Db.prototype ?? new (wrap(fakeRaw()).Db)());
  const engine = surfaceOf(engineDb(fakeSession()));
  const extra = [...engine].filter(m => !memory.has(m) && !ENGINE_ONLY.has(m));
  assert.deepEqual(
    extra, [],
    `the engine surface grew ${extra.join(", ")}, which an app on an unpublished ` +
    "project would not have. Add them to the in-memory one, or to ENGINE_ONLY with a reason.");
});

t("a BINDING from the engine has the shape a component holds", () => {
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
