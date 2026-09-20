// Provisioning a node from a page, and connecting to it.
//
// Like `connection.js`, this makes NO decisions. It fetches three files,
// hands them to Rust, and drives two clocks. What to send, in what order,
// whether an ack answers the step in flight, whether the delegate is
// provisioned at all — every one of those is in `wire`, tested against a
// transport that reorders, duplicates and drops, and against a node.

import { connect } from "./connection.js";

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
export async function openSession(Session, { port = 7509, artefacts = null, tickMs = 1000, onEvent = () => {} } = {}) {
  const session = new Session(port);

  if (artefacts) {
    // Fetched in parallel and awaited TOGETHER: a partial set is not a
    // smaller provisioning, it is one that installs a delegate it cannot
    // then give contract code to.
    const [delegate, block, register] = await Promise.all(
      [artefacts.delegate, artefacts.block, artefacts.register].map(async url => {
        const r = await fetch(url);
        if (!r.ok) throw new Error(`could not fetch ${url}: ${r.status}`);
        return new Uint8Array(await r.arrayBuffer());
      }),
    );
    session.provision(delegate, block, register);
  }

  const conn = connect(session, {
    url: session.url(),
    onEvent: e => {
      // A new socket means the stream ids restart, so a half-received
      // chunked reply from the old one must not be completed with bytes
      // from this one.
      if (e.kind === "open") session.reconnected();
      onEvent(e);
    },
  });

  const timer = setInterval(() => {
    const report = JSON.parse(session.tick());
    if (report.stalled) onEvent({ kind: "stalled", step: report.stalled });
    if (report.rolledBack > 0) onEvent({ kind: "rolledBack", count: report.rolledBack });
    const done = JSON.parse(session.take_progress());
    for (const step of done) onEvent({ kind: "provisioned", step });
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
    close: () => { clearInterval(timer); conn.close(); },
  };
}
