// TESTS THAT NEVER RAN (sdk#475). A member's count is the sum of its targets, so a target that compiles to NOTHING
// natively -- `web` is `#![cfg(target_arch = "wasm32")]`, and a `#[cfg(test)]` module in its src/ ran 0 tests,
// green -- hides behind the other targets' tests: the member's `before -> after` never moved. Two checks, both over
// what cargo actually printed:
//   silent:  a test TARGET whose source declares a `#[test]` and which ran 0 tests (both gate modes);
//   unran:   a test function a PR ADDS that cargo never listed as run (`--pr`), which also catches a test behind a
//            cfg inside a target that runs others. An `#[ignore]`d test is listed (`... ignored`), so it is not
//            unran, but it executes nothing, so it is NAMED ("added but IGNORED"), never silent. An added
//            `#[wasm_bindgen_test]` FAILS: nothing in this gate runs them (no wasm test runner), so it would never run.
// Pure: cargo's output, the member's files and the diff in, the names out, so both are tested without compiling.
//
// usage:
//   node tools/test-targets.mjs silent <member-dir> < cargo-test-output   -> one line per silent target; exit 1 if any
//   node tools/test-targets.mjs unran  <cargo-test-output-file> < diff    -> one line per added test not run; exit 1 if any
//                                                                          (none: `added tests: N, all ran`)
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

/** A test attribute: `#[test]`, `#[tokio::test]`, `#[tokio::test(flavor = …)]`. */
const TEST_ATTR = /#\[\s*(?:[A-Za-z_][\w]*::)*test\s*(?:\(|\])/;
/** `#[wasm_bindgen_test]`: never run natively, and this gate has no wasm test runner, so an added one never runs. */
const WASM_TEST_ATTR = /#\[\s*(?:wasm_bindgen_test::)?wasm_bindgen_test\s*(?:\(|\])/;

/** Every `.rs` file under `p` (a file or a directory). */
function rustFiles(p) {
  if (!existsSync(p)) return [];
  if (statSync(p).isFile()) return p.endsWith(".rs") ? [p] : [];
  return readdirSync(p).flatMap(f => rustFiles(join(p, f)));
}

/** The targets cargo ran for one member and how many tests each ran: `Running <src> (…)` then `running N tests`. */
export function targetsRun(out) {
  const targets = [];
  let cur = null;
  for (const line of out.split("\n")) {
    const m = line.match(/^\s*Running (?:unittests )?(\S+)/);
    if (m) { cur = { src: m[1], ran: null }; targets.push(cur); continue; }
    if (/^\s*Doc-tests /.test(line)) { cur = null; continue; }
    const r = line.match(/^running (\d+) tests?$/);
    if (r && cur && cur.ran === null) cur.ran = Number(r[1]);
  }
  return targets;
}

/** The source a target's tests live in: a lib/bin unit-test target is the member's whole src/; `tests/x.rs` is that
 * file plus `tests/x/` (its modules). */
function sourcesOf(memberDir, src) {
  if (/^src\//.test(src)) return rustFiles(join(memberDir, "src"));
  const file = join(memberDir, src);
  return [...rustFiles(file), ...rustFiles(file.replace(/\.rs$/, ""))];
}

/** The targets whose source declares a test and which ran 0 tests: `[{ src, file }]`, `file` being the first
 * declaring file. `read` is injectable for the tests. */
export function silentTargets(out, memberDir, read = f => readFileSync(f, "utf8")) {
  const silent = [];
  for (const t of targetsRun(out)) {
    if (t.ran !== 0) continue;
    const file = sourcesOf(memberDir, t.src).find(f => TEST_ATTR.test(read(f)));
    if (file) silent.push({ src: t.src, file });
  }
  return silent;
}

/** The test functions a unified diff ADDS: an added test attribute, then the next `fn name` (added or context: an
 * attribute added to an existing fn counts too). `[{ file, name }]`. */
export function addedTests(diff, attr = TEST_ATTR) {
  const added = [];
  let file = null;
  let pending = false;
  for (const line of diff.split("\n")) {
    const f = line.match(/^\+\+\+ (?:b\/)?(.+)$/);
    if (f) { file = f[1]; pending = false; continue; }
    if (line.startsWith("---") || line.startsWith("@@") || line.startsWith("diff ")) continue;
    if (line.startsWith("-")) continue;
    const body = line.slice(1);
    if (line.startsWith("+") && attr.test(body)) { pending = true; continue; }
    if (!pending) continue;
    const fn = body.match(/^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)/);
    if (fn) { added.push({ file, name: fn[1] }); pending = false; continue; }
    // Other attributes and blank lines between the test attribute and its fn keep it pending; anything else ends it.
    if (!/^\s*(#\[|\/\/|$)/.test(body)) pending = false;
  }
  return added;
}

/** What cargo listed, by fn name: `test path::name ... ok|FAILED|ignored` -> name -> the status word. */
function listed(out) {
  const ran = new Map();
  for (const line of out.split("\n")) {
    const m = line.match(/^test (\S+) \.\.\. (\S+)/);
    if (m) ran.set(m[1].split("::").pop(), m[2]);
  }
  return ran;
}

/** The added tests cargo never listed as run (`test path::name ... ok|FAILED|ignored`). */
export function unranTests(diff, out) {
  const ran = listed(out);
  return addedTests(diff).filter(t => !ran.has(t.name));
}

/** The added tests cargo listed as `ignored`: not unran, but they execute nothing, so the gate names them. */
export function ignoredTests(diff, out) {
  const ran = listed(out);
  return addedTests(diff).filter(t => ran.get(t.name) === "ignored");
}

/** The added `#[wasm_bindgen_test]`s: each one fails, since no runner in this gate would ever run it. */
export function addedWasmTests(diff) {
  return addedTests(diff, WASM_TEST_ATTR);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [cmd, arg] = process.argv.slice(2);
  if (cmd === "silent") {
    const s = silentTargets(readFileSync(0, "utf8"), arg);
    for (const t of s) process.stdout.write(`${t.src} ran 0 tests, but ${t.file} declares a #[test]\n`);
    process.exit(s.length ? 1 : 0);
  } else if (cmd === "unran") {
    const diff = readFileSync(0, "utf8");
    const out = readFileSync(arg, "utf8");
    const u = unranTests(diff, out);
    const w = addedWasmTests(diff);
    for (const t of u) process.stdout.write(`${t.file}: added test \`${t.name}\` never ran\n`);
    for (const t of w) process.stdout.write(`${t.file}: added #[wasm_bindgen_test] \`${t.name}\`: no runner for wasm_bindgen_test in this gate\n`);
    if (!u.length && !w.length) process.stdout.write(`added tests: ${addedTests(diff).length}, all ran\n`);
    for (const t of ignoredTests(diff, out)) process.stdout.write(`added but IGNORED: ${t.name} (${t.file})\n`);
    process.exit(u.length || w.length ? 1 : 0);
  } else {
    process.stderr.write("usage: test-targets.mjs silent <member-dir> < out | unran <out-file> < diff\n");
    process.exit(2);
  }
}
