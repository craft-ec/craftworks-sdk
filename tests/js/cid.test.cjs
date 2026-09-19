// Checks the JS binding against the same vectors Rust checks.
const fs = require("fs");
const path = require("path");
const assert = require("assert");
const sdk = require(path.join(__dirname, "../../pkg/node/craftworks_sdk.js"));

const lines = fs.readFileSync(path.join(__dirname, "../vectors.txt"), "utf8").trimEnd().split("\n");
let n = 0;
for (const line of lines) {
  const [input, want] = line.split(" ");
  const bytes = Uint8Array.from((input.match(/../g) || []).map((h) => parseInt(h, 16)));
  assert.strictEqual(sdk.cidHex(bytes), want, `input ${input}`);
  n++;
}
assert.ok(n >= 4, `only ${n} vectors`);
assert.match(sdk.version(), /^\d+\.\d+\.\d+$/);
console.log(`ok ${n} vectors, sdk ${sdk.version()}`);
