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
