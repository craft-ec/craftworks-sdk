// open() HANDS OVER A DB THAT CAN BE READ (sdk#234): when it provisions the
// node, it returns only once the node says it is provisioned — or fails
// saying why. Measured on the notes site: a read straight after an open()
// that returned early failed as UNAVAILABLE every time.
import assert from "node:assert/strict";
import { open, untilProvisioned } from "../../js/session.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack}\n`); }
};
const tick = (r, ms) => setTimeout(r, 0);

/** A session that is provisioned after `after` polls, or refuses, or never is. */
function FakeSession({ after = 3, refuse = "", exhaust = false } = {}) {
  let polls = 0;
  return function () {
    return {
      url: () => "ws://127.0.0.1:1/", outbound: () => [], sent() {}, on_inbound: () => true, unowned() {}, set_app() {},
      reconnected() {}, take_progress: () => "[]", take_loads: () => "[]", tick_ms: () => 1000, tick: () => "{}",
      unsaved_writes: () => 0, cold_due_ms: () => -1, cold_tick() {}, flush() {}, provision() {},
      provisioned: () => (polls += 1) > after && !refuse && !exhaust, refused: () => refuse, exhausted: () => exhaust,
      polls: () => polls,
    };
  };
}
const deps = extra => ({
  port: 7999, app: "test-app", artefacts: { signer: "s", block: "b", register: "r" },
  fetch: async () => ({ ok: true, arrayBuffer: async () => new Uint8Array([1]).buffer }),
  connect: () => ({ pump() {}, close() { deps.closed = (deps.closed ?? 0) + 1; } }),
  setInterval: () => 0, clearInterval() {}, setTimeout: tick, clearTimeout() {},
  addEventListener: null, removeEventListener: null, documentOf: null, provisionEveryMs: 0, ...extra,
});

await t("**open() returns only once the node says it is provisioned**", async () => {
  const S = FakeSession({ after: 3 });
  const h = await open(S, deps());
  assert.ok(h.session.polls() > 3, `open() returned after ${h.session.polls()} polls, before the node was provisioned`);
  assert.ok(h.db, "no db");
});

await t("a REFUSAL ends it in the node's words, and the session is closed", async () => {
  deps.closed = 0;
  await assert.rejects(() => open(FakeSession({ refuse: "a second install" }), deps()), /the node refused to set up: a second install/);
  assert.equal(deps.closed, 1, "the socket of a failed open() was left open");
});

await t("EXHAUSTED and the BUDGET each end it, saying which", async () => {
  // The socket OPENS here: this is the signer failing to answer, not a node
  // that is not there — the two are told apart by the test below.
  const opens = () => ({ connect: (_e, { onEvent } = {}) => { onEvent?.({ kind: "open" }); return { pump() {}, close() {} }; } });
  await assert.rejects(() => open(FakeSession({ exhaust: true }), deps(opens())), /the signer is not answering/);
  let clock = 0;
  await assert.rejects(() => untilProvisioned({ provisioned: () => false, connectedOnce: () => true }, { provisionBudgetMs: 5000, provisionEveryMs: 1000, now: () => (clock += 1000), setTimeout: tick }),
    /did not finish setting up in 5 s/);
});

await t("**a node that is NOT RUNNING is named as that, not as a silent signer**", async () => {
  // Nothing ever opened: page-io spends its re-asks exactly as it would
  // against a node that ignores them, so only the socket tells them apart.
  const S = FakeSession({ exhaust: true });
  await assert.rejects(() => open(S, deps({ connect: () => ({ pump() {}, close() {} }) })),
    /nothing answered at ws:\/\/127\.0\.0\.1:1\/: no connection was ever made — is the node running\?/);
  // THE CONTROL: the same exhaustion, with a socket that DID open, is the
  // signer's failure and says so.
  await assert.rejects(() => open(S, deps({ connect: (_e, { onEvent } = {}) => { onEvent?.({ kind: "open" }); return { pump() {}, close() {} }; } })),
    /the signer is not answering/);
});

await t("**a node that is not there is named at once: two refusals, never opened — not the budget**", async () => {
  // The clock moves 1 ms a poll: without the refusal end this reaches the
  // BUDGET's message, not this one — the assertion then fails, never hangs.
  let c = 0;
  const frozen = { provisionBudgetMs: 2_000, provisionEveryMs: 1, now: () => (c += 1), setTimeout: r => setTimeout(r, 0) };
  await assert.rejects(() => untilProvisioned({ provisioned: () => false, connectedOnce: () => false, refusedBeforeOpen: () => 2, url: () => "ws://127.0.0.1:7999/" }, frozen),
    /nothing answered at ws:\/\/127\.0\.0\.1:7999\/: the connection was refused and never opened — is the node running\?/);
  // THE CONTROL: one refusal is a connection still being made — it waits.
  let polls = 0;
  const one = untilProvisioned({ provisioned: () => (polls += 1) > 3, connectedOnce: () => false, refusedBeforeOpen: () => 1 }, frozen);
  await one;
  assert.ok(polls > 3, "one refusal ended the wait");
  // And through open(): a connect that reports two closes with no open.
  const S = FakeSession({ after: 1e9 });
  const refused = { connect: (_e, { onEvent } = {}) => { onEvent?.({ kind: "error" }); onEvent?.({ kind: "closed" }); return { pump() {}, close() {} }; } };
  await assert.rejects(() => open(S, deps(refused)), /the connection was refused and never opened/);
});

await t("the BUDGET says which too: never connected, or connected and still setting up", async () => {
  let clock = 0;
  const tick = r => setTimeout(r, 0);
  const budget = { provisionBudgetMs: 5000, provisionEveryMs: 1000, now: () => (clock += 1000), setTimeout: tick };
  await assert.rejects(() => untilProvisioned({ provisioned: () => false, connectedOnce: () => false, url: () => "ws://127.0.0.1:7999/" }, budget),
    /nothing answered at ws:\/\/127\.0\.0\.1:7999\/ in 5 s: no connection was ever made/);
  clock = 0;
  await assert.rejects(() => untilProvisioned({ provisioned: () => false, connectedOnce: () => true }, budget),
    /did not finish setting up in 5 s/);
});

await t("THE CONTROL: provision: false does not wait — nothing is being set up", async () => {
  const S = FakeSession({ after: 1e9 });
  const h = await open(S, deps({ provision: false }));
  assert.equal(h.session.polls(), 0, "open() waited on provisioning it was told not to do");
});

if (failures) { process.stdout.write(`${failures} failed\n`); process.exit(1); }
