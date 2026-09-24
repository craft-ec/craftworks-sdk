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

await t("**a change to ENGINE tests its reverse dependents too (page, page-io, web, …), and not what engine does not reach**", async () => {
  const r = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "engine/src/lib.rs" });
  assert.equal(r.status, 0, r.stderr);
  const members = planLine(r.stdout, "members tested (with reverse dependents)").split(" ");
  for (const m of ["engine", "page", "page-io", "web", "probe", "testkit", "craftworks-sdk"]) assert.ok(members.includes(m), `${m} not tested: ${members}`);
  for (const m of ["signer", "wire", "contract-keys", "signer-proto", "core-types"]) assert.ok(!members.includes(m), `${m} tested though engine does not reach it: ${members}`);
  assert.equal(planLine(r.stdout, "npm"), "true", "web is in scope, so pkg/ changes and npm runs");
});

await t("**EVERY control is in the plan whatever the PR touches; a README-only PR tests no member and runs no npm**", async () => {
  const r = gate(root, ["--pr", "--dry-run"], { GATE_PR_CHANGED: "README.md" });
  const gateSrc = readFileSync(join(root, "gate.sh"), "utf8");
  const listed = [...gateSrc.matchAll(/^ {2}"([a-z_-]+)\|/gm)].map(m => m[1]);
  assert.ok(listed.length >= 7, `the CONTROLS list read nothing: ${listed}`);
  assert.deepEqual(planLine(r.stdout, "controls").split(" "), listed);
  assert.equal(planLine(r.stdout, "members tested (with reverse dependents)"), "(none)");
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

if (failures) { process.stdout.write(`gate-pr: ${failures} FAILED\n`); process.exit(1); }
process.stdout.write("gate-pr: all ok\n");
