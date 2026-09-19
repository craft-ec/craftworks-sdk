// Checks the JS bindings against the same frozen vectors Rust checks, so a
// binding that mangles bytes on the way in or out cannot pass both.
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const sdk = require(path.join(__dirname, "../../pkg/node/craftworks_sdk.js"));
const { wrap } = require(path.join(__dirname, "../../js/wrap.js"));

const read = (f) =>
  fs.readFileSync(path.join(__dirname, "..", f), "utf8").trimEnd().split("\n");
const bytes = (hex) => Uint8Array.from((hex.match(/../g) || []).map((h) => parseInt(h, 16)));

let n = 0;
for (const line of read("vectors.txt")) {
  const [input, want] = line.split(" ");
  assert.strictEqual(sdk.contentHash(bytes(input)), `hash:${want}`, `input ${input}`);
  n++;
}
assert.ok(n >= 4, `only ${n} content-hash vectors`);

let m = 0;
for (const line of read("block_vectors.txt")) {
  const [tag, input, want] = line.split(" ");
  if (tag !== "raw") continue; // the JS surface hands out ids for app bytes
  assert.strictEqual(sdk.blockId(bytes(input)), want, `input ${input}`);
  m++;
}
assert.ok(m >= 4, `only ${m} block vectors`);

// The two are different ids for the same bytes — the whole point of the split.
const b = bytes("76616c7565"); // "value"
assert.notStrictEqual(sdk.blockId(b).slice(4), sdk.contentHash(b).slice(5));
assert.match(sdk.blockId(b), /^raw:[0-9a-f]{64}$/);
assert.match(sdk.contentHash(b), /^hash:[0-9a-f]{64}$/);

// An app gets block ids. `contentHash` is on the raw module for these vectors
// and is deliberately not on the surface apps are handed.
assert.strictEqual(typeof wrap(sdk).blockId, "function");
assert.strictEqual(wrap(sdk).contentHash, undefined, "apps see only block ids");
assert.strictEqual(wrap(sdk).cidHex, undefined, "the old name is gone");

// Reading an id back: the other half of making one.
const api = wrap(sdk);
for (const line of read("block_vectors.txt")) {
  const want = line.split(" ").pop();
  const got = api.parseBlockId(want);
  assert.strictEqual(got.id, want, "an id must parse back to itself");
  assert.strictEqual(want, `${got.tag}:${got.hex}`, "tag and hex are the whole id");
  assert.match(got.hex, /^[0-9a-f]{64}$/);
  assert.ok(got.tag === "raw" || got.tag === "node", `tag ${got.tag}`);
}

// Everything that is not a block id throws an ordinary Error, and the content
// hash — the mistake this whole split exists to catch — is named in the message.
const hexOnly = sdk.blockId(b).slice(4);
for (const [what, text] of [
  ["a content hash", sdk.contentHash(b)],
  ["bare hex", hexOnly],
  ["empty", ""],
  ["a tag alone", "node:"],
  ["upper case", `node:${hexOnly.toUpperCase()}`],
  ["short", `node:${hexOnly.slice(0, 63)}`],
  ["long", `node:${hexOnly}0`],
  ["a stray space", ` node:${hexOnly}`],
  ["an unknown tag", `sausage:${hexOnly}`],
  ["not an id at all", "hello"],
]) {
  assert.throws(() => api.parseBlockId(text), Error, `${what} must be refused`);
}
assert.throws(() => api.parseBlockId(sdk.contentHash(b)), /content hash/,
  "the error must say which sort it got");

// A root out of a live database is a block id like any other, and the surface
// that produced it can read it back.
const db = new api.Db();
db.define("t", { type: "T", fields: [{ name: "a", kind: "text", required: true }] });
db.put("t", { a: "x" });
const root = api.parseBlockId(db.root());
assert.strictEqual(root.tag, "node", "a tree root is a tree-node block");
assert.strictEqual(root.id, db.root());

assert.match(sdk.version(), /^\d+\.\d+\.\d+$/);
console.log(`ok ${n} content-hash vectors, ${m} block vectors, ids parse back, sdk ${sdk.version()}`);
