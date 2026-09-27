#!/usr/bin/env node
// THE PUBLISHING DOORS, DERIVED (ARCHITECTURE §19, app-as-data P5; the architect's control): which of the SDK's
// exported doors can reach a NON-TREE PUT -- never a hand list. Every wasm-exported method (`pub fn` of the web crate's
// `Session` and `Db`) is followed through the private methods of its file it calls (`self.<name>(`), and classified by
// the SINKS its closure reaches:
//   * SITE WRITE: page-io's site paths -- `.publish_app(` (the app-publish machine), `.publish_site(` / `.send_site(`;
//   * PIECE PUT: `.put_contract(` (a content-addressed container PUT: the load pieces' repair).
// Then the JS doors that call each such wasm door (`session.<door>(` / `#db.<door>(` in js/*.js), named by their
// enclosing function. The build ships the result as pkg/web/doors.json; the SDK's test pins it (site writes ==
// publishDefinition, piece PUTs == the repair), and the builder scans its own code against it.
//
//   node tools/doors.mjs [<sdk root>]   -> JSON on stdout
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const SINKS = {
  siteWrite: /(?<!self)\.(publish_app|publish_site|send_site)\(/,
  piecePut: /(?<!self)\.put_contract\(/,
};

/** Rust methods of one file at impl indent: `{ name: { pub, body } }`. */
function methods(text) {
  const out = {};
  const re = /^ {4}(pub )?fn (\w+)\s*[(<]/gm;
  const at = [...text.matchAll(re)];
  for (const [i, m] of at.entries()) {
    const end = i + 1 < at.length ? at[i + 1].index : text.length;
    out[m[2]] = { pub: !!m[1], body: text.slice(m.index, end) };
  }
  return out;
}

/** Which exported doors of `rust` (file -> text) reach each sink, through their file's private methods. */
export function deriveWasm(rust) {
  const found = { siteWrite: new Set(), piecePut: new Set() };
  for (const text of Object.values(rust)) {
    const ms = methods(text);
    const reaches = (name, kind, seen = new Set()) => {
      if (seen.has(name) || !ms[name]) return false;
      seen.add(name);
      const { body } = ms[name];
      if (SINKS[kind].test(body)) return true;
      return [...body.matchAll(/self\.(\w+)\(/g)].some(c => c[1] !== name && reaches(c[1], kind, seen));
    };
    for (const [name, m] of Object.entries(ms)) {
      if (!m.pub) continue;
      for (const kind of Object.keys(SINKS)) if (reaches(name, kind)) found[kind].add(name);
    }
  }
  return { siteWrite: [...found.siteWrite].sort(), piecePut: [...found.piecePut].sort() };
}

const KEYWORDS = new Set(["if", "for", "while", "switch", "catch", "return", "await", "else", "do", "try"]);

/** The JS doors (`file:function`) that call any of `doors` on the wasm session/db. */
export function deriveJs(js, doors) {
  const out = new Set();
  for (const [file, text] of Object.entries(js)) {
    const lines = text.split("\n");
    for (const [i, line] of lines.entries()) {
      if (!doors.some(d => new RegExp(`(?:session|#db|raw)\\.${d}\\(`).test(line))) continue;
      let name = "?";
      for (let j = i; j >= 0; j--) {
        const m = lines[j].match(/^export (?:async )?function (\w+)/) ?? lines[j].match(/^ {2,4}(?:async )?(\w+)\s*(?:\(|:\s*(?:async\s*)?\()/);
        if (m && !KEYWORDS.has(m[1])) { name = m[1]; break; }
      }
      out.add(`${file}:${name}`);
    }
  }
  return [...out].sort();
}

/** The SDK's doors, derived from its sources at `root`. */
export function doorsOf(root) {
  const rust = Object.fromEntries(["web/src/session.rs", "web/src/lib.rs"].map(f => [f, readFileSync(join(root, f), "utf8")]));
  const js = Object.fromEntries(readdirSync(join(root, "js")).filter(f => f.endsWith(".js")).map(f => [f, readFileSync(join(root, "js", f), "utf8")]));
  const wasm = deriveWasm(rust);
  return {
    note: "derived by tools/doors.mjs: the SDK's doors that can reach a non-tree PUT (never a hand list)",
    siteWrite: { wasm: wasm.siteWrite, js: deriveJs(js, wasm.siteWrite) },
    piecePut: { wasm: wasm.piecePut, js: deriveJs(js, wasm.piecePut) },
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const root = process.argv[2] ?? fileURLToPath(new URL("..", import.meta.url));
  process.stdout.write(`${JSON.stringify(doorsOf(root), null, 2)}\n`);
}
