// WHAT A PR's GATE RUNS (`./gate.sh --pr`): the workspace members its changed files belong to, PLUS every member that
// depends on one of them (reverse dependents, from `cargo metadata`), and whether npm must run. Pure: the changed
// paths and the metadata in, the plan out, so the scoping is tested without compiling anything.
//
// usage:
//   node tools/pr-scope.mjs plan   <metadata.json> < changed-paths      -> JSON { changed, members, npm, why }
//   node tools/pr-scope.mjs count  <member> <before|-> <after> [--drop-ok]  -> one line; exit 1 on a DROP
import { readFileSync } from "node:fs";
import { relative } from "node:path";

/** The members `build.sh` builds `pkg/` from: a change reaching one of them changes what npm tests. */
export const PKG_MEMBERS = ["web", "signer", "wire", "page"];

/** Changed paths outside every member that npm reads: the JS, its tests, and the build that makes `pkg/`. */
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
  // Reverse dependents, transitively: a member that depends (by any edge) on a changed one.
  const dependents = new Map([...names].map(n => [n, new Set()]));
  for (const p of metadata.packages) {
    for (const d of p.dependencies) {
      const dep = names.has(d.name) ? d.name : null;
      if (dep && dep !== p.name) dependents.get(dep).add(p.name);
    }
  }
  const members = new Set(changedMembers);
  const stack = [...changedMembers];
  while (stack.length) {
    for (const r of dependents.get(stack.pop()) ?? []) if (!members.has(r)) { members.add(r); stack.push(r); }
  }
  for (const m of PKG_MEMBERS) if (members.has(m)) { npm = true; why.push(`${m} in scope: npm (pkg/ is built from it)`); break; }
  return { changed: [...changedMembers].sort(), members: [...members].sort(), npm, why };
}

/** One member's line: `before -> after`, and whether it is a DROP (a test that stopped running). */
export function count(member, before, after, dropOk = false) {
  const b = before === "-" || before === "" ? null : Number(before);
  const a = Number(after);
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
  } else if (cmd === "count") {
    const r = count(a[0], a[1], a[2], a.includes("--drop-ok"));
    process.stdout.write(`${r.line}\n`);
    process.exit(r.drop ? 1 : 0);
  } else {
    process.stderr.write("usage: pr-scope.mjs plan <metadata.json> < changed | count <member> <before|-> <after> [--drop-ok]\n");
    process.exit(2);
  }
}
