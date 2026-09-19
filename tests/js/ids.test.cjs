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

assert.match(sdk.version(), /^\d+\.\d+\.\d+$/);
console.log(`ok ${n} content-hash vectors, ${m} block vectors, sdk ${sdk.version()}`);
