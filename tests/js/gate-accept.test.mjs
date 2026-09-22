// THE RATCHET REFUSES what a partial or shrinking run would write (sdk#159).
//
// `./gate.sh --accept` wrote `npm=14` over 146 after a suite stopped partway —
// the same run printed `npm LOST 92` — and the next honest run then read as
// "counts moved UP". `tools/gate-accept.sh` is now the baseline's one writer;
// this drives it with /bin/bash (macOS ships 3.2, which the gate must run on)
// and checks the gate still hands it the step-failure flag.
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const helper = join(root, "tools", "gate-accept.sh");
let failures = 0;
const t = (name, fn) => {
  try { fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const sha = p => createHash("sha256").update(readFileSync(p)).digest("hex");

/** A baseline and a run's counts in a fresh dir; run the helper; report. */
function accept(baseline, counts, failed = 0, extra = []) {
  const dir = mkdtempSync(join(tmpdir(), "gate-accept-"));
  const b = join(dir, "gate.baseline"), c = join(dir, "counts");
  writeFileSync(b, baseline);
  writeFileSync(c, counts);
  const before = sha(b);
  const r = spawnSync("/bin/bash", [helper, b, String(failed), c, ...extra], { encoding: "utf8" });
  return { rc: r.status, err: r.stderr, unchanged: sha(b) === before, now: readFileSync(b, "utf8") };
}
const BASE = "craftworks-sdk=221\nengine=78\nnpm=146\n";

t("**a baseline that CANNOT be written is a failure naming it — never 'recorded' (sdk#252)**", () => {
  const dir = mkdtempSync(join(tmpdir(), "gate-accept-ro-"));
  const b = join(dir, "gate.baseline"), c = join(tmpdir(), `gate-accept-counts-${process.pid}`);
  writeFileSync(b, BASE); writeFileSync(c, "craftworks-sdk=222\nengine=78\nnpm=146\n");
  chmodSync(dir, 0o555); // the directory refuses the rename, as a full disk refuses the write
  try {
    const r = spawnSync("/bin/bash", [helper, b, "0", c], { encoding: "utf8" });
    assert.notEqual(r.status, 0, `a write that failed exited 0: ${r.stdout}${r.stderr}`);
    assert.match(r.stderr, /FAILED to write .*gate\.baseline/);
    assert.doesNotMatch(r.stdout, /recorded/, "it said 'recorded' about counts it did not record");
    assert.equal(readFileSync(b, "utf8"), BASE, "the baseline changed");
  } finally { chmodSync(dir, 0o755); }
});

t("**free space is checked again RIGHT BEFORE writing: under the floor, refused, byte-identical**", () => {
  const dir = mkdtempSync(join(tmpdir(), "gate-accept-"));
  const b = join(dir, "gate.baseline"), c = join(dir, "counts");
  writeFileSync(b, BASE); writeFileSync(c, "craftworks-sdk=222\nengine=78\nnpm=146\n");
  const r = spawnSync("/bin/bash", [helper, b, "0", c], { encoding: "utf8", env: { ...process.env, GATE_MIN_GIB: "5", GATE_FREE_GIB_FOR_TEST: "2" } });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /2 GiB free, under 5 GiB, right before writing/);
  assert.equal(readFileSync(b, "utf8"), BASE);
});

t("THE CONTROL: a writable baseline with room is recorded and reads back identical", () => {
  const r = accept(BASE, "craftworks-sdk=222\nengine=78\nnpm=146\n");
  assert.equal(r.rc, 0, r.err);
  assert.equal(r.now, "craftworks-sdk=222\nengine=78\nnpm=146\n");
});

t("**a run in which a STEP FAILED is refused, and the baseline is byte-identical**", () => {
  // Counts that only RISE, so the loss check cannot be what refuses it: the
  // step-failure guard alone has to. (With npm=14 here, removing that guard
  // stayed refused — two guards on one property hide each other.)
  const r = accept(BASE, "craftworks-sdk=230\nengine=80\nnpm=150\n", 1);
  assert.notEqual(r.rc, 0);
  assert.ok(r.unchanged, "the baseline was rewritten by a failed run");
  assert.match(r.err, /a step of this run FAILED/);
});

t("THE CONTROL: a green run that RAISES a count is recorded", () => {
  const r = accept(BASE, "craftworks-sdk=224\nengine=78\nnpm=146\n");
  assert.equal(r.rc, 0, r.err);
  assert.equal(r.now, "craftworks-sdk=224\nengine=78\nnpm=146\n");
});

t("**a green run that LOWERS a count is refused without an override, byte-identical**", () => {
  const r = accept(BASE, "craftworks-sdk=221\nengine=78\nnpm=140\n");
  assert.notEqual(r.rc, 0);
  assert.ok(r.unchanged);
  assert.match(r.err, /npm would fall 146 -> 140 \(6 test\(s\) lost\)/);
});

t("a lowered count NAMED with --accept-loss is recorded, and what was lost is printed", () => {
  const r = accept(BASE, "craftworks-sdk=221\nengine=78\nnpm=140\n", 0, ["--accept-loss", "npm"]);
  assert.equal(r.rc, 0, r.err);
  assert.match(r.err, /npm LOWERED 146 -> 140: 6 test\(s\) given up/);
  assert.match(r.now, /^npm=140$/m);
});

t("an --accept-loss naming a member that did NOT fall is refused: it would license the next loss", () => {
  const r = accept(BASE, "craftworks-sdk=222\nengine=78\nnpm=146\n", 0, ["--accept-loss", "engine"]);
  assert.notEqual(r.rc, 0);
  assert.ok(r.unchanged);
});

t("a run with NO counts is refused, not recorded as an empty baseline", () => {
  const r = accept(BASE, "");
  assert.notEqual(r.rc, 0);
  assert.ok(r.unchanged);
});

t("a NEW member is recorded; one no longer in the run is dropped", () => {
  const r = accept(BASE, "craftworks-sdk=221\nnpm=146\nwire=33\n", 0, []);
  assert.equal(r.rc, 0, r.err);
  assert.equal(r.now, "craftworks-sdk=221\nnpm=146\nwire=33\n");
});

// The call site still ASKS (a helper nothing calls has cost this repo more
// than any other shape): gate.sh routes --accept through the helper, with the
// step-failure flag, and every step's failure sets it.
t("**gate.sh hands --accept to the helper WITH the step-failure flag, and writes the baseline nowhere else**", () => {
  const src = readFileSync(join(root, "gate.sh"), "utf8");
  assert.match(src, /\.\/tools\/gate-accept\.sh "\$BASELINE" "\$STEP_FAILED" "\$counts_file"/);
  assert.doesNotMatch(src, />>?\s*"\$BASELINE"/, "gate.sh writes the baseline directly again");
  const stepFails = (src.match(/^\s*.*\bstep_fail "/gm) ?? []).length;
  assert.ok(stepFails >= 8, `only ${stepFails} step failures set STEP_FAILED`);
  // THE CONTROL: the reader does find the other kind of failure — a count
  // that moved is a `fail`, not a `step_fail`, or --accept could never raise.
  assert.match(src, /\n\s+fail "\$m LOST/);
});

t("THE CONTROL: the helper really runs under /bin/bash (3.2 on macOS)", () => {
  const v = execFileSync("/bin/bash", ["-c", "echo $BASH_VERSION"], { encoding: "utf8" }).trim();
  assert.ok(v.length > 0);
});

if (failures) { console.log(`\ngate accept: ${failures} FAILED`); process.exit(1); }
console.log("\ngate accept: all ok");
