// THE SHARED ARTEFACT CACHE: one download per node, not one per app (sdk#5).
//
// Every assertion here has a control beside it, because the claim is about
// something NOT happening — a second fetch — and "it did not happen" is the
// shape that passes when the mechanism is absent, when the test is wrong, and
// when the environment differs. So each arm runs twice: once with the cache
// and once with it off.

import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import { artefactBytes, allArtefactBytes, CACHE_NAME } from "../../js/artefacts.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};

const subtle = webcrypto.subtle;
const hex = b => [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, "0")).join("");
const sha = async bytes => hex(await subtle.digest("SHA-256", bytes));

/** A Cache Storage stand-in: one Map per named box, shared by every "app". */
function fakeCaches({ failOpen = false, failPut = false } = {}) {
  const boxes = new Map();
  return {
    boxes,
    async open(name) {
      if (failOpen) throw new Error("storage unavailable (private window)");
      if (!boxes.has(name)) boxes.set(name, new Map());
      const m = boxes.get(name);
      return {
        async match(k) { const v = m.get(k); return v ? new Response(v) : undefined; },
        async put(k, res) {
          if (failPut) throw new Error("QuotaExceededError");
          m.set(k, new Uint8Array(await res.arrayBuffer()));
        },
        async delete(k) { return m.delete(k); },
      };
    },
  };
}

/** A fetch that counts, so "no second download" is a number and not a vibe. */
function countingFetch(bytes) {
  let calls = 0;
  const f = async () => { calls += 1; return new Response(bytes); };
  return { fetch: f, calls: () => calls };
}

const PAYLOAD = new Uint8Array(4096).map((_, i) => (i * 7) % 251);

await t("a cold cache fetches, and a second app does not", async () => {
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches();
  const net = countingFetch(PAYLOAD);
  const spec = { url: "/shared/sdk.wasm", sha256 };

  const a = await artefactBytes(spec, { fetch: net.fetch, caches, subtle });
  assert.equal(net.calls(), 1, "the first app must actually fetch (cold cache)");
  assert.deepEqual(a, PAYLOAD);

  const b = await artefactBytes(spec, { fetch: net.fetch, caches, subtle });
  assert.equal(net.calls(), 1, "the second app must not fetch again");
  assert.deepEqual(b, PAYLOAD, "and must get the same bytes");
});

await t("CONTROL: with the cache off, the second app fetches again", async () => {
  const sha256 = await sha(PAYLOAD);
  const net = countingFetch(PAYLOAD);
  const spec = { url: "/shared/sdk.wasm", sha256 };

  await artefactBytes(spec, { fetch: net.fetch, caches: null, subtle });
  await artefactBytes(spec, { fetch: net.fetch, caches: null, subtle });
  assert.equal(
    net.calls(), 2,
    "without the cache there must be two fetches, or the test above proves \
nothing about the cache",
  );
});

await t("a POISONED entry is discarded and the bytes are fetched", async () => {
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches();
  const net = countingFetch(PAYLOAD);

  // Right key, wrong bytes: what any other script on the origin could write.
  const box = await caches.open(CACHE_NAME);
  await box.put(`/artefact/${sha256}`, new Response(new Uint8Array([1, 2, 3])));

  const got = await artefactBytes(
    { url: "/shared/sdk.wasm", sha256 },
    { fetch: net.fetch, caches, subtle },
  );
  assert.equal(net.calls(), 1, "a claim that does not match its hash must be re-fetched");
  assert.deepEqual(got, PAYLOAD, "and the REAL bytes returned, never the poison");
});

await t("a poisoned entry is not left behind for the next app", async () => {
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches();
  const net = countingFetch(PAYLOAD);
  const box = await caches.open(CACHE_NAME);
  await box.put(`/artefact/${sha256}`, new Response(new Uint8Array([9, 9, 9])));

  await artefactBytes({ url: "/u", sha256 }, { fetch: net.fetch, caches, subtle });

  const after = caches.boxes.get(CACHE_NAME).get(`/artefact/${sha256}`);
  assert.deepEqual(
    new Uint8Array(after), PAYLOAD,
    "the poison must be gone, replaced by verified bytes — not left to be \
retried by every app after this one",
  );
  const second = countingFetch(PAYLOAD);
  await artefactBytes({ url: "/u", sha256 }, { fetch: second.fetch, caches, subtle });
  assert.equal(second.calls(), 0, "and the next app must get a clean hit");
});

await t("a poisoned entry is removed even when the cache REFUSES to replace it", async () => {
  // The case that makes the delete load-bearing, found by mutation: while the
  // put succeeds it OVERWRITES the poison, so removing the delete changes
  // nothing and the test above still passes. When the cache is full the put
  // fails — and without the delete the poison stays, and every app after this
  // one reads it, fails its hash check, and re-fetches for ever.
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches({ failPut: true });
  // Seeded behind the stand-in's back, because it refuses puts — which is
  // the condition under test.
  await caches.open(CACHE_NAME);
  caches.boxes.get(CACHE_NAME).set(`/artefact/${sha256}`, new Uint8Array([7, 7, 7]));

  const net = countingFetch(PAYLOAD);
  const got = await artefactBytes({ url: "/u", sha256 }, { fetch: net.fetch, caches, subtle });
  assert.deepEqual(got, PAYLOAD, "the caller still gets the real bytes");

  assert.equal(
    caches.boxes.get(CACHE_NAME).has(`/artefact/${sha256}`), false,
    "the poison must be GONE. A bad entry that cannot be replaced must be \
removed, or it is re-tried by every app that follows.",
  );
});

await t("bytes that do not hash to what was promised are never installed", async () => {
  const caches = fakeCaches();
  const net = countingFetch(new Uint8Array([4, 5, 6]));
  const promised = await sha(PAYLOAD);
  await assert.rejects(
    () => artefactBytes({ url: "/u", sha256: promised }, { fetch: net.fetch, caches, subtle }),
    /does not hash to/,
    "a wrong artefact is not a smaller one: it must be refused",
  );
  assert.equal(caches.boxes.get(CACHE_NAME)?.size ?? 0, 0, "and never cached");
});

await t("an artefact with no hash is refused rather than cached unverified", async () => {
  const net = countingFetch(PAYLOAD);
  await assert.rejects(
    () => artefactBytes({ url: "/u" }, { fetch: net.fetch, caches: fakeCaches(), subtle }),
    /no sha256/,
  );
  assert.equal(net.calls(), 0, "and not even fetched");
});

await t("storage that is UNAVAILABLE falls back to fetching", async () => {
  const sha256 = await sha(PAYLOAD);
  const net = countingFetch(PAYLOAD);
  const got = await artefactBytes(
    { url: "/u", sha256 },
    { fetch: net.fetch, caches: fakeCaches({ failOpen: true }), subtle },
  );
  assert.deepEqual(got, PAYLOAD, "a private window must still load the app");
  assert.equal(net.calls(), 1);
});

await t("storage that REFUSES to store falls back to fetching", async () => {
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches({ failPut: true });
  const net = countingFetch(PAYLOAD);

  const a = await artefactBytes({ url: "/u", sha256 }, { fetch: net.fetch, caches, subtle });
  assert.deepEqual(a, PAYLOAD, "a full quota must not fail the load");
  const b = await artefactBytes({ url: "/u", sha256 }, { fetch: net.fetch, caches, subtle });
  assert.deepEqual(b, PAYLOAD);
  assert.equal(net.calls(), 2, "it costs the next app a fetch, which is what it would have paid anyway");
});

await t("all three are fetched together, and a missing one fails the set", async () => {
  const sha256 = await sha(PAYLOAD);
  const caches = fakeCaches();
  const net = countingFetch(PAYLOAD);
  const spec = {
    delegate: { url: "/d", sha256 },
    block: { url: "/b", sha256 },
    register: { url: "/r", sha256 },
  };
  const all = await allArtefactBytes(spec, { fetch: net.fetch, caches, subtle });
  assert.deepEqual(Object.keys(all).sort(), ["block", "delegate", "register"]);
  // All three are the same bytes here, so the cache makes it ONE fetch — which
  // is the content-addressing working: same hash, same entry, whatever the url.
  assert.ok(net.calls() >= 1);

  await assert.rejects(
    () => allArtefactBytes({ delegate: spec.delegate }, { fetch: net.fetch, caches, subtle }),
    /no block artefact/,
    "a partial set is not a smaller provisioning",
  );
});

await t("the manifest's hashes ARE the shipped files", async () => {
  // The acceptance that the shared bytes are byte-identical to what a per-app
  // build produced. It is the same question as "is the hash right", because
  // the hash is the only thing an app checks before running the wasm — a
  // manifest that drifts from its files is a cache that can never hit, or
  // worse, one that rejects the correct bytes.
  //
  // NOT a silent skip when the build is missing: a test that quietly passes
  // without the artefacts is the gate this file exists to avoid.
  const dir = new URL("../../pkg/web/", import.meta.url);
  const manifest = JSON.parse(
    await readFile(new URL("artefacts.json", dir), "utf8"),
  );
  let checked = 0;
  for (const name of ["delegate", "block", "register", "sdk"]) {
    const entry = manifest[name];
    assert.ok(entry, `artefacts.json has no ${name}`);
    const bytes = new Uint8Array(await readFile(new URL(entry.file, dir)));
    assert.equal(
      await sha(bytes), entry.sha256,
      `${entry.file} does not hash to what artefacts.json claims`,
    );
    assert.equal(bytes.length, entry.bytes, `${entry.file} is not ${entry.bytes} B`);
    checked += 1;
  }
  assert.equal(checked, 4, "every artefact must be checked, not some of them");
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall ok\n");
process.exit(failures ? 1 : 0);
