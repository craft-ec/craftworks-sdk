// Provisioning a node from a page, and connecting to it.
//
// Like `connection.js`, this makes NO decisions. It fetches three files,
// hands them to Rust, and drives two clocks. What to send, in what order,
// whether an ack answers the step in flight, whether the delegate is
// provisioned at all — every one of those is in `wire`, tested against a
// transport that reorders, duplicates and drops, and against a node.

import { connect } from "./connection.js";
import { engineDb } from "./engine-db.js";

/**
 * The artefacts as `build.sh` ships them, beside this file.
 *
 * Named here rather than by each caller: they are the SDK's own build output,
 * and a page that spelled one of them wrong would fetch a 404, hand in an
 * empty array and install a delegate with no contract code behind it.
 */
export const SHIPPED_ARTEFACTS = {
  delegate: new URL("./engine_delegate.wasm", import.meta.url).href,
  block: new URL("./block.wasm", import.meta.url).href,
  register: new URL("./register.wasm", import.meta.url).href,
};

/**
 * Open a session against a node on THIS machine, provisioning it if needed.
 *
 * `artefacts` are URLs for the three wasm files the page ships beside its
 * own — `SHIPPED_ARTEFACTS` is where `build.sh` puts them. This is the DEVELOPMENT path: the long-term shape fetches both
 * contracts from the network by hash (§19, sdk#5).
 *
 * Pass no artefacts to connect to a node that is already provisioned — the
 * page then never sends a byte of contract code, and `provisioned()` still
 * answers, because the delegate is asked rather than assumed.
 */
export async function openSession(Session, {
  port = 7509,
  artefacts = null,
  tickMs = 1000,
  onEvent = () => {},
  // Injected so the wiring can be tested without a node and without a
  // browser. A test that could only run against a real socket would not run,
  // and the wiring is exactly the part that must not be taken on trust.
  connect: connectWith = connect,
  fetch: fetchWith = (typeof fetch === "function" ? fetch : null),
  setInterval: everyMs = setInterval,
  clearInterval: stopEvery = clearInterval,
} = {}) {
  const session = new Session(port);

  if (artefacts) {
    // Fetched in parallel and awaited TOGETHER: a partial set is not a
    // smaller provisioning, it is one that installs a delegate it cannot
    // then give contract code to.
    const [delegate, block, register] = await Promise.all(
      [artefacts.delegate, artefacts.block, artefacts.register].map(async url => {
        const r = await fetchWith(url);
        if (!r.ok) throw new Error(`could not fetch ${url}: ${r.status}`);
        return new Uint8Array(await r.arrayBuffer());
      }),
    );
    session.provision(delegate, block, register);
  }

  const conn = connectWith(session, {
    url: session.url(),
    onEvent: e => {
      // A new socket means the stream ids restart, so a half-received
      // chunked reply from the old one must not be completed with bytes
      // from this one.
      if (e.kind === "open") session.reconnected();
      // A message arrived and has been handed to the session: any load it
      // completed can now wake the reads parked on it. On the task that
      // handled the message, not on a timer.
      if (e.kind === "message") drainReads();
      onEvent(e);
    },
  });

  // What wakes a parked read. `connection.js` calls back on every message;
  // this is how the session's "that load ended" reaches the promise waiting
  // on it. Set by `engineDb` when one is built over this session.
  let drainReads = () => {};

  const timer = everyMs(() => {
    const report = JSON.parse(session.tick());
    if (report.stalled) onEvent({ kind: "stalled", step: report.stalled });
    if (report.rolledBack > 0) onEvent({ kind: "rolledBack", count: report.rolledBack });
    const done = JSON.parse(session.take_progress());
    for (const step of done) onEvent({ kind: "provisioned", step });
    // The tick is also where a load that nobody answered is given up on, so
    // the reads parked on it are woken with a fact rather than left hanging.
    // Nothing else exercises this path: every other route delivers a message.
    drainReads();
  }, tickMs);

  return {
    session,
    provisioned: () => session.provisioned(),
    // Everything was accepted and the delegate STILL cannot write a head.
    // A different fact from stalled and from refused, and a page says so
    // rather than spinning.
    exhausted: () => session.exhausted(),
    refused: () => session.refused(),
    unusable: () => JSON.parse(session.unusable()),
    // `engineDb` registers its drain here, so a completed load reaches the
    // reads waiting on it. Without this every read that missed the cache
    // waits for ever — which is why `open()` exists and why a page should
    // not be wiring this by hand.
    onReadsWake: fn => { drainReads = fn; },
    close: () => { stopEvery(timer); conn.close(); },
  };
}

/**
 * THE ONE CALL A PAGE MAKES.
 *
 *   const { db } = await open(Session, { artefacts: SHIPPED_ARTEFACTS });
 *   const rows = await db.scan("tasks");
 *
 * Everything between those two lines is wiring, and wiring is the part an app
 * must not be trusted to get right. A parked read is woken by `engineDb`s
 * `drain`, and `drain` has to be called on every message AND on every tick —
 * miss the first and a cold read waits for ever, miss the second and a read
 * whose data never arrives waits for ever too. Neither failure says anything:
 * the screen simply does not fill.
 *
 * So the SDK does it. `engineDb(session)` stays exported for tests, where
 * driving the parts separately is the whole point.
 */
export async function open(Session, opts = {}) {
  const handle = await openSession(Session, opts);
  // Registers its own wake-up with the handle. The page never sees `drain`.
  const db = engineDb(handle);
  return { ...handle, db };
}
