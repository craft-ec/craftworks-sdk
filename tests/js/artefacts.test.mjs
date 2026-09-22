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
import { shippedArtefacts } from "../../js/session.js";
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
  // The artefacts CONTAINER (builder#104): the shipped file is what the
  // manifest says, and it names the address a builder PUTs it under.
  const c = manifest.container;
  assert.ok(c && c.address && c.sha256 && c.xz, `artefacts.json has no usable container entry: ${JSON.stringify(c)}`);
  const cbytes = new Uint8Array(await readFile(new URL("artefacts.webapp", dir)));
  assert.equal(await sha(cbytes), c.sha256, "artefacts.webapp does not hash to what artefacts.json claims");
  assert.equal(cbytes.length, c.bytes);
});

// ---------------------------------------------------------------------------
// ONE HASH, SEVERAL PLACES TO GET IT.
//
// An app that named ONE source for its artefacts is dead whenever that source
// is unavailable — and if every app names the same one, every app is dead
// together. The hash is the identity, so a second source costs nothing in
// trust: it cannot serve anything different, because anything different does
// not hash to this.
// ---------------------------------------------------------------------------

await t("**the first source that VERIFIES wins, not the first that answers**", async () => {
  const good = new Uint8Array([1, 2, 3]);
  const sha256 = await sha(good);
  const asked = [];
  const fetchWith = async url => {
    asked.push(url);
    // A source that answers 200 with the WRONG bytes. It must not end the
    // search: a wrong artefact is not a smaller one.
    if (url === "a") return new Response(new Uint8Array([9, 9, 9]));
    if (url === "b") return new Response(good);
    return new Response("", { status: 404 });
  };
  const bytes = await artefactBytes(
    { urls: ["a", "b"], sha256 },
    { fetch: fetchWith, caches: null, subtle: crypto.subtle },
  );
  assert.deepEqual([...bytes], [...good], "it took the bytes that did not verify");
  assert.deepEqual(asked, ["a", "b"], "it stopped at the first source that merely answered");
});

await t("a source that is DOWN is skipped, not fatal", async () => {
  const good = new Uint8Array([4, 5, 6]);
  const sha256 = await sha(good);
  const fetchWith = async url => {
    if (url === "down") throw new Error("connection refused");
    return new Response(good);
  };
  const bytes = await artefactBytes(
    { urls: ["down", "up"], sha256 },
    { fetch: fetchWith, caches: null, subtle: crypto.subtle },
  );
  assert.deepEqual([...bytes], [...good], "one unreachable source killed the lot");
});

await t("**a total failure names the artefact AND what each source did**", async () => {
  // An app that will not open is the symptom a person reports, so the first
  // thing they can send must identify the block and say what was tried.
  const sha256 = await sha(new Uint8Array([7]));
  const fetchWith = async url => {
    if (url === "missing") return new Response("", { status: 404 });
    return new Response(new Uint8Array([8]));      // wrong bytes
  };
  await assert.rejects(
    () => artefactBytes({ urls: ["missing", "wrong"], sha256 }, { fetch: fetchWith, caches: null, subtle: crypto.subtle }),
    e => {
      assert.match(e.message, new RegExp(sha256), "the message does not name WHICH artefact");
      assert.match(e.message, /missing: 404/, "it does not say what the first source did");
      assert.match(e.message, /wrong: does not hash/, "it does not say what the second source did");
      return true;
    },
  );
});

await t("THE CONTROL: a single `url` still works, unchanged", async () => {
  // Every existing caller passes one url. If the list form had replaced it
  // rather than joined it, they would all fail — and this is the shape the
  // manifest still produces today.
  const good = new Uint8Array([1]);
  const sha256 = await sha(good);
  const bytes = await artefactBytes(
    { url: "only", sha256 },
    { fetch: async () => new Response(good), caches: null, subtle: crypto.subtle },
  );
  assert.deepEqual([...bytes], [...good]);
});

await t("THE CONTROL: no url at all is refused, not silently empty", async () => {
  await assert.rejects(
    () => artefactBytes({ sha256: "abc" }, { fetch: async () => new Response(new Uint8Array()), caches: null, subtle: crypto.subtle }),
    /no url to fetch it from/,
  );
});

// ---------------------------------------------------------------------------
// AN APP NAMES ITS ARTEFACTS AND CARRIES NONE OF THEM (§19, sdk#108).
// ---------------------------------------------------------------------------

const MANIFEST = JSON.stringify({
  delegate: { file: "engine_delegate.wasm", sha256: "d".repeat(64), bytes: 1 },
  block: { file: "block.wasm", sha256: "b".repeat(64), bytes: 1 },
  register: { file: "register.wasm", sha256: "r".repeat(64), bytes: 1 },
  sdk: { file: "craftworks_sdk_bg.wasm", sha256: "5".repeat(64), bytes: 1 },
});

const manifestFetch = async url =>
  url.endsWith("artefacts.json")
    ? new Response(MANIFEST)
    : new Response("", { status: 404 });

await t("**an app that names a contract key points at it FIRST**", async () => {
  const spec = await shippedArtefacts(manifestFetch, "https://node/v1/contract/web/theapp/sdk/session.js", {
    artefactsKey: "ARTEFACTS",
    origin: "https://node",
  });
  assert.deepEqual(spec.delegate.urls, [
    "https://node/v1/contract/web/ARTEFACTS/engine_delegate.wasm",
    "https://node/v1/contract/web/theapp/sdk/engine_delegate.wasm",
  ], "the shared copy is not tried first, or the local one is not kept as a fallback");
  assert.equal(spec.delegate.sha256, "d".repeat(64), "the hash from the manifest is gone");
});

await t("THE CONTROL: with no key, it is the local file and nothing else", async () => {
  // The development path, and what every existing caller gets. If naming a
  // contract had REPLACED the local file rather than joined it, a build with
  // no published artefacts would resolve nothing.
  const spec = await shippedArtefacts(manifestFetch, "https://node/v1/contract/web/theapp/sdk/session.js", {});
  assert.deepEqual(spec.block.urls, ["https://node/v1/contract/web/theapp/sdk/block.wasm"]);
});

await t("**NO CIRCULARITY: resolving the artefacts consumes none of them**", async () => {
  // A bootstrap route is only interesting if it bootstraps from NOTHING.
  // Every url is a plain HTTP GET to the node already serving the page; none
  // of them resolves a Block contract, which would need `block.wasm` — one
  // of the very four being fetched.
  const asked = [];
  const spec = await shippedArtefacts(
    async url => { asked.push(url); return manifestFetch(url); },
    "https://node/v1/contract/web/theapp/sdk/session.js",
    { artefactsKey: "ARTEFACTS", origin: "https://node" },
  );
  assert.deepEqual(asked, ["https://node/v1/contract/web/theapp/sdk/artefacts.json"],
    "reading the manifest itself fetched something other than the manifest");
  for (const [name, e] of Object.entries(spec)) {
    for (const u of e.urls) {
      assert.match(u, /^https:\/\/node\/v1\/contract\/web\//,
        `${name} is fetched from ${u}, which is not a plain GET to this node`);
    }
  }
});

await t("**THE CONTROL THAT MATTERS: bytes that do not match the named hash are REFUSED**", async () => {
  // The acceptance names this one specifically. A shared artefact is only
  // safe because the hash is checked — an app must not load something
  // unverified, it must not load at all.
  const sha256 = await sha(new Uint8Array([1, 2, 3]));
  await assert.rejects(
    () =>
      artefactBytes(
        { urls: ["https://node/v1/contract/web/ARTEFACTS/engine_delegate.wasm"], sha256 },
        {
          fetch: async () => new Response(new Uint8Array([9, 9, 9])),
          caches: null,
          subtle: crypto.subtle,
        },
      ),
    e => {
      assert.match(e.message, /does not hash/, "it did not say the bytes failed verification");
      assert.match(e.message, new RegExp(sha256), "it did not name the artefact");
      return true;
    },
  );
});

await t("**naming a key with no origin is a REFUSAL, not a quiet fallback**", async () => {
  // An app that names a contract carries none of the artefacts, so the only
  // remaining source is a file it does not ship: it 404s, and the failure
  // names a missing LOCAL file — pointing at the app's own bundle when the
  // real fault is that there was nowhere to ask. Fatal and misleading
  // together.
  await assert.rejects(
    () => shippedArtefacts(manifestFetch, "https://node/v1/contract/web/theapp/sdk/session.js", {
      artefactsKey: "ARTEFACTS",
      origin: null,
    }),
    e => {
      assert.match(e.message, /ARTEFACTS/, "the refusal does not name the contract");
      assert.match(e.message, /no origin/, "it does not say what is missing");
      return true;
    },
  );
});

await t("THE CONTROL: no key and no origin is fine — that is the local path", async () => {
  // Without this, refusing whenever `origin` is absent would break every
  // development build and every test that passes no origin at all.
  const spec = await shippedArtefacts(manifestFetch, "https://node/v1/contract/web/theapp/sdk/session.js", {
    origin: null,
  });
  assert.deepEqual(spec.block.urls, ["https://node/v1/contract/web/theapp/sdk/block.wasm"]);
});

await t("`url` and `urls` together is refused, not silently half-used", async () => {
  // Taking either one drops a source the caller believed it supplied, and
  // the symptom is an artefact resolving from the wrong place with no hint
  // that half the request was discarded.
  await assert.rejects(
    () => artefactBytes(
      { url: "a", urls: ["b"], sha256: "f".repeat(64) },
      { fetch: async () => new Response(new Uint8Array()), caches: null, subtle: crypto.subtle },
    ),
    /both .url. and .urls./,
  );
});

process.stdout.write(failures ? `\n${failures} failing\n` : "\nall ok\n");
process.exit(failures ? 1 : 0);
