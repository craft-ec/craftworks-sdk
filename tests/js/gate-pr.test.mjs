// `./gate.sh --pr`, the one command every PR runs: every structural control whatever the PR touches; the changed
// members PLUS their reverse dependents (tools/pr-scope.mjs, from cargo metadata); a count line per member, a DROP
// failing; and a target shared with another worktree refused.
import assert from "node:assert/strict";
import { spawnSync, execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { count } from "../../tools/pr-scope.mjs";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};
const root = new URL("../../", import.meta.url).pathname;
const gate = (cwd, args, env = {}) => spawnSync("./gate.sh", args, { cwd, encoding: "utf8", env: { ...process.env, ...env } });
const planLine = (out, what) => (out.split("\n").find(l => l.startsWith(`gate --pr: ${what}:`)) ?? "").split(": ").slice(2).join(": ").trim();

await t("**a change to ENGINE tests ENGINE only** (the owner: dependents run at the batch gate), and no npm", async () => {
  const r = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "engine/src/lib.rs" });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(planLine(r.stdout, "members tested (changed only; dependents run at the batch gate)"), "engine");
  assert.equal(planLine(r.stdout, "npm"), "false", "no JS changed, so no npm");
  const two = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "page-io/src/lib.rs page/src/lib.rs" });
  assert.equal(planLine(two.stdout, "members tested (changed only; dependents run at the batch gate)"), "page page-io",
    "page-io/x was taken for page's, or a member was missed");
});

await t("**a SLOW test is batch-only: --pr skips it BY NAME and says so; the batch gate runs it with the full model seeds**", async () => {
  const { testArgs } = await import("../../tools/pr-scope.mjs");
  const meta = JSON.parse(execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }));
  const page = testArgs(meta, "page", ["model"]);
  assert.ok(!page.args.includes("model"), `model was not skipped: ${page.args}`);
  assert.ok(page.args.includes("--lib") && page.args.includes("race_get"), `page's other tests were dropped: ${page.args}`);
  assert.equal(page.doc, true, "page's doc-tests would not be counted");
  assert.deepEqual(testArgs(meta, "engine", ["model"]), { args: null, doc: false }, "nothing to skip in engine, yet flags");
  const pr = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "page/src/lib.rs" });
  assert.equal(planLine(pr.stdout, "batch-only (skipped here, run by the batch gate)"), "page@model");
  const batch = gate(root, ["--dry-run"], { CRAFTWORKS_MODEL_SEEDS: "" });
  assert.match(batch.stdout, /gate: model seeds 40 \(CRAFTWORKS_MODEL_SEEDS\)/, "the batch gate does not state the full seed count");
  assert.match(batch.stdout, /gate: batch-only targets, run here: page@model/);
});

await t("**EVERY control is in the plan whatever the PR touches; a README-only PR tests no member and runs no npm**", async () => {
  const r = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "README.md" });
  const gateSrc = readFileSync(join(root, "gate.sh"), "utf8");
  const listed = [...gateSrc.matchAll(/^ {2}"([a-z0-9_-]+)\|/gm)].map(m => m[1]);
  assert.ok(listed.length >= 7, `the CONTROLS list read nothing: ${listed}`);
  assert.deepEqual(planLine(r.stdout, "controls").split(" "), listed);
  assert.equal(planLine(r.stdout, "members tested (changed only; dependents run at the batch gate)"), "(none)");
  assert.equal(planLine(r.stdout, "npm"), "false");
  const js = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "js/session.js" });
  assert.equal(planLine(js.stdout, "npm"), "true", "a JS change did not run npm");
});

await t("**a DELETED test shows as a count DROP and fails; named with --accept-loss it is shown, not failed**", async () => {
  assert.deepEqual(count("engine", "120", "119"), { line: "engine: 120 -> 119 (-1) DROP: a test stopped running", drop: true });
  assert.equal(count("engine", "120", "119", true).drop, false);
  assert.match(count("engine", "120", "119", true).line, /DROP, named with --accept-loss/);
  assert.deepEqual(count("engine", "120", "121"), { line: "engine: 120 -> 121 (+1)", drop: false });
  assert.deepEqual(count("pieces", "-", "5"), { line: "pieces: NEW -> 5", drop: false });
  const r = spawnSync("node", [join(root, "tools/pr-scope.mjs"), "count", "web", "30", "29"], { encoding: "utf8" });
  assert.equal(r.status, 1, "a drop did not fail the command");
});

await t("**a target claimed by ANOTHER worktree is refused before anything builds**", async () => {
  const target = mkdtempSync(join(tmpdir(), "gate-pr-target-"));
  writeFileSync(join(target, ".craftworks-worktree"), "/some/other/worktree\n");
  const r = gate(root, ["--controls"], { CARGO_TARGET_DIR: target });
  rmSync(target, { recursive: true, force: true });
  assert.notEqual(r.status, 0, "a shared target was used");
  assert.match(r.stderr, /belongs to \/some\/other\/worktree, another worktree/);
});

await t("**a control violation PLANTED in a crate the PR does not touch fails the PR** (its own worktree and target)", async () => {
  const scratch = mkdtempSync(join(tmpdir(), "gate-pr-plant-"));
  const wt = join(scratch, "wt");
  const target = join(scratch, "target");
  execFileSync("git", ["-C", root, "worktree", "add", "--detach", wt, "HEAD"], { stdio: "ignore" });
  try {
    // The gate AS IT IS in this tree (a PR runs the gate on its own branch, committed or not).
    for (const f of ["gate.sh", "tools/pr-scope.mjs", "tools/target-dir.sh"]) writeFileSync(join(wt, f), readFileSync(join(root, f)));
    const env = { GATE_PR_CHANGED: "README.md", GATE_CONTROLS_ONLY: "one_home", CARGO_TARGET_DIR: target };
    const clean = gate(wt, ["--pr"], env);
    assert.equal(clean.status, 0, `THE CONTROL: the clean tree failed:\n${clean.stdout}\n${clean.stderr}`);
    assert.match(clean.stdout, /control one_home: ok \(\d+ test/);
    const f = join(wt, "wire/src/lib.rs");
    writeFileSync(f, `${readFileSync(f, "utf8")}\npub fn planted(b: u8) -> String { format!("{b:02x}") }\n`);
    const planted = gate(wt, ["--pr"], env);
    assert.notEqual(planted.status, 0, "a hand-written hex in wire/ (untouched by the PR) passed");
    assert.match(planted.stderr, /control one_home FAILED/);
  } finally {
    execFileSync("git", ["-C", root, "worktree", "remove", "--force", wt], { stdio: "ignore" });
    rmSync(scratch, { recursive: true, force: true });
  }
});

await t("**GATE_OWNERS_PER_PR is the batch gate's alone** (sdk#420): refused in --pr and a bare full run (both as --dry-run, so a mutant that HONOURS the flag ends fast instead of running a gate), before any work, naming the flag; a non-count refused even with --accept", async () => {
  for (const args of [["--pr", "--dry-run"], ["--dry-run"]]) {
    const r = gate(root, args, { GATE_OWNERS_PER_PR: "1" });
    assert.notEqual(r.status, 0, `honoured with ${JSON.stringify(args)}: a stray export would switch the owners control off`);
    assert.match(r.stderr, /GATE_OWNERS_PER_PR is only for the batch gate/);
  }
  const bad = gate(root, ["--accept"], { GATE_OWNERS_PER_PR: "x" });
  assert.notEqual(bad.status, 0);
  assert.match(bad.stderr, /must be a PR count, got 'x'/);
});

await t("**GATE_OWNERS_PER_PR is read once and UNSET before the gate starts anything** (batch 3: an inner gate.sh inherited it, refused, and npm lost 39 tests)", async () => {
  const src = readFileSync(new URL("../../gate.sh", import.meta.url), "utf8");
  const read = src.indexOf("OWNERS_PER_PR=${GATE_OWNERS_PER_PR:-}");
  const unset = src.indexOf("unset GATE_OWNERS_PER_PR");
  const firstChild = Math.min(...["cargo ", "npm ", "node "].map(w => { const i = src.indexOf("\n" + w); return i < 0 ? Infinity : i; }), src.indexOf("$guard"));
  assert.ok(read > 0, "the flag is not read into a local");
  assert.ok(unset > read, "the flag is not unset after it is read: a child gate.sh would inherit it");
  assert.ok(unset < firstChild, "the flag is unset only after the gate has started a child");
  const uses = [...src.matchAll(/GATE_OWNERS_PER_PR/g)].length;
  assert.ok(!/\$\{?GATE_OWNERS_PER_PR/.test(src.slice(unset)), "the environment variable is read again after it was unset");
  assert.ok(uses >= 3, `the scan found ${uses} mentions: it read nothing`);
});

if (failures) { process.stdout.write(`gate-pr: ${failures} FAILED\n`); process.exit(1); }
process.stdout.write("gate-pr: all ok\n");
