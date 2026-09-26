// `npm test`: RUN EVERY JS TEST FILE THAT EXISTS, THEN TALLY.
//
// The directory is the ONE list of test files (CLAUDE.md "Structure before code": one list, read by everyone). The
// `test` script used to be a hand-written `&&` chain, a second list: a new file not added to it never ran and every
// gate stayed green (keep-tab.test.mjs), and the first red file hid every file after it (builder#70). Now every
// `*.test.{mjs,cjs,js}` under tests/js, at any depth, runs because it exists, in path order, each whatever happened
// before it, and the verdict comes at the end with every file named.
//
// Sequential on purpose: several tests start nodes or browsers, and side by side on a loaded machine their timing
// waits would become the thing under test.
//
// The tally prints PASS/FAILED, never "  ok ": gate.sh counts `^  ok ` lines as the tests that passed, and every
// review grep is `FAILED|panicked|REFUSED`, which a bare "FAIL" would slip past.
import { spawnSync } from "node:child_process";
import { readdirSync, statSync } from "node:fs";
import { join, relative } from "node:path";

export const TEST_FILE = /\.test\.(mjs|cjs|js)$/;

/** Every test file under `dir`, at any depth, sorted by path. */
export function testFiles(dir) {
  const out = [];
  const walk = d => {
    for (const f of readdirSync(d).sort()) {
      const p = join(d, f);
      if (statSync(p).isDirectory()) walk(p);
      else if (TEST_FILE.test(f)) out.push(p);
    }
  };
  walk(dir);
  return out;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const dir = process.argv[2] ?? "tests/js";
  const files = testFiles(dir);
  if (files.length === 0) {
    console.error(`run-tests: no test files under ${dir}: a run that checked nothing is not a pass`);
    process.exit(1);
  }
  const results = [];
  for (const f of files) {
    const t0 = Date.now();
    const r = spawnSync(process.execPath, [f], { stdio: "inherit" });
    results.push({ f: relative(process.cwd(), f), rc: r.status ?? (r.signal ? `signal ${r.signal}` : "?"), s: ((Date.now() - t0) / 1000).toFixed(1) });
  }
  const failed = results.filter(r => r.rc !== 0);
  console.log(`\n── ${files.length} test files: ${files.length - failed.length} passed, ${failed.length} failed ──`);
  for (const r of results) console.log(`${r.rc === 0 ? "PASS" : "FAILED"} ${r.f}  (${r.s} s${r.rc === 0 ? "" : `, rc ${r.rc}`})`);
  process.exit(failed.length ? 1 : 0);
}
