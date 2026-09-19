// The same behaviour as tests/collection.rs, through the JavaScript surface.
import assert from "node:assert";
import { createRequire } from "node:module";
import { wrap } from "../../js/wrap.js";
const sdk = wrap(createRequire(import.meta.url)("../../pkg/node/craftworks_sdk.js"));

const db = new sdk.Db();
db.define("tasks", { type: "Task", fields: [
  { name: "title", kind: "text", required: true }, { name: "done", kind: "bool" }, { name: "priority", kind: "int" }] });

const a = db.put("tasks", { title: "a", done: false });
const b = db.put("tasks", { title: "b", priority: 3 });
const c = db.put("tasks", { title: "c" });
assert.match(a.id, /^[0-9a-f]{32}$/);
assert.ok(a.id < b.id && b.id < c.id, "ids sort in creation order");
assert.deepStrictEqual(a.fields, { title: "a", done: false });
assert.deepStrictEqual(db.get("tasks", b.id), b);
assert.strictEqual(db.count("tasks"), 3);

const titles = rs => rs.map(r => r.fields.title);
assert.deepStrictEqual(titles(db.scan("tasks")), ["a", "b", "c"]);
assert.deepStrictEqual(titles(db.scan("tasks", { reverse: true, limit: 2 })), ["c", "b"]);
assert.deepStrictEqual(titles(db.scan("tasks", { after: a.id })), ["b", "c"]);

const u = db.update("tasks", b.id, { done: true, priority: null });
assert.deepStrictEqual(u.fields, { title: "b", done: true });
assert.strictEqual(u.created, b.created);

assert.strictEqual(db.delete("tasks", a.id), true);
assert.strictEqual(db.delete("tasks", a.id), false);
assert.strictEqual(db.get("tasks", a.id), null);

// refusals arrive as thrown Errors with the Rust message
assert.throws(() => db.put("tasks", { done: true }), /`title` is required/);
assert.throws(() => db.put("tasks", { title: 7 }), /must be text/);
assert.throws(() => db.put("tasks", { title: "t", colour: "red" }), /not a field/);
assert.throws(() => db.put("nope", { title: "t" }), /no schema/);
assert.throws(() => db.get("tasks", "xyz"), /32 hex/);
assert.throws(() => db.define("tasks", { type: "Task", fields: [] }), /only appended/);
assert.strictEqual(db.count("tasks"), 2, "refused writes store nothing");

// schemas only grow
db.define("tasks", { type: "Task", fields: [
  { name: "title", kind: "text", required: true }, { name: "done", kind: "bool" }, { name: "priority", kind: "int" },
  { name: "due", kind: "time" }] });
assert.deepStrictEqual(db.get("tasks", c.id).fields, { title: "c" });
assert.deepStrictEqual(db.domains(), ["tasks"]);
console.log("ok collection through JS");

// The tree behind the database, through the JS surface — the builder's tree
// panel needs both, and the root is what changes as the user adds records.
{
  const t = new sdk.Db();
  t.define("notes", { type: "Note", fields: [{ name: "body", kind: "text", required: true }] });
  const empty = t.root();
  assert.match(empty, /^[0-9a-f]{64}$/, "the root is a 32-byte hash in hex");
  assert.deepStrictEqual(Object.keys(t.stats()).sort(), ["blocks", "bytes", "height"]);
  assert.strictEqual(t.stats().height, 1, "a small tree is one leaf");

  const roots = [empty];
  for (let i = 0; i < 40; i++) {
    t.put("notes", { body: `note ${i}` });
    roots.push(t.root());
  }
  assert.strictEqual(new Set(roots).size, roots.length, "every write moves the root");
  assert.ok(t.stats().blocks > 40, "nothing is dropped: superseded nodes stay");
  assert.ok(t.stats().bytes > 0);

  // The root is the CONTENTS, not the history: the same records written in a
  // different order give the same hash.
  const a = new sdk.Db(), b = new sdk.Db();
  const schema = { type: "Note", fields: [{ name: "body", kind: "text", required: true }] };
  a.define("notes", schema); b.define("notes", schema);
  assert.strictEqual(a.root(), b.root(), "two empty databases agree");
  // Records get time-ordered ids, so to compare contents both sides must hold
  // the same ids — write in one, then replay the same ids into the other is not
  // expressible through this surface. What IS: the same database read twice.
  const before = a.root();
  a.put("notes", { body: "x" });
  assert.notStrictEqual(a.root(), before, "a write moves it");
  assert.strictEqual(a.root(), a.root(), "and reading it does not");
}

// A record too large for the tree must come back as an ordinary Error the app
// can catch. The wasm build is `panic = abort`, so if the limit breach reached
// the store it would take the whole SDK down — and everything after this point
// would be unreachable rather than failing.
{
  const t = new sdk.Db();
  t.define("notes", { type: "Note", fields: [{ name: "body", kind: "text", required: true }] });
  t.put("notes", { body: "small" });
  const root = t.root(), stats = t.stats();

  const huge = "x".repeat(256 * 1024 + 1024);
  let caught = null;
  try { t.put("notes", { body: huge }); } catch (e) { caught = e; }
  assert.ok(caught instanceof Error, "a limit breach must throw a normal Error");
  assert.ok(!(caught instanceof WebAssembly.RuntimeError), "not a wasm abort");
  assert.match(caught.message, /262144/, "the message names the limit");
  assert.match(caught.message, /file|blob/, "and says what to do instead");

  // Nothing moved, and the database still works.
  assert.strictEqual(t.root(), root);
  assert.deepStrictEqual(t.stats(), stats);
  const after = t.put("notes", { body: "still here" });
  assert.strictEqual(t.count("notes"), 2);
  assert.deepStrictEqual(t.get("notes", after.id).fields, { body: "still here" });
  assert.notStrictEqual(t.root(), root);
}
