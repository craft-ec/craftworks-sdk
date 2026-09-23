// THE DUPLICATE-CODE GATE HAS TO FIRE (craftworks-sdk#320). Each arm runs the
// real tool (jscpd, pinned) over a planted tree, so a gate that looks at
// nothing, or keys clones by line numbers, fails here.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const GATE = fileURLToPath(new URL("../../tools/dup-gate.mjs", import.meta.url));
let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${String(e.message).split("\n").join("\n    ")}\n`); }
};
const run = (root, ...args) => {
  try { return { code: 0, out: execFileSync("node", [GATE, "--root", root, ...args], { encoding: "utf8" }) }; }
  catch (e) { return { code: e.status, out: String(e.stdout) }; }
};
/** A function long enough to be a clone at 50 tokens, distinct per `tag`. */
const fnText = (name, tag) => `pub fn ${name}(xs: &[u64]) -> u64 {
    let mut total = 0u64;
    for (i, x) in xs.iter().enumerate() {
        if i % 2 == 0 { total = total.wrapping_add(*x * ${tag}); } else { total = total.wrapping_sub(*x); }
        if total > 1_000_000 { total /= 3; }
    }
    let mut best = 0u64;
    for x in xs { if *x > best { best = *x; } }
    total ^ best ^ ${tag}
}
`;
const tree = files => {
  const root = mkdtempSync(join(tmpdir(), "dup-gate-test-"));
  mkdirSync(join(root, "src"));
  writeFileSync(join(root, "dup-gate.json"), JSON.stringify({ dirs: ["src"], formats: "rust:rs", ignore: [], minTokens: 50, baseline: "dup-baseline.json" }));
  for (const [f, text] of Object.entries(files)) writeFileSync(join(root, "src", f), text);
  return root;
};

await t("THE CONTROL: two DIFFERENT functions are not a duplicate", async () => {
  const root = tree({ "a.rs": fnText("alpha", 3), "b.rs": fnText("beta", 1).replace(/wrapping_add/g, "saturating_add").replace(/total \/= 3/, "total >>= 2").replace("best = *x", "best = best.max(*x) + 1") });
  const r = run(root);
  rmSync(root, { recursive: true, force: true });
  assert.equal(r.code, 0, r.out);
  assert.match(r.out, /0 clone\(s\).*0 NEW/);
});

await t("**a planted COPY of a function is red, and names both places**", async () => {
  const root = tree({ "a.rs": fnText("alpha", 3), "b.rs": "// a copy\n" + fnText("alpha", 3) });
  const r = run(root);
  rmSync(root, { recursive: true, force: true });
  assert.equal(r.code, 1, `a copied function passed the gate:\n${r.out}`);
  assert.match(r.out, /NEW DUPLICATE .*src\/a\.rs:\d+-\d+ +== +src\/b\.rs:\d+-\d+|NEW DUPLICATE .*src\/b\.rs:\d+-\d+ +== +src\/a\.rs:\d+-\d+/, r.out);
});

await t("a KNOWN duplicate (in the baseline) passes, even after it MOVES within its files", async () => {
  const root = tree({ "a.rs": fnText("alpha", 3), "b.rs": fnText("alpha", 3) });
  assert.equal(run(root, "--write-baseline").code, 0);
  writeFileSync(join(root, "src", "b.rs"), "// moved down\n\n\n\n" + fnText("alpha", 3));
  const r = run(root);
  assert.equal(r.code, 0, `a known clone that moved was reported new (keyed by line?):\n${r.out}`);
  // …and a THIRD copy elsewhere is new.
  writeFileSync(join(root, "src", "c.rs"), fnText("alpha", 3));
  const r3 = run(root);
  rmSync(root, { recursive: true, force: true });
  assert.equal(r3.code, 1, `a third copy passed because its text was known:\n${r3.out}`);
});

await t("a gate that scanned NOTHING could not check: exit 2, never a pass", async () => {
  const root = tree({});
  const r = run(root);
  rmSync(root, { recursive: true, force: true });
  assert.equal(r.code, 2, r.out);
  assert.match(r.out, /COULD NOT CHECK/);
});

if (failures) { console.log(`${failures} failing`); process.exit(1); }
