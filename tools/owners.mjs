#!/usr/bin/env node
// WHO OWNS WHAT A BRANCH CHANGES (craftworks-sdk#320, builder#120).
//
// Each task is one issue in files one engineer owns; two sessions editing one
// file is how PRs collide and how a fix lands on top of someone's half-done
// rewrite. `OWNERS` says who owns which paths (longest prefix wins; `*` is the
// default). This prints the owner of every file the branch changes against
// origin/main, and FAILS when they belong to more than one owner unless the
// branch SAYS so: a line starting `shared:` in a commit message on the
// branch, or in the PR body (`PR_BODY`, or the open PR for this branch, read
// through the REST API when `gh` is there).
//
//   node tools/owners.mjs [--root DIR] [--base origin/main]
//
// Exit 0: one owner, or several with a `shared:` line. Exit 1: several owners
// and no `shared:` line. Exit 2: could not check (no OWNERS, no base).
//
// ONE COPY: the builder's gate runs this file from the SDK revision it pins,
// with its own OWNERS.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const arg = (name, fallback = null) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : fallback;
};

/** `OWNERS`: one `path-prefix owner` per line; `#` comments; `*` the default. */
export function parseOwners(text) {
  const rules = [];
  for (const raw of text.split("\n")) {
    const line = raw.replace(/#.*/, "").trim();
    if (!line) continue;
    const [path, owner, ...rest] = line.split(/\s+/);
    if (!owner || rest.length) throw new Error(`OWNERS: \`${raw.trim()}\` is not \`path owner\``);
    rules.push({ path, owner });
  }
  if (!rules.some(r => r.path === "*")) throw new Error("OWNERS has no `*` default: an unowned file would pass unseen");
  return rules;
}

/** The owner of `file`: the rule with the longest matching prefix. */
export function ownerOf(rules, file) {
  // A path ending `/` owns everything under it; any other path owns exactly
  // that file. `*` is length 0, so any real match beats it.
  const matches = r => r.path === "*" || (r.path.endsWith("/") ? file.startsWith(r.path) : file === r.path);
  const len = r => (r.path === "*" ? 0 : r.path.length);
  return rules.filter(matches).reduce((a, b) => (len(b) > len(a) ? b : a)).owner;
}

const git = (root, ...a) => execFileSync("git", ["-C", root, ...a], { encoding: "utf8" }).trim();

function prBody(root) {
  if (process.env.PR_BODY !== undefined) return process.env.PR_BODY;
  try {
    const branch = git(root, "rev-parse", "--abbrev-ref", "HEAD");
    const url = git(root, "remote", "get-url", "origin");
    const repo = url.replace(/^.*github\.com[:/]/, "").replace(/\.git$/, "");
    const owner = repo.split("/")[0];
    const out = execFileSync("gh", ["api", `repos/${repo}/pulls?head=${owner}:${branch}&state=open`, "--jq", ".[0].body // \"\""], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] });
    return out;
  } catch {
    return "";
  }
}

export function main() {
  const root = resolve(arg("--root", "."));
  const base = arg("--base", "origin/main");
  const ownersPath = join(root, "OWNERS");
  if (!existsSync(ownersPath)) {
    console.log("owners: COULD NOT CHECK (no OWNERS file) — a failure, not a pass");
    return 2;
  }
  let rules, since, files;
  try {
    rules = parseOwners(readFileSync(ownersPath, "utf8"));
    since = git(root, "merge-base", base, "HEAD");
    // Committed on the branch AND not yet committed: the gate runs on the tree.
    files = [...new Set([
      ...git(root, "diff", "--name-only", `${since}...HEAD`).split("\n"),
      ...git(root, "diff", "--name-only", "HEAD").split("\n"),
    ])].filter(Boolean).sort();
  } catch (e) {
    console.log(`owners: COULD NOT CHECK (${e.message.split("\n")[0]}) — a failure, not a pass`);
    return 2;
  }
  const by = new Map();
  for (const f of files) {
    const o = ownerOf(rules, f);
    if (!by.has(o)) by.set(o, []);
    by.get(o).push(f);
  }
  for (const [o, fs] of by) console.log(`  ${o}: ${fs.join(", ")}`);
  const said = /^shared:/im.test(git(root, "log", "--format=%B", `${since}..HEAD`)) || /^shared:/im.test(prBody(root));
  const owners = [...by.keys()];
  const line = `owners: ${files.length} changed file(s), ${owners.length} owner(s)${owners.length ? ` (${owners.join(", ")})` : ""}`;
  if (owners.length > 1 && !said) {
    console.log(`${line}: CROSSES OWNERS with no \`shared:\` line (in a commit message on the branch, or the PR body)`);
    return 1;
  }
  console.log(`${line}${owners.length > 1 ? ", shared: said" : ""}`);
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exit(main());
