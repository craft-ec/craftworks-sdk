// ONE FETCH, ONE RULE (#126 ruling): a second copy of either is the defect,
// so the tree refuses one.
//
//   1. `fetch…(` — a call of `fetch`, `fetchWith`, … — anywhere but the ONE
//      fetch (`served` in js/artefacts.js). A second fetch grows its own
//      give-up; every copy found had one (`!r.ok → throw`).
//   2. a validation regex literal `/^[…` — an id rule written again. The SDK's
//      rules live in Rust (`app::check`, the id parses), exported to JS as
//      `sdk.ids`; the SDK's own JS has no place for one.
//   3. `import(` of anything but a string literal: a dynamic import of a
//      computed URL is a fetch path too.
//
// Over the SDK's non-test JavaScript (`git ls-files js/*.js`). Run by `npm
// test` (tests/js/one-rule-gate.test.mjs), which also proves each check
// fires on a planted copy.
//
//   node tools/one-rule-gate.mjs [root]   exit 1 naming every violation
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join } from "node:path";

/** The ONE place each thing may be: path relative to the root. */
export const OWNERS = { fetch: ["js/artefacts.js"], rule: [] };

const CHECKS = [
  { kind: "fetch", re: /\bfetch\w*\s*\(/, says: "a fetch outside the one fetch (served, js/artefacts.js)" },
  { kind: "rule", re: /\/\^\[/, says: "a validation regex: an id rule written again (use sdk.ids)" },
  { kind: "import", re: /\bimport\s*\(\s*(?!["'`][^"'`$]*["'`]\s*\))/, says: "a dynamic import of a computed URL (a second fetch path)" },
];

/** Every violation in `files` (paths relative to `root`), as `path:line: says`. */
export function violations(root, files) {
  const out = [];
  for (const f of files) {
    const lines = readFileSync(join(root, f), "utf8").split("\n");
    lines.forEach((line, i) => {
      // A comment is not code: a doc naming `fetch(` is not a second fetch.
      const code = line.replace(/\/\/.*$/, "").trim();
      if (code.startsWith("*") || code.startsWith("/*")) return;
      for (const c of CHECKS) {
        if (c.kind !== "import" && (OWNERS[c.kind] ?? []).includes(f)) continue;
        if (c.re.test(code)) out.push(`${f}:${i + 1}: ${c.says}: ${line.trim()}`);
      }
    });
  }
  return out;
}

/** The files the gate reads: the SDK's shipped JavaScript, as git tracks it. */
export function scope(root) {
  return execFileSync("git", ["ls-files", "js/*.js"], { cwd: root, encoding: "utf8" }).split("\n").filter(Boolean);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const root = process.argv[2] ?? ".";
  const files = scope(root);
  if (files.length === 0) {
    console.log("one-rule-gate: no files in scope — nothing was checked");
    process.exit(1);
  }
  const v = violations(root, files);
  for (const x of v) console.log(x);
  console.log(`one-rule-gate: ${files.length} file(s), ${v.length} violation(s)`);
  process.exit(v.length ? 1 : 0);
}
