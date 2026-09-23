#!/usr/bin/env node
// THE DUPLICATE-CODE GATE (craftworks-sdk#320, builder#120).
//
// The owner's rule 3 (one owner per fact) was checked by people, and people
// missed copies. This checks it by machine: jscpd, at a PINNED version, over
// the configured sources; every clone it finds is keyed by its CONTENT and the
// two files it sits in (never by line numbers, which move with every edit);
// and the gate fails when a clone appears that the baseline does not name.
// The baseline is today's known duplicates, to be worked DOWN, never up by
// accident: adding to it is `--write-baseline`, a change a reviewer sees.
//
//   node tools/dup-gate.mjs [--config dup-gate.json] [--root DIR] [--write-baseline]
//
// Exit 0: no clone outside the baseline. Exit 1: new clone(s), each printed
// with both places. Exit 2: could not check (jscpd did not run, no report) --
// a FAILURE, never a pass: a gate that cannot look must not say "nothing".
//
// ONE COPY OF THIS SCRIPT. The builder's gate runs this file from the SDK
// revision it pins (`git show <SDK_REV>:tools/dup-gate.mjs`), with its own
// config and baseline; a copy in each repo would be the defect it looks for.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

export const JSCPD = "jscpd@4.0.9";

const arg = (name, fallback = null) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : fallback;
};

/** A clone's identity: its format, its two files (sorted), and its text with
 * whitespace collapsed. A clone that MOVES within its files keeps its key; a
 * third copy of the same text elsewhere is a new pair, so a new key. */
export function keyOf(c) {
  const files = [c.firstFile.name, c.secondFile.name].sort();
  const text = String(c.fragment ?? "").replace(/\s+/g, " ").trim();
  return createHash("sha256").update(`${c.format}\0${files.join("\0")}\0${text}`).digest("hex").slice(0, 16);
}

const where = f => `${f.name}:${f.start}-${f.end}`;

/** Run jscpd over `dirs` under `root`; the clones it reports. Throws when it
 * could not run or wrote no report. */
export function clones(root, cfg) {
  const out = mkdtempSync(join(tmpdir(), "dup-gate-"));
  try {
    const args = [
      "--yes", JSCPD, "--silent",
      "--reporters", "json", "--output", out,
      "--min-tokens", String(cfg.minTokens ?? 50),
      "--formats-exts", cfg.formats,
      "--ignore", (cfg.ignore ?? []).join(","),
      ...cfg.dirs.filter(d => existsSync(join(root, d))),
    ];
    execFileSync("npx", args, { cwd: root, stdio: ["ignore", "pipe", "pipe"], encoding: "utf8" });
    const report = join(out, "jscpd-report.json");
    if (!existsSync(report)) throw new Error(`${JSCPD} wrote no report`);
    const r = JSON.parse(readFileSync(report, "utf8"));
    if (!r.statistics?.total) throw new Error(`${JSCPD}'s report has no totals: it did not scan`);
    // ZERO FILES SCANNED is not "no duplicates": a config whose dirs moved
    // would otherwise pass for ever, having looked at nothing.
    if (!r.statistics.total.sources) throw new Error(`${JSCPD} scanned no files under ${cfg.dirs.join(", ")}`);
    return { duplicates: r.duplicates ?? [], total: r.statistics.total };
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
}

export function main() {
  const root = resolve(arg("--root", "."));
  const cfgPath = join(root, arg("--config", "dup-gate.json"));
  const cfg = JSON.parse(readFileSync(cfgPath, "utf8"));
  const basePath = join(root, cfg.baseline ?? "dup-baseline.json");
  let found;
  try {
    found = clones(root, cfg);
  } catch (e) {
    console.log(`dup-gate: COULD NOT CHECK (${e.message.split("\n")[0]}) — a failure, not a pass`);
    return 2;
  }
  const now = new Map(found.duplicates.map(c => [keyOf(c), c]));
  if (process.argv.includes("--write-baseline")) {
    const entries = [...now].map(([key, c]) => ({ key, a: where(c.firstFile), b: where(c.secondFile), lines: c.lines })).sort((x, y) => x.a.localeCompare(y.a));
    writeFileSync(basePath, JSON.stringify({ tool: JSCPD, note: "known duplicates, to be worked DOWN (craftworks-sdk#320)", clones: entries }, null, 1) + "\n");
    console.log(`dup-gate: wrote ${entries.length} known clone(s) to ${cfg.baseline ?? "dup-baseline.json"}`);
    return 0;
  }
  const base = existsSync(basePath) ? JSON.parse(readFileSync(basePath, "utf8")).clones : [];
  const known = new Set(base.map(e => e.key));
  const added = [...now].filter(([k]) => !known.has(k));
  const gone = base.filter(e => !now.has(e.key));
  for (const [, c] of added) {
    console.log(`NEW DUPLICATE (${c.lines} lines, ${c.format}): ${where(c.firstFile)}  ==  ${where(c.secondFile)}`);
  }
  for (const e of gone) console.log(`gone from the baseline (drop it with --write-baseline): ${e.a} == ${e.b}`);
  const line = `dup-gate: ${now.size} clone(s) by ${JSCPD} over ${found.total.sources} file(s), ${known.size} known, ${added.length} NEW, ${gone.length} gone`;
  console.log(line);
  return added.length ? 1 : 0;
}

if (import.meta.url === `file://${process.argv[1]}`) process.exit(main());
