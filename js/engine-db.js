// The SAME database API, backed by the ENGINE instead of by memory.
//
//   const db = engineDb(session);
//   await db.put("tasks", { title: "hello" });
//
// Every method is the one `wrap.js` gives the in-memory `Db`, and every one
// returns a Promise. That is the whole of what "Publish switches the backend
// and the app code does not change" means: an app that awaits its reads works
// on both, because awaiting a value that is already there is free.
//
// NOTHING HERE DECIDES ANYTHING. It moves JSON across the boundary, resolves
// promises, and re-asks once after a reload. What a read is allowed to answer,
// whether a range is loaded, what a write costs, which refusals are worth
// retrying — all of that is in Rust, where it is tested against a transport
// that reorders, duplicates and drops.

/// An error from the database, carrying the STABLE code Rust chose.
///
/// `code` is from a fixed list — NOT_LOADED, UNAVAILABLE, TOO_LARGE,
/// NOT_DEFINED, REFUSED. `message` is for a person to read.
///
/// **Nothing in this file, and nothing in an app, branches on `message`.**
/// A message is prose; rewording it would change behaviour, and it would
/// change it silently.
export class DbError extends Error {
  constructor({ code, message, transient }) {
    super(message);
    this.name = "DbError";
    this.code = code;
    // Whether a RELOAD is the recovery. Rust decides this, not the caller,
    // and not a list of codes kept in sync by hand over here.
    this.transient = transient;
  }
}

const rethrow = e => {
  // A thrown value carrying a `code` is one Rust built deliberately. Anything
  // else is a bug in this layer or in wasm-bindgen, and is passed through
  // unchanged rather than dressed up as a database error.
  if (e && typeof e === "object" && typeof e.code === "string") throw new DbError(e);
  throw e;
};

export function engineDb(session) {
  // Reads parked on a load, by ticket id. Resolved when the SESSION says the
  // load ended — which happens on the task that handles the websocket
  // message, not on a timer and not in a microtask.
  const parked = new Map();

  /**
   * Called by the page whenever a message has been handed to the session.
   *
   * This is what wakes a parked read. `connection.js` already calls back on
   * every message; that callback calls this. Nothing here polls, because a
   * poll either spins or answers late and neither is a fact about the data.
   */
  const drain = () => {
    for (const { id, ok, code } of JSON.parse(session.take_loads())) {
      const waiters = parked.get(id);
      if (!waiters) continue;
      parked.delete(id);
      for (const w of waiters) (ok ? w.resolve : w.reject)(code);
    }
  };

  /** Wait for the load this read is parked on. */
  const waitFor = ticket =>
    new Promise((resolve, reject) => {
      const list = parked.get(ticket) ?? [];
      list.push({ resolve, reject });
      parked.set(ticket, list);
    });

  // One re-ask after a load, and only one.
  //
  // The FIRST version of this awaited a resolved promise and asked again.
  // That asks again in a microtask — before any websocket message can
  // possibly have arrived — and it had requested nothing, because nothing
  // outside `preload` ever called `request_range`. So every read outside a
  // preload manifest rejected, always, and the test passed only because its
  // fake scripted the second call to succeed. The comment described the
  // design; the code did not implement it.
  //
  // A second NOT_LOADED is information the caller needs rather than a reason
  // to go round again: the load DID complete, and the range still does not
  // answer, which is a different fact from "not yet".
  // A cold read is a CHAIN, not one hop.
  //
  // `scan` reads its domain's schema before its rows, so the first
  // NOT_LOADED names the schema's key and the next names the range. So this
  // goes round as many times as the chain is long — and what must never
  // happen is going round WITHOUT PROGRESS.
  //
  // Progress is not counted here. Rust knows which spans it has already
  // loaded, and a read asking for one of them again is told so: the error
  // comes back with no ticket, and this stops. A count kept in JavaScript
  // would be a guess about a chain whose length it cannot see.
  //
  // The cap below is a backstop against a chain that is somehow both making
  // progress and unbounded. It is not the mechanism.
  const MAX_HOPS = 8;

  const once = async call => {
    for (let hop = 0; ; hop += 1) {
      let ticket;
      try {
        return call();
      } catch (e) {
        if (!e || e.code !== "NOT_LOADED") rethrow(e);
        // No ticket: either the session could not queue the load, or this
        // span was loaded already and the read still cannot be answered.
        // Nothing is coming, so waiting would be a hang.
        if (e.wait === undefined) rethrow(e);
        if (hop >= MAX_HOPS) rethrow(e);
        ticket = e.wait;
      }
      try {
        await waitFor(ticket);
      } catch (code) {
        // The load ENDED and did not deliver: the engine could not reach the
        // data, or nothing answered within the session's bound. Either way
        // it is not "not yet".
        throw new DbError({
          code: code ?? "UNAVAILABLE",
          message: "the range this read needed could not be loaded",
          transient: true,
        });
      }
    }
  };

  return {
    // ---- writes: no reload can help, so they are not retried ----
    async define(domain, schema) {
      try { return session.define(domain, JSON.stringify(schema)); } catch (e) { rethrow(e); }
    },
    async put(domain, fields) {
      try { return JSON.parse(session.put(domain, JSON.stringify(fields))); } catch (e) { rethrow(e); }
    },
    async update(domain, id, patch) {
      try { return JSON.parse(session.update(domain, id, JSON.stringify(patch))); } catch (e) { rethrow(e); }
    },
    async delete(domain, id) {
      try { return session.delete(domain, id); } catch (e) { rethrow(e); }
    },

    // ---- reads: a NOT_LOADED queues a load, waits for it, asks once more ----
    schema:  domain  => once(() => JSON.parse(session.schema(domain))),
    domains: ()      => once(() => JSON.parse(session.domains())),
    get:     (d, id) => once(() => JSON.parse(session.get(d, id))),
    count:   d       => once(() => session.count(d)),
    scan: (domain, { reverse = false, limit = 0, after = "" } = {}) =>
      once(() => JSON.parse(session.scan(domain, reverse, limit, after))),

    // THE PAGE CALLS THIS after handing a message to the session, and after
    // each tick. It is how a parked read learns its load is done. Without it
    // every read that missed the cache waits for ever.
    drain,

    // The tree's root, or "" when this client cannot state one. EMPTY is not
    // a zero root: a zero root reads as a real, empty database.
    root: () => session.root(),
    // blocks/bytes/height are NULL here — they are properties of the tree,
    // which lives on the node. Zero would render as a real empty database.
    stats: () => JSON.parse(session.stats()),

    // ---- what only the engine-backed one has ----

    // Load the ranges this project names on open, before anything asks for
    // them, so a first read is answered from memory.
    preload: manifest => session.preload(JSON.stringify(manifest)),

    // The call tree of the last write, in the instrument's fixed vocabulary.
    // No key, value or domain name crosses this — the same recording ships in
    // a user's app and is what a support bundle is made of.
    trace: () => JSON.parse(session.trace()),
    traceOn: on => session.trace_on(on),
  };
}
