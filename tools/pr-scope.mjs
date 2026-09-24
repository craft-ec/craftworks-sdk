// WHAT A PR's GATE RUNS (`./gate.sh --pr`): the workspace members its changed files belong to -- THOSE ONLY, never
// their dependents (the owner: "why does it need to run tests not relevant to what it is doing every single time?");
// the batch gate runs everything, and a break it finds is bisected there -- and whether npm must run (JS changed).
// Pure: the changed paths and the metadata in, the plan out, so the scoping is tested without compiling anything.
//
// usage:
//   node tools/pr-scope.mjs plan   <metadata.json> < changed-paths      -> JSON { changed, members, npm, why }
//   node tools/pr-scope.mjs count  <member> <before|-> <after> [--drop-ok]  -> one line; exit 1 on a DROP
import { readFileSync } from "node:fs";
import { relative } from "node:path";

/** Changed paths that are the JavaScript npm tests: the JS, its tests, and the build and tools around them. */
export const NPM_INPUTS = /^(js\/|tests\/js\/|package(-lock)?\.json$|build\.sh$|tools\/|fixture)/;

/** Paths of the ROOT package (the workspace root is also a package): its own sources, not the whole repo. */
const ROOT_SOURCES = /^(src\/|tests\/(?!js\/)|examples\/|benches\/|build\.rs$|Cargo\.toml$)/;

export function plan(metadata, changed) {
  const root = metadata.workspace_root;
  const pkgs = metadata.packages.map(p => ({ name: p.name, dir: relative(root, p.manifest_path.replace(/\/Cargo\.toml$/, "")) }));
  const names = new Set(pkgs.map(p => p.name));
  const rootPkg = pkgs.find(p => p.dir === "")?.name;
  // Deepest member directory first, so `page-io/x` is page-io's and never page's.
  const byDir = pkgs.filter(p => p.dir !== "").sort((a, b) => b.dir.length - a.dir.length);
  const why = [];
  const changedMembers = new Set();
  let npm = false;
  for (const f of changed) {
    if (f === "Cargo.lock") {
      for (const n of names) changedMembers.add(n);
      why.push("Cargo.lock: every member");
      continue;
    }
    const owner = byDir.find(p => f === p.dir || f.startsWith(`${p.dir}/`));
    if (owner) changedMembers.add(owner.name);
    else if (rootPkg && ROOT_SOURCES.test(f)) changedMembers.add(rootPkg);
    if (NPM_INPUTS.test(f)) { npm = true; why.push(`${f}: npm`); }
  }
  const members = [...changedMembers].sort();
  return { changed: members, members, npm, why };
}

/**
 * The cargo target flags that test `member` WITHOUT its batch-only test targets (`skip`): its lib, bins and every
 * other test target, by name. `null` when nothing is skipped (plain `cargo test -p`). Doc-tests cannot be mixed with
 * target flags, so `doc` says whether a separate `--doc` run is needed to count the same tests as the baseline.
 */
export function testArgs(metadata, member, skip) {
  const pkg = metadata.packages.find(p => p.name === member);
  if (!pkg) throw new Error(`no workspace member ${member}`);
  const tests = pkg.targets.filter(t => t.kind.includes("test"));
  if (!tests.some(t => skip.includes(t.name))) return { args: null, doc: false };
  const args = [];
  if (pkg.targets.some(t => t.kind.includes("lib"))) args.push("--lib");
  if (pkg.targets.some(t => t.kind.includes("bin"))) args.push("--bins");
  for (const t of tests) if (!skip.includes(t.name)) args.push("--test", t.name);
  return { args, doc: pkg.targets.some(t => t.kind.includes("lib") && t.doctest !== false) };
}

/** One member's line: `before -> after`, and whether it is a DROP (a test that stopped running). `?` before: the
 * baseline cannot say what this run should count (a batch-only target's own count is not recorded yet). */
export function count(member, before, after, dropOk = false) {
  const a = Number(after);
  if (before === "?") return { line: `${member}: ? -> ${a} (before not comparable: the batch gate has not yet recorded its batch-only targets' own counts)`, drop: false };
  const b = before === "-" || before === "" ? null : Number(before);
  if (b === null) return { line: `${member}: NEW -> ${a}`, drop: false };
  const d = a - b;
  const mark = d === 0 ? "—" : d > 0 ? `+${d}` : `${d}`;
  const drop = d < 0 && !dropOk;
  return { line: `${member}: ${b} -> ${a} (${mark})${d < 0 ? (dropOk ? " DROP, named with --accept-loss" : " DROP: a test stopped running") : ""}`, drop };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [cmd, ...a] = process.argv.slice(2);
  if (cmd === "plan") {
    const metadata = JSON.parse(readFileSync(a[0], "utf8"));
    const changed = readFileSync(0, "utf8").split("\n").map(s => s.trim()).filter(Boolean);
    process.stdout.write(`${JSON.stringify(plan(metadata, changed))}\n`);
  } else if (cmd === "test-args") {
    const metadata = JSON.parse(readFileSync(a[0], "utf8"));
    const r = testArgs(metadata, a[1], a.slice(2));
    process.stdout.write(`${r.args ? r.args.join(" ") : ""}\n${r.doc ? "doc" : ""}\n`);
  } else if (cmd === "count") {
    const r = count(a[0], a[1], a[2], a.includes("--drop-ok"));
    process.stdout.write(`${r.line}\n`);
    process.exit(r.drop ? 1 : 0);
  } else {
    process.stderr.write("usage: pr-scope.mjs plan <metadata.json> < changed | count <member> <before|-> <after> [--drop-ok]\n");
    process.exit(2);
  }
}
