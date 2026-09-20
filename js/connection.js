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
    // A send on a socket that is not open is DROPPED, not queued here — the
    // engine still holds the request and hands it back on the next pump, so
    // a second queue in this file would be a second source of truth about
    // what is outstanding.
    if (ws?.readyState === 1) { ws.send(bytes); return true; }
    return false;
  };

  /** Move everything the engine has, once. */
  const pump = () => {
    if (pumping) return;              // re-entrancy, not a lock: onmessage pumps too
    pumping = true;
    try {
      for (const bytes of engine.take_outbound()) {
        if (!send(bytes)) break;      // the rest stays with the engine
      }
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
   * Watch a tree's head contract over the client API.
   *
   * This is the notifier that reaches a tab which made no write: an
   * engine-originated push returns to whoever invoked the delegate, so it
   * cannot. Measured, F40.
   */
  const subscribeHead = head => {
    watched.add(head);
    if (ws?.readyState !== 1) return;  // re-asserted on the next open
    ws.send(JSON.stringify({ subscribe: head, delegate: delegateKey }));
  };

  open();
  return {
    pump,
    subscribeHead,
    get connected() { return ws?.readyState === 1; },
    close() { closed = true; ws?.close(); },
  };
}
