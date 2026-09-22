// Turn `wasm-function[N]` frames back into Rust paths, from the names a build
// KEPT beside its stripped wasm (sdk#228: `pkg/web-symbols/<name>.<sha256 of
// the shipped bytes>.wasm`). Offline: nothing leaves the machine.
//
//   node tools/symbolise.mjs <kept.wasm> < trace.txt
//
// The `name` custom section, subsection 1 (function names): a count, then
// (index, name) pairs, all LEB128 — the WebAssembly spec's layout.
import { readFileSync } from "node:fs";

function uleb(b, i) {
  let r = 0, s = 0, x;
  do {
    x = b[i++];
    r |= (x & 0x7f) << s;
    s += 7;
  } while (x & 0x80);
  return [r >>> 0, i];
}

/** index → name, from a wasm module's `name` section (empty if it has none). */
export function functionNames(bytes) {
  const out = new Map();
  const secs = WebAssembly.Module.customSections(new WebAssembly.Module(bytes), "name");
  if (secs.length === 0) return out;
  const b = new Uint8Array(secs[0]);
  let i = 0;
  while (i < b.length) {
    const id = b[i++];
    let len;
    [len, i] = uleb(b, i);
    const end = i + len;
    if (id === 1) {
      let n;
      [n, i] = uleb(b, i);
      for (let k = 0; k < n; k++) {
        let idx, l;
        [idx, i] = uleb(b, i);
        [l, i] = uleb(b, i);
        out.set(idx, new TextDecoder().decode(b.subarray(i, i + l)));
        i += l;
      }
    }
    i = end;
  }
  return out;
}

/** Every `wasm-function[N]` in `trace`, with its name. */
export function symbolise(trace, names) {
  return [...trace.matchAll(/wasm-function\[(\d+)\]/g)].map(m => ({ index: Number(m[1]), name: names.get(Number(m[1])) ?? null }));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const names = functionNames(readFileSync(process.argv[2]));
  const trace = readFileSync(0, "utf8");
  for (const f of symbolise(trace, names)) console.log(`wasm-function[${f.index}] = ${f.name ?? "(no name)"}`);
}
