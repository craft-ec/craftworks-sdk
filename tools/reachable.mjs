// Every module reachable from an entry, by following its imports.
//
// COMPUTED, never listed. A hand-written list of files to ship is the
// failure mode of every path-enumerating manifest: it is correct on the day
// it is written and silently wrong on the day someone adds a module. It has
// already happened twice here — `wrap.js` gained `session.js` and
// `engine-db.js`, and both the SDK's own package and the builder's copy of
// it went on shipping the older list. The symptom is not a build error; it
// is `ERR_MODULE_NOT_FOUND` in a browser, at run time, in somebody else's
// repository.
//
// Static relative imports only, which is all this package uses. A dynamic
// import would not be found — so the check below refuses a file that contains
// one rather than reporting a closure it cannot actually compute.

import { readFileSync } from "node:fs";
import { dirname, resolve, relative } from "node:path";

const STATIC = /^\s*(?:import|export)\b[^'"]*?from\s*['"](\.[^'"]+)['"]/gm;
const BARE_IMPORT = /^\s*import\s*['"](\.[^'"]+)['"]/gm;
const DYNAMIC = /\bimport\s*\(/;

/**
 * Every file reachable from `entry`, as paths relative to `root`, including
 * the entry itself.
 *
 * Throws on a module it cannot read — a broken import is exactly what this
 * exists to find, so it must not be swallowed — and on a dynamic import,
 * which it cannot follow and must not pretend to have followed.
 */
export function reachable(entry, root = dirname(entry)) {
  const seen = new Set();
  const queue = [resolve(entry)];
  while (queue.length) {
    const file = queue.shift();
    const rel = relative(root, file);
    if (seen.has(rel)) continue;
    seen.add(rel);

    let src;
    try {
      src = readFileSync(file, "utf8");
    } catch (e) {
      throw new Error(`${rel}: cannot be read, so something imports a module that is not there (${e.code})`);
    }
    if (DYNAMIC.test(src)) {
      throw new Error(`${rel}: contains a dynamic import, which this walker cannot follow — it must not report a closure it did not compute`);
    }
    for (const re of [STATIC, BARE_IMPORT]) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(src))) queue.push(resolve(dirname(file), m[1]));
    }
  }
  return [...seen].sort();
}

// As a command: print the reachable set, one per line.
if (import.meta.url === `file://${process.argv[1]}`) {
  const [entry, root] = process.argv.slice(2);
  for (const f of reachable(entry, root)) console.log(f);
}
