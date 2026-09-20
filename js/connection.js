// The socket, and nothing else.
//
// This file owns a WebSocket and moves bytes. It makes NO decisions: retry,
// outbox order, what to do with a Lost write, when a binding reloads, the
// pending cap, rolling back a write that never got a verdict — all of that is
// in Rust, where it is tested against a transport that reorders, duplicates
// and drops replies.
//
// The rule is worth stating because the pressure runs the other way: it is
// always easier to "just handle this one case here", and a decision that ends
// up in this file is a decision nothing tests. If something here looks like it
// is choosing, it belongs in the engine.
//
// What it does do:
//   * send what `takeOutbound()` gives it
//   * feed `onInbound()` what arrives
//   * reconnect, and re-assert the head subscriptions on the way back
//   * resolve the promise for a request when its answer arrives

/** Open a connection to a node's client API and drive an engine over it. */
export function connect(engine, { url, delegateKey, onEvent = () => {} } = {}) {
  let ws = null, closed = false, backoff = 250;
  const watched = new Set();          // head contracts to re-assert on reconnect
  let pumping = false;

  const send = bytes => {
    if (ws?.readyState !== 1) return false;
    try { ws.send(bytes); return true; } catch (_) { return false; }
  };

  /**
   * Move what the engine has, and TELL IT HOW MUCH ACTUALLY WENT.
   *
   * `outbound()` does not give the queue up — `sent(n)` does, after the bytes
   * are on the socket. The first version of this used the draining form and
   * broke out of the loop on a failed send, so everything it had taken and
   * not sent was dropped on the floor. Its comment said the engine still had
   * them. It did not: the drain had already happened.
   *
   * That is not an edge case. A write made before `onopen` — the ordinary
   * start of every session — went straight into the gap, and the symptom
   * would have been a first write that silently never happened.
   */
  const pump = () => {
    if (pumping) return;              // re-entrancy, not a lock: onmessage pumps too
    pumping = true;
    try {
      const batch = engine.outbound();
      let n = 0;
      // A closed socket sends nothing and CONFIRMS nothing, so the whole
      // batch stays with the engine for the next pump.
      while (n < batch.length && send(batch[n])) n += 1;
      if (n > 0) engine.sent(n);
    } finally { pumping = false; }
  };

  const open = () => {
    if (closed) return;
    ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      backoff = 250;
      // Subscriptions are CLIENT state: the node's copy outlives the engine's
      // context and can be evicted without anyone being told, so they are
      // re-asserted on every connect. Re-subscribing is idempotent at the
      // node, so this costs nothing when nothing was lost.
      for (const head of watched) subscribeHead(head);
      onEvent({ kind: "open" });
      pump();
    };

    // The heads this connection intends to watch, so a test — and a caller —
    // can see that a reconnect re-asserts them.
    ws.onmessage = e => {
      const bytes = new Uint8Array(e.data);
      engine.on_inbound(bytes);
      onEvent({ kind: "message" });
      pump();                          // a reply may unblock the next request
    };
    ws.onclose = () => {
      onEvent({ kind: "closed" });
      if (closed) return;
      // Reconnect with a backoff. Nothing is decided about the WRITES here —
      // they are still the engine's, and it hands them back when asked.
      setTimeout(open, backoff);
      backoff = Math.min(backoff * 2, 10_000);
    };
    ws.onerror = () => onEvent({ kind: "error" });
  };

  /**
   * Remember that a tree's head should be watched.
   *
   * **It does not subscribe yet, and it must not pretend to.** The notifier
   * that reaches a tab which made no write is a CLIENT-API subscription — an
   * engine-originated push returns to whoever invoked the delegate, so it
   * cannot (F40) — and a client-API request is a `ClientRequest`, framed the
   * way the node's own client library frames it.
   *
   * This file cannot produce that frame. The first version invented one
   * (`JSON.stringify({subscribe, delegate})`) on a socket carrying the native
   * encoding; nothing would have accepted it, and the failure would have been
   * a subscription that silently never existed — which is indistinguishable,
   * from inside the app, from a tree that simply stopped changing.
   *
   * So the intention is RECORDED and the fact that nothing is subscribed is
   * reported, rather than a frame being guessed. What is missing is a
   * client-API encoder on this side; the Rust half of the notifier is built
   * and tested (sdk#46). Where that encoder lives is a decision, not work:
   * the SDK's boundary gate keeps `freenet-stdlib` out of its normal
   * dependencies on purpose.
   */
  const subscribeHead = head => {
    watched.add(head);
    onEvent({ kind: "head-watch-pending", head, why: "no client-API encoder on this side yet" });
    return false;
  };

  open();
  return {
    pump,
    subscribeHead,
    get watching() { return [...watched]; },
    get connected() { return ws?.readyState === 1; },
    close() { closed = true; ws?.close(); },
  };
}
