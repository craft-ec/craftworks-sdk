// WHAT A PR's GATE RUNS (`./gate.sh --pr`): the workspace members its changed files belong to -- THOSE ONLY, never
// their dependents (the owner: "why does it need to run tests not relevant to what it is doing every single time?");
// the batch gate runs everything, and a break it finds is bisected there -- and whether npm must run (JS changed, or a
// member compiled into the wasm package the JS tests load, sdk#537).
// Pure: the changed paths and the metadata in, the plan out, so the scoping is tested without compiling anything.
//
// usage:
//   node tools/pr-scope.mjs plan   <metadata.json> < changed-paths      -> JSON { changed, members, npm, why }
//   node tools/pr-scope.mjs count  <member> <before|-> <after> [--drop-ok]  -> one line; exit 1 on a DROP
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join, relative } from "node:path";

/** Changed paths that are the JavaScript npm tests: the JS, its tests, and the build and tools around them. */
export const NPM_INPUTS = /^(js\/|tests\/js\/|package(-lock)?\.json$|build\.sh$|tools\/|fixture)/;

/** Paths of the ROOT package (the workspace root is also a package): its own sources, not the whole repo. */
const ROOT_SOURCES = /^(src\/|tests\/(?!js\/)|examples\/|benches\/|build\.rs$|Cargo\.toml$)/;

/** The crates build.sh compiles to wasm for the package the JS tests load: READ from build.sh, never listed here. */
export function wasmRoots(buildSh) {
  return [...buildSh.matchAll(/cargo build\b[^\n]*?-p\s+([\w-]+)[^\n]*--target\s+wasm32/g)].map(m => m[1]);
}

/** Every workspace member `roots` depend on, themselves included: normal and build dependencies, by path, transitively. */
export function closure(metadata, roots) {
  const byName = new Map(metadata.packages.map(p => [p.name, p]));
  const seen = new Set();
  const walk = n => {
    if (seen.has(n) || !byName.has(n)) return;
    seen.add(n);
    for (const d of byName.get(n).dependencies) if (d.path && d.kind !== "dev") walk(d.name);
  };
  roots.forEach(walk);
  return seen;
}

/**
 * `shipped`: the members whose code is IN the wasm package (sdk#537). A change to one of them changes what the JS tests
 * load, so npm runs -- a Rust-only PR is not a JS-free PR (#522's finality rule turned two JS tests red, unseen).
 */
export function plan(metadata, changed, shipped = new Set()) {
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
  for (const m of members) if (shipped.has(m)) { npm = true; why.push(`${m}: in the wasm package the JS tests load`); }
  return { changed: members, members, npm, why };
}

/** THE MODEL TARGETS (sdk#536), derived, never listed: a member's test targets named `model` or `*_model`. */
export const MODEL_TARGET = /(^|_)model$/;

export function modelTargets(metadata, member) {
  const pkg = metadata.packages.find(p => p.name === member);
  if (!pkg) throw new Error(`no workspace member ${member}`);
  return pkg.targets.filter(t => t.kind.includes("test") && MODEL_TARGET.test(t.name)).map(t => t.name).sort();
}

/** What makes a test a SEEDED MODEL: it walks seeds through the one runner, or reads the seed knob itself. */
export const SEEDED = /testkit::model|\bmodel::(run|until|share)\b|CRAFTWORKS_MODEL_SEEDS?\b/;

/**
 * THE CONTROL on the convention: every test target whose source is a seeded model (`SEEDED`) is NAMED a model, so
 * `modelTargets` finds it and both gates run it in `--profile model`. A seeded model under another name would run in
 * DEBUG (~40x slower) on every PR, unnoticed: that is the failure this names. `read(path)` reads a source file.
 */
export function misnamedModels(metadata, read) {
  const out = [];
  for (const pkg of metadata.packages) {
    for (const t of pkg.targets) {
      if (!t.kind.includes("test") || MODEL_TARGET.test(t.name)) continue;
      if (SEEDED.test(read(t.src_path))) out.push(`${pkg.name}: test target \`${t.name}\` (${t.src_path}) is a seeded model not named \`model\` or \`*_model\``);
    }
  }
  return out;
}

/**
 * The cargo target flags that test `member` WITHOUT its model test targets (`skip`): its lib, bins and every
 * other test target, by name. `null` when nothing is skipped (plain `cargo test -p`). Doc-tests cannot be mixed with
 * target flags, so `doc` says whether a separate `--doc` run is needed to count the same tests as the baseline.
 */
/** A library target, whatever crate type it declares: `lib`, `rlib`, `cdylib`, ... (the root crate is an `rlib`: a
 * test for `lib` alone dropped its 14 unit tests from every PR's count). */
const isLib = t => t.kind.some(k => k === "lib" || k === "rlib" || k === "dylib" || k === "cdylib" || k === "staticlib" || k === "proc-macro");

export function testArgs(metadata, member, skip) {
  const pkg = metadata.packages.find(p => p.name === member);
  if (!pkg) throw new Error(`no workspace member ${member}`);
  const tests = pkg.targets.filter(t => t.kind.includes("test"));
  if (!tests.some(t => skip.includes(t.name))) return { args: null, doc: false };
  const args = [];
  if (pkg.targets.some(isLib)) args.push("--lib");
  if (pkg.targets.some(t => t.kind.includes("bin"))) args.push("--bins");
  for (const t of tests) if (!skip.includes(t.name)) args.push("--test", t.name);
  return { args, doc: pkg.targets.some(t => isLib(t) && t.doctest !== false) };
}

/** One member's line: `before -> after`, and whether it is a DROP (a test that stopped running). */
export function count(member, before, after, dropOk = false) {
  const a = Number(after);
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
    const roots = wasmRoots(readFileSync(join(metadata.workspace_root, "build.sh"), "utf8"));
    if (roots.length === 0) { process.stderr.write("pr-scope: build.sh names no wasm build: cannot say which members ship\n"); process.exit(2); }
    process.stdout.write(`${JSON.stringify(plan(metadata, changed, closure(metadata, roots)))}\n`);
  } else if (cmd === "test-args") {
    const metadata = JSON.parse(readFileSync(a[0], "utf8"));
    const r = testArgs(metadata, a[1], a.slice(2));
    process.stdout.write(`${r.args ? r.args.join(" ") : ""}\n${r.doc ? "doc" : ""}\n`);
  } else if (cmd === "model-targets") {
    const metadata = JSON.parse(readFileSync(a[0], "utf8"));
    process.stdout.write(`${modelTargets(metadata, a[1]).join(" ")}\n`);
  } else if (cmd === "model-check") {
    const metadata = JSON.parse(a[0] ? readFileSync(a[0], "utf8") : execFileSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }));
    const bad = misnamedModels(metadata, f => readFileSync(f, "utf8"));
    const n = metadata.packages.reduce((k, p) => k + p.targets.filter(t => t.kind.includes("test")).length, 0);
    const models = metadata.packages.flatMap(p => modelTargets(metadata, p.name).map(t => `${p.name}@${t}`));
    if (bad.length) { for (const b of bad) process.stderr.write(`model-check: ${b}\n`); process.exit(1); }
    process.stdout.write(`model-check: ${n} test target(s) scanned, ${models.length} model target(s): ${models.join(" ")}\n`);
    if (models.length === 0) { process.stderr.write("model-check: NO model target found: the derivation reads nothing\n"); process.exit(1); }
  } else if (cmd === "count") {
    const r = count(a[0], a[1], a[2], a.includes("--drop-ok"));
    process.stdout.write(`${r.line}\n`);
    process.exit(r.drop ? 1 : 0);
  } else {
    process.stderr.write("usage: pr-scope.mjs plan <metadata.json> < changed | count <member> <before|-> <after> [--drop-ok]\n");
    process.exit(2);
  }
}
