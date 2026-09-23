// THE ONE FETCH (`served`, js/artefacts.js): every file a page or tool fetches
// over HTTP comes through it. It waits until answered on the page's back-off
// (rto.js, generated from page/src/rto.rs), with no give-up of its own; only a
// person's cancel, or a `check` that every source refuses as final, ends it.
import assert from "node:assert/strict";
import { served, servedText } from "../../js/artefacts.js";
import { RTO_SCHEDULE_MS } from "../../js/rto.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.message}\n`); }
};

/** A node that answers `answers` in order (a status, "throw", or bytes), then the last one for ever. */
function node(answers) {
  let n = 0;
  const fetch = async () => {
    const a = answers[Math.min(n, answers.length - 1)];
    n += 1;
    if (a === "throw") throw new Error("connection refused");
    if (typeof a === "number") return new Response("", { status: a });
    return new Response(a);
  };
  return { fetch, asked: () => n };
}

/** A clock the test drives: `sleep` records each wait and moves time on. */
function clock() {
  let at = 0;
  const waits = [];
  return { now: () => at, sleep: async ms => { waits.push(ms); at += ms; }, waits };
}

await t("**a file the node does not serve yet is WAITED on (404, 500, a refused connection), then fetched**", async () => {
  const net = node([404, 500, "throw", new TextEncoder().encode("hello")]);
  const c = clock();
  const said = [];
  const got = await served({ url: "/f" }, { fetch: net.fetch, sleep: c.sleep, now: c.now, onWait: w => said.push(w) });
  assert.equal(new TextDecoder().decode(got), "hello");
  assert.equal(net.asked(), 4, "not asked until answered");
  assert.deepEqual(c.waits, RTO_SCHEDULE_MS.slice(0, 3), "the waits are not the page's RTO schedule");
  assert.equal(said.length, 3, "a wait was not said");
  assert.ok(said.every(w => w.failures.length === 1), "a wait did not say what the source did");
});

await t("**NO GIVE-UP: 200 rounds of 404 still wait; only a person's cancel ends it, by name**", async () => {
  const net = node([404]);
  const c = clock();
  const ctl = new AbortController();
  let rounds = 0;
  const p = served({ url: "/never" }, { fetch: net.fetch, sleep: c.sleep, now: c.now, signal: ctl.signal, onWait: () => { rounds += 1; if (rounds === 200) ctl.abort(); } });
  await assert.rejects(p, /\/never: cancelled after \d+ s unanswered/);
  assert.ok(net.asked() >= 200, `gave up after ${net.asked()} asks`);
  assert.ok(c.waits.every(ms => ms <= RTO_SCHEDULE_MS.at(-1)), "a wait past the RTO's ceiling");
});

await t("a `check` every source refuses as FINAL ends it, naming the refusal; one refused only once is waited on", async () => {
  const bytes = new TextEncoder().encode("wrong");
  const c = clock();
  let seen = 0;
  // Final only on the second sight from the same source (the torn-read case).
  const check = async () => { seen += 1; return { says: "not it", answer: seen >= 2 }; };
  await assert.rejects(served({ urls: ["/a"] }, { fetch: node([bytes]).fetch, sleep: c.sleep, now: c.now, check, refusal: "a hash mismatch" }), /refused: a hash mismatch from every source/);
  assert.equal(c.waits.length, 1, "a first refusal was taken as final");
});

await t("url AND urls is refused rather than silently dropping one; no url at all is refused", async () => {
  await assert.rejects(served({ url: "/a", urls: ["/b"] }, { fetch: node([404]).fetch }), /both `url` and `urls`/);
  await assert.rejects(served({}, { fetch: node([404]).fetch }), /no url/);
});

await t("fetch OPTIONS go on every request, as given (a file read fresh: no-store)", async () => {
  const c = clock();
  const seen = [];
  const fetch = async (u, init) => { seen.push(init); return seen.length < 2 ? new Response("", { status: 404 }) : new Response("x"); };
  await served({ url: "/b" }, { fetch, sleep: c.sleep, now: c.now, init: { cache: "no-store" } });
  assert.deepEqual(seen, [{ cache: "no-store" }, { cache: "no-store" }], "an option was dropped on a re-ask");
  const plain = [];
  await served({ url: "/b" }, { fetch: async (u, ...rest) => { plain.push(rest.length); return new Response("x"); } });
  assert.deepEqual(plain, [0], "a request with no options was given some");
});

await t("servedText is the same fetch, decoded", async () => {
  const c = clock();
  assert.equal(await servedText({ url: "/j" }, { fetch: node([404, new TextEncoder().encode('{"a":1}')]).fetch, sleep: c.sleep, now: c.now }), '{"a":1}');
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
