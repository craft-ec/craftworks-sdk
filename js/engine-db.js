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

/** Are these the same rows? By id and `updated`, as the in-memory one does. */
const same = (a, b) => {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i].id !== b[i].id || a[i].updated !== b[i].updated) return false;
  }
  return true;
};

export function engineDb(handle) {
  // Either the object `openSession` returns, or a bare session. The wrapper
  // is what knows when a message arrived, so when there is one this registers
  // with it and a parked read is woken by the answer rather than by a clock.
  const session = handle.session ?? handle;
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

  // Woken by the page, on the task that handled the message.
  handle.onReadsWake?.(() => { drain(); reloadStale(); });

  // Bindings this app is showing, by domain, so a head move can re-run them.
  const bound = new Map();   // domain -> Set<callback>

  // EVERY binding on a domain, live or not. Distinct from `bound`, which is
  // only the LIVE ones — a plain binding takes out no watch, and that is the
  // whole cost difference between them. But a plain binding must still see
  // its own client's writes: `live` decides whether somebody ELSE'S change
  // reaches you, never whether your own does.
  const mine = new Map();    // domain -> Set<binding.reload>

  /** This client changed `domain` itself: re-run every binding on it. */
  const touched = domain => { for (const r of mine.get(domain) ?? []) r(); };

  /**
   * The head moved: re-run the bindings the SESSION says are stale.
   *
   * Which ones is Rust's decision, not this file's. Reloading "everything"
   * would turn one write anywhere into a full refetch of every screen, and
   * guessing would miss the domain that changed.
   *
   * This is what makes LIVE mean anything. Without it a HeadChanged set a
   * flag nothing read, and a tab that made no write found out on its tick —
   * so the live arm of an acceptance would have been the tick's number
   * wearing a different name.
   */
  const reloadStale = () => {
    let changed;
    try { changed = JSON.parse(session.take_stale()); } catch (_) { return; }
    for (const domain of changed) for (const cb of bound.get(domain) ?? []) cb();
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

  // Named so `bind` can reach the surface it is part of.
  const self = {
    // ---- writes: no reload can help, so they are not retried ----
    async define(domain, schema) {
      try { return session.define(domain, JSON.stringify(schema)); } catch (e) { rethrow(e); }
    },
    async put(domain, fields) {
      let r;
      try { r = JSON.parse(session.put(domain, JSON.stringify(fields))); } catch (e) { rethrow(e); }
      // A person's OWN write shows at once. It changes `base + pending` and
      // not the root, so nothing else would tell this binding — and a row
      // that appeared only after the network confirmed it would make the
      // optimistic copy pointless.
      touched(domain);
      return r;
    },
    async update(domain, id, patch) {
      let r;
      try { r = JSON.parse(session.update(domain, id, JSON.stringify(patch))); } catch (e) { rethrow(e); }
      touched(domain);
      return r;
    },
    async delete(domain, id) {
      let r;
      try { r = session.delete(domain, id); } catch (e) { rethrow(e); }
      touched(domain);
      return r;
    },

    // ---- reads: a NOT_LOADED queues a load, waits for it, asks once more ----
    schema:  domain  => once(() => JSON.parse(session.schema(domain))),
    domains: ()      => once(() => JSON.parse(session.domains())),
    get:     (d, id) => once(() => JSON.parse(session.get(d, id))),
    count:   d       => once(() => session.count(d)),
    scan: (domain, { reverse = false, limit = 0, after = "" } = {}) =>
      once(() => JSON.parse(session.scan(domain, reverse, limit, after))),

    /**
     * A BINDING: one domain, as a component consumes it.
     *
     * The same shape the in-memory `Db` gives — `subscribe`, a referentially
     * stable `getSnapshot`, an awaited `reload` — because "Publish switches
     * the backend and the app code does not change" has to be true of the
     * object a component actually holds, not only of the database it came
     * from.
     *
     * It was NOT true. `engineDb` had no `bind` at all, so a published
     * project threw `db.bind is not a function` the moment it rendered. The
     * surface gate did not see it: that compares the two WASM objects, and
     * `bind` lives on the JavaScript wrapper.
     *
     * `live` is the only difference, and it is a declaration, not a mode: a
     * live binding is re-run when the head moves, a plain one when it is
     * reloaded. Both have the same snapshot and the same subscribe, so
     * flipping it never changes the component.
     */
    bind(domain, { live = false } = {}) {
      let rows = [], root = null;
      const listeners = new Set();
      const b = {
        get live() { return live; },
        // The SAME array until the rows change: a caller re-rendering on
        // every identity change would re-render for ever otherwise.
        getSnapshot: () => rows,
        subscribe: cb => { listeners.add(cb); return () => listeners.delete(cb); },
        /**
         * Bring the rows up to date.
         *
         * It ASKS. The first version compared the copy's root and returned
         * early when it had not moved — and the copy's root moves only when a
         * delta or a page arrives, so a reload that only read could never
         * show anything new. Nothing sent `ChangesSince`, so the root never
         * moved, and a tab that made no write could never see another's.
         *
         * The answer comes back through `drain`, which re-runs this binding
         * if the delta moved anything. So this returns whether the ROWS THIS
         * CLIENT CAN SEE changed — which includes its own pending writes,
         * because those are visible before any engine has confirmed them.
         */
        async reload() {
          session.refresh_domain(domain);
          const next = await self.scan(domain);
          root = self.root();
          if (same(rows, next)) return false;
          rows = next;
          for (const cb of listeners) cb();
          return true;
        },
      };
      // Its own client's writes reach it whatever `live` says.
      const rerun = () => { b.reload(); };
      if (!mine.has(domain)) mine.set(domain, new Set());
      mine.get(domain).add(rerun);

      // A LIVE binding is additionally re-run when the session says this
      // domain is stale — that is, when somebody else changed it. A plain one
      // takes out no watch at all, which is the whole cost difference.
      const unwatch = live ? self.watch(domain, rerun) : null;
      b.stop = () => {
        unwatch?.();
        const set = mine.get(domain);
        set?.delete(rerun);
        if (set && set.size === 0) mine.delete(domain);
      };
      b.reload();
      return b;
    },

    /**
     * Watch a domain: `cb` runs when the head moves and this domain is
     * stale. Returns an unsubscribe.
     *
     * The session is TOLD which domains are bound, because it is the thing
     * that decides what a head move makes stale.
     */
    watch(domain, cb) {
      if (!bound.has(domain)) { bound.set(domain, new Set()); session.bind(domain); }
      bound.get(domain).add(cb);
      return () => {
        const cbs = bound.get(domain);
        cbs?.delete(cb);
        if (cbs && cbs.size === 0) { bound.delete(domain); session.unbind(domain); }
      };
    },

    /**
     * How this session finds out the head moved — and when it is polling,
     * WHY. A page that believed it was being notified while it was polling
     * is the failure this exists to prevent.
     */
    liveMode: () => JSON.parse(session.live_mode()),

    // THE PAGE CALLS THIS after handing a message to the session, and after
    // each tick. It is how a parked read learns its load is done, and how a
    // stale binding learns the head moved. Without it every read that missed
    // the cache waits for ever.
    drain: () => { drain(); reloadStale(); },

    // The tree's root, or "" when this client cannot state one. EMPTY is not
    // a zero root: a zero root reads as a real, empty database.
    root: () => session.root(),
    // blocks/bytes/height are NULL here — they are properties of the tree,
    // which lives on the node. Zero would render as a real empty database.
    stats: () => JSON.parse(session.stats()),

    // ---- what only the engine-backed one has ----

    // Load the DOMAINS this project names on open, before anything asks for
    // them, so a first read is answered from memory rather than from a round
    // trip.
    //
    //   await db.preload(["tasks", "notes"]);
    //
    // Domains, not key ranges: what a range is, is the SDK's business. A
    // caller that built one would be encoding the key layout, and a layout
    // baked into apps could never change afterwards.
    //
    // A domain this project does not define is REFUSED, and refused before
    // anything is sent — a manifest naming a domain that has drifted out of
    // the project is wrong, and skipping it quietly makes the page
    // mysteriously slow instead of saying so.
    preload: domains => {
      try { return session.preload(JSON.stringify(domains)); }
      catch (e) { rethrow(e); }
    },

    // The call tree of the last write, in the instrument's fixed vocabulary.
    // No key, value or domain name crosses this — the same recording ships in
    // a user's app and is what a support bundle is made of.
    trace: () => JSON.parse(session.trace()),
    traceOn: on => session.trace_on(on),
  };
  return self;
}
