// Web entry point: `import { load } from "./sdk/index.js"; const sdk = await load();`
import init, * as raw from "./craftworks_sdk.js";
import { wrap } from "./wrap.js";

let ready;
/** Load once. `wasm` is optional bytes or a URL; by default the module fetches its own. */
export function load(wasm) {
  ready ??= init(wasm === undefined ? undefined : { module_or_path: wasm }).then(() => wrap(raw));
  return ready;
}
