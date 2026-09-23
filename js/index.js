// Web entry point: `import { load } from "./sdk/index.js"; const sdk = await load();`
import init, * as raw from "./craftworks_sdk.js";
import { wrap } from "./wrap.js";
import { artefactBytes, servedText } from "./artefacts.js";

let ready;
/** Load once. `wasm` is optional bytes or a URL; by default the module fetches its own. */
export function load(wasm) {
  ready ??= init(wasm === undefined ? undefined : { module_or_path: wasm }).then(() => wrap(raw));
  return ready;
}

/**
 * Load the SDK's own wasm through the SHARED, content-addressed cache.
 *
 * `load()` fetches the module beside this one, which means one download per
 * APP. This asks for it by content hash instead, so the second app on a node
 * pays nothing — every app is a path on one origin, and the hash is the same
 * whichever app's copy it came from (sdk#5).
 *
 * The bytes are verified before they are handed to `init`, for the reason the
 * whole cache is verified: an entry any script on the origin can write is
 * about to be run as wasm by every app here.
 */
export async function loadShared({
  fetch: fetchWith = typeof fetch === "function" ? fetch : null,
  ...deps
} = {}) {
  const at = f => new URL(f, import.meta.url).href;
  // Through the one fetch: waited on, never ended by a status (#126 ruling).
  const m = JSON.parse(await servedText({ url: at("./artefacts.json") }, { fetch: fetchWith, ...deps }));
  if (!m.sdk || !m.sdk.file || !m.sdk.sha256) {
    throw new Error("artefacts.json has no usable sdk entry");
  }
  const bytes = await artefactBytes(
    { url: at("./" + m.sdk.file), sha256: m.sdk.sha256 },
    { fetch: fetchWith, ...deps },
  );
  return load(bytes);
}
