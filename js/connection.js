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
//   * reconnect (the page re-subscribes its trees itself: page-io, sdk#376)
//   * resolve the promise for a request when its answer arrives

/** Open a connection to a node's client API and drive an engine over it. */
export function connect(engine, { url, delegateKey, onEvent = () => {} } = {}) {
  let ws = null, closed = false, backoff = 250;
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

  // Messages from a replaced socket, dropped (see `onmessage`).
  let staleDropped = 0;
  const open = () => {
    if (closed) return;
    const sock = new WebSocket(url);
    ws = sock;
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      backoff = 250;
      onEvent({ kind: "open" });
      pump();
    };

    ws.onmessage = e => {
      // THE CURRENT CONNECTION'S FRAMES ONLY (sdk#490, OPENING.md Machine 3's S2): a frame read from a socket that has
      // been REPLACED is never handed to the engine. The node dropped that connection's subscriptions when it closed,
      // so a head move it carried must not "prove" the new connection's subscription. Counted and named, not used.
      // (The platform already orders a socket's messages before its close, and a new socket opens only from the old
      // one's onclose; this is the drop point OUR code owns, and a test holds it.)
      if (sock !== ws) {
        staleDropped += 1;
        onEvent({ kind: "stale" });
        return;
      }
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

  open();
  return {
    pump,
    get connected() { return ws?.readyState === 1; },
    /** Frames that arrived on a socket already replaced, and were dropped (sdk#490). */
    get staleDropped() { return staleDropped; },
    close() { closed = true; ws?.close(); },
  };
}
