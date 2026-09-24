// Fetching the SDK's wasm artefacts ONCE per node, not once per app.
//
// Every app on a node is a path on the SAME origin — freenet serves web
// contracts from `/v1/contract/web/<key>/` — so they share one storage
// partition. Measured: an app at one path reads what an app at another path
// stored, with no fetch at all (sdk#5).
//
// That is what makes sharing possible. What makes it HAPPEN is caching by
// CONTENT HASH rather than by URL: two apps shipping their own copy of the
// same bytes have two URLs and would download twice, but they have one hash.
//
// # Why not the HTTP cache
//
// Because it does not work here, measured rather than assumed. The node
// serves web-contract files through `ServeFile` and sends no `cache-control`,
// no `etag` and no `last-modified`; with no directive the browser re-fetches,
// and a second app downloaded the whole 474,463 B again. An `immutable`
// header would fix it and is exactly true of a content-addressed URL — but
// that header belongs to the node, not to us. This route needs none.
//
// # A CACHE ENTRY IS A CLAIM UNTIL ITS HASH MATCHES
//
// This is the whole safety argument, not a nicety. The HTTP cache is
// poison-resistant by accident: the browser fills it from the response it
// fetched. A cache the SDK writes is one that ANY script on that origin can
// write, and the bytes are a wasm module every app on that origin then runs.
//
// So: bytes are hashed and compared before they are used, coming out of the
// cache and coming off the network. A mismatch is DISCARDED and re-fetched —
// never repaired in place, never used once "just this time", and never left
// behind for the next app to retry.
//
// # Storage is allowed to fail
//
// Private browsing, a refused quota, an eviction mid-session: every call into
// the Cache API can throw, and an app that breaks in a private window is a
// bug a person reports rather than a test catches. So every storage touch is
// best-effort — a failure degrades to fetching, which is exactly what the
// code did before there was a cache.
//
// The fetch itself — re-asked on the page's back-off until answered — is the one fetch, `served`
// (served.js); this file only adds the cache in front of it.

import { best, digestOf, matches, served, sleepFor } from "./served.js";

/** Where cached artefacts live. One name, so every app finds the same ones. */
export const CACHE_NAME = "craftworks-artefacts";

/**
 * The page's Cache Storage, or `null` where it cannot be had. In a SANDBOXED
 * frame without `allow-same-origin` — which is how a node serves every web
 * app (builder#104) — merely READING `caches` throws a SecurityError. No cache
 * then means fetching and verifying every time, which is already this file's
 * path for a cache that is unavailable; throwing instead stopped the app from
 * opening at all.
 */
export function ambientCaches() {
  try {
    return typeof caches === "object" ? caches : null;
  } catch (_) {
    return null;
  }
}

/**
 * The bytes for one artefact: from the shared cache if they are there AND
 * correct, otherwise fetched, checked, and left there for the next app.
 *
 * `caches` and `fetch` are injected so the wiring can be tested without a
 * browser — the same reason `openSession` injects them. `caches: null` turns
 * the cache OFF, which is the control proving the cache is what avoids the
 * second fetch rather than something else in the environment.
 */
export async function artefactBytes(
  { url, urls, sha256 },
  {
    fetch: fetchWith = typeof fetch === "function" ? fetch : null,
    caches: cacheStorage = ambientCaches(),
    subtle = typeof crypto === "object" ? crypto.subtle : null,
    // Told after every round nothing verified: { sha256, waitedMs, nextMs,
    // failures, says }. `says` is the line a loader shows.
    onWait = null,
    // A person's cancel. Nothing else ends a wait.
    signal = null,
    sleep = sleepFor,
    now = () => Date.now(),
  } = {},
) {
  if (!sha256) {
    // Without a hash nothing can be verified and nothing may be shared: a
    // cache keyed by a name nobody checks is worse than no cache at all.
    throw new Error(
      `artefact ${url ?? (urls ?? []).join(", ")} has no sha256; refusing to cache it`,
    );
  }
  const key = `/artefact/${sha256}`;
  const box = cacheStorage ? await best(() => cacheStorage.open(CACHE_NAME)) : null;

  if (box) {
    const hit = await best(() => box.match(key));
    if (hit) {
      const claimed = await best(async () => new Uint8Array(await hit.arrayBuffer()));
      if (claimed && (await matches(claimed, sha256, subtle))) return claimed;
      // Present and WRONG, or unreadable. Drop it rather than serve it, and
      // fall through to a fetch: the cache is shared, so a bad entry is not
      // necessarily one this app put there.
      await best(() => box.delete(key));
    }
  }

  // MORE THAN ONE PLACE TO GET IT, because the hash is the identity.
  //
  // An app that named ONE source for its artefacts would be dead whenever
  // that one source was unavailable — and if every app names the same one,
  // every app is dead together. That is a coupling worth removing, and it is
  // free: the bytes are verified against `sha256` either way, so a second
  // source costs nothing in trust. It cannot serve anything different,
  // because anything different does not hash to this.
  //
  // First one that VERIFIES wins, not first that answers: a source that
  // returns 200 with the wrong bytes must not end the search.
  //
  // The wrong hash each source answered LAST round. A 200 whose bytes do not
  // hash is not yet a refusal: a node unpacking its web cache serves a TORN
  // (truncated) file for a moment (measured: 1,242,614 B of 1,259,519), which
  // hashes wrong and then right. The SAME wrong bytes twice is an answer.
  const wrongBefore = new Map();
  const bytes = await served(
    { url, urls },
    {
      fetch: fetchWith, signal, sleep, now,
      name: `artefact ${sha256}`,
      refusal: "a hash mismatch",
      onWait: onWait && (w => onWait({ sha256, ...w })),
      check: async (got, from) => {
        const digest = await digestOf(got, subtle);
        if (digest === sha256) return null;
        // Never installed, never cached. A wrong artefact is not a smaller one.
        const again = digest !== null && wrongBefore.get(from) === digest;
        wrongBefore.set(from, digest);
        return { says: `does not hash to ${sha256} (${got.length} B hashing to ${digest}${again ? ", the same again" : ""})`, answer: again };
      },
    },
  );
  // Storing is the OPTIONAL part: a full or refused cache costs the next app
  // a fetch, which is what it would have paid anyway.
  if (box) await best(() => box.put(key, new Response(bytes)));
  return bytes;
}

/**
 * All three, in parallel and awaited together.
 *
 * A partial set is not a smaller provisioning, it is one that provisions a
 * signer it cannot then give contract code to — so one failure fails the
 * lot rather than leaving a half-provisioned node.
 *
 * **The signer (a delegate) is always fetched, never named by key.** `DelegateRequest`
 * has only `RegisterDelegate { delegate: DelegateContainer, … }`,
 * `ApplicationMessages` and `UnregisterDelegate` — there is no request that
 * fetches delegate code from the network, so its bytes must come from the
 * page. It shares the cache like the others; it just has nowhere else to come
 * from on a miss.
 */
export async function allArtefactBytes(spec, deps = {}) {
  const names = ["signer", "block", "register"];
  const out = await Promise.all(
    names.map(n => {
      if (!spec[n]) throw new Error(`no ${n} artefact in the manifest`);
      return artefactBytes(spec[n], deps);
    }),
  );
  return Object.fromEntries(names.map((n, i) => [n, out[i]]));
}
