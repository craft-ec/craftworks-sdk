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
/// NOT_DEFINED, REFUSED, and for a write the store refused before making it
/// (sdk#180): NO_ROOM, NO_ROOM_BYTES, TOO_LARGE_TO_SEND, NO_SESSION.
/// `message` is for a person to read.
///
/// **Nothing in this file, and nothing in an app, branches on `message`.**
/// A message is prose; rewording it would change behaviour, and it would
/// change it silently.
export class DbError extends Error {
  constructor({ code, message, transient, retryable = false, cap }) {
    super(message);
    this.name = "DbError";
    this.code = code;
    // Whether a RELOAD is the recovery. Rust decides this, not the caller,
    // and not a list of codes kept in sync by hand over here.
    this.transient = transient;
    // Whether the SAME write, made again after the node confirms earlier
    // ones, may succeed — a NO_ROOM. Rust decides this too (sdk#180).
    this.retryable = retryable;
    if (cap !== undefined) this.cap = cap;
  }
}

const rethrow = e => {
  // A thrown value carrying a `code` is one Rust built deliberately. Anything
  // else is a bug in this layer or in wasm-bindgen, and is passed through
  // unchanged rather than dressed up as a database error.
  if (e && typeof e === "object" && typeof e.code === "string") throw new DbError(e);
  throw e;
};

/**
 * Did the STATUS move? All three fields, because any of them is on screen.
 *
 * `state` alone is not enough: two failures with different reasons share a
 * state, and a caller showing `why` would display the first sentence for
 * ever. The same mistake as comparing rows without `state` (below) and as
 * comparing a binding by rows alone — three instances of one shape.
 */
const changed = (a, b) => a.state !== b.state || a.why !== b.why || a.code !== b.code;

/**
 * Is this the same VALUE? Objects by their keys in any order, lists in order,
 * everything else by `Object.is`-like identity of kind and value.
 */
const equal = (a, b) => {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  if (Array.isArray(a)) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (!equal(a[i], b[i])) return false;
    return true;
  }
  const keys = Object.keys(a);
  if (keys.length !== Object.keys(b).length) return false;
  for (const k of keys) if (!Object.hasOwn(b, k) || !equal(a[k], b[k])) return false;
  return true;
};

/**
 * Are these the same rows? EVERYTHING a row carries, not a subset of it.
 *
 * ONE definition, used by both backends (`wrap.js` imports this one). It was
 * two copies of the same comparison over `id`, `updated` and `state`, on the
 * reasoning that "a record's contents cannot change without its `updated`
 * moving". They can: `Db::update` stamps `now_ms().max(previous)`, so a second
 * edit in one millisecond, or any edit after the clock went backwards, keeps
 * `updated` where it was. The record was written, `reload()` answered false,
 * no listener fired and the screen kept the old text (craftworks-sdk#129).
 *
 * Fourth instance of one shape — a row's write state (#96), a range becoming
 * readable (#114), a status reason (#114's fix), and now content: each a
 * change invisible to a comparison over a SUBSET of what a person can see.
 * So this compares the whole row and names no field; a field added to a row
 * later is compared without anyone remembering to add it here.
 *
 * The cost is one walk of rows that `scan` has just `JSON.parse`d — the same
 * order of work, already paid once per reload.
 */
export const sameRows = (a, b) => {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (!equal(a[i], b[i])) return false;
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
  handle.onReadsWake?.(() => { drain(); reloadStale(); reloadOwnStates(); });

  // Bindings this app is showing, by domain, so a head move can re-run them.
  const bound = new Map();   // domain -> Set<callback>

  // EVERY binding on a domain, live or not. Distinct from `bound`, which is
  // only the LIVE ones — a plain binding takes out no watch, and that is the
  // whole cost difference between them. But a plain binding must still see
  // its own client's writes: `live` decides whether somebody ELSE'S change
  // reaches you, never whether your own does.
  const mine = new Map();    // domain -> Set<binding.reload>

  /**
   * This client changed `domain` itself: re-run every binding on it — and
   * tell the page a write was made, so its unsaved-changes guard is armed at
   * once rather than at the next message (craftworks-sdk#163).
   */
  const touched = domain => {
    handle.wrote?.();
    for (const r of mine.get(domain) ?? []) r();
  };

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

  /**
   * This client's OWN writes changed state (published, backed up, lost,
   * conflicted, superseded): re-run every binding of this client on those
   * domains, LIVE or not. Without it a plain binding showed "saving" until
   * something else re-rendered it (builder#107): its own write's state is
   * local knowledge about its own write, not a subscription, and costs no
   * network. LIVE still governs only somebody ELSE'S changes.
   */
  const reloadOwnStates = () => {
    let changed;
    try { changed = JSON.parse(session.take_state_changed()); } catch (_) { return; }
    for (const domain of changed) for (const r of mine.get(domain) ?? []) r();
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
          // NOT_ANSWERING: the page read it itself and the node stopped
          // answering — not that the data is missing.
          message: code === "NOT_ANSWERING"
            ? "the node is not answering"
            : "the range this read needed could not be loaded",
          transient: true,
        });
      }
    }
  };

  // Named so `bind` can reach the surface it is part of.
  const self = {
    // ---- writes: a REFUSAL is final, but "I could not read" is not ----
    //
    // These used to say "no reload can help, so they are not retried", and
    // that conflated two different failures. A write can fail because it is
    // invalid — a bad schema, a field of the wrong type, a domain that does
    // not exist — and no amount of loading changes that. It can also fail
    // because it had to READ something in order to apply itself and that
    // range was not loaded, which is the one case a load fixes.
    //
    // Every write here reads first: `define` reads the existing schema to
    // check the new one against it, and all of them go through `need_schema`.
    // So they take the SAME bounded path a read takes — `once` retries only
    // a ticketed NOT_LOADED, at most MAX_HOPS times, and a span already
    // loaded comes back with no ticket and is rethrown rather than asked
    // again.
    //
    // What the old heading cost (sdk#89): a cold `define` returned a
    // ticketless NOT_LOADED, so a published app kept a form on screen, a
    // button saying Published, and refused every write with `domain has no
    // schema; define it first` — permanently, because the next mount ran the
    // same cold define. 120 of 120 writes refused against a real node.
    //
    // Retrying cannot double-apply: in `Db` every one of these reads happens
    // before the single write that mutates, so a NOT_LOADED leaves the tree
    // untouched and asking again repeats the attempt, not the effect.
    async define(domain, schema) {
      return once(() => session.define(domain, JSON.stringify(schema)));
    },
    async put(domain, fields) {
      const r = await once(() => JSON.parse(session.put(domain, JSON.stringify(fields))));
      // A person's OWN write shows at once. It changes `base + pending` and
      // not the root, so nothing else would tell this binding — and a row
      // that appeared only after the network confirmed it would make the
      // optimistic copy pointless.
      touched(domain);
      return r;
    },
    // Reads the slot first, so a cold one parks and retries like any read;
    // nothing is written before it answers (sdk#149).
    async createAt(domain, slot, fields) {
      const r = await once(() => JSON.parse(session.create_at(domain, slot, JSON.stringify(fields))));
      touched(domain);
      return r;
    },
    async update(domain, id, patch) {
      const r = await once(() => JSON.parse(session.update(domain, id, JSON.stringify(patch))));
      touched(domain);
      return r;
    },
    async delete(domain, id) {
      const r = await once(() => session.delete(domain, id));
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
    // The children of one parent, bounded to that parent's band. Goes through
    // `once` like every other read, so a cold band queues a load and retries
    // rather than answering an empty list (craftworks-sdk#122).
    children: (domain, parent, { reverse = false, limit = 0, after = "" } = {}) =>
      once(() => JSON.parse(session.children(domain, parent, reverse, limit, after))),

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
    bind(domain, { parent = null, live = false, limit = 0, reverse = false } = {}) {
      let rows = [], root = null;
      // WHAT THIS BINDING WATCHES: the domain, or one parent's band of it
      // (sdk#137). Named by the session — the layout is Rust's — so a live
      // band is told of changes in its band, and not of one under a sibling.
      const key = parent ? session.watch_key(domain, parent) : domain;
      // WHAT THIS BINDING LAST MANAGED TO DO.
      //
      // A component holds a binding and reads `getSnapshot()`. Until now that
      // was all it could learn, so an empty array meant BOTH "the range was
      // read and holds nothing" and "the range could not be reached" — and a
      // component has no way to tell them apart.
      //
      // That distinction is the architecture's whole character: absence is
      // never provable (§3), a cold far read is seconds (F43), and the layers
      // underneath were built at real cost to keep the two apart —
      // `NotLoaded` is not empty (sdk#54, #63). A binding that flattens them
      // is the last place it can be lost, and an app saying "No records yet"
      // over a tree it merely could not reach wastes every honest answer
      // beneath it.
      //
      // `loading` until the first reload finishes, then `ready` or
      // `unreachable`. Empty is not a state here: it is `ready` with no rows,
      // which is exactly what makes it PROVABLE.
      let status = { state: "loading", why: "", code: "" };
      const listeners = new Set();
      const b = {
        get live() { return live; },
        /**
         * How many rows this binding reads. `0` is UNBOUNDED, and is what
         * every caller got before there was a choice.
         *
         * # A READ SHOULD COST WHAT THE SCREEN COSTS
         *
         * `reload` scanned the whole domain, every time, for every binding.
         * A component showing twenty rows read twenty thousand if the domain
         * held them, and `Scan { limit, after }` existed unused the whole
         * time — the third mechanism this week built, tested and never
         * called.
         *
         * Priced by F43 that is not a rounding error: a cold far read is
         * seconds, so a list over a cold domain fetched every record in it
         * to show a screenful.
         *
         * Unbounded stays the DEFAULT rather than a hidden 50, because a
         * caller that asked for everything and silently got a page would
         * draw a partial list and call it complete — which is the same
         * confusion between "all of it" and "what I could get" that
         * `NotLoaded` exists to prevent one layer down.
         */
        get limit() { return limit; },
        /**
         * WHICH END OF THE RANGE A PAGE COMES FROM.
         *
         * A limit without a direction is only half a page. Ids are
         * time-ordered, so the first `limit` rows of a FORWARD scan are the
         * OLDEST — and a caller that renders newest-first by reversing the
         * snapshot then shows the oldest page in reverse order, with the
         * newest records never on screen at all.
         *
         * Measured on the builder (craftworks-builder#51): past 50 records a
         * list showed notes 1..50 newest-first and note 51 onward never
         * appeared, though the form had accepted them. The page bounded the
         * read and silently changed WHICH rows it was a page OF.
         *
         * So the direction belongs to the binding, beside the limit, and not
         * to whoever reverses an array afterwards.
         */
        get reverse() { return reverse; },
        /** Whose children this reads, or `null` for the whole domain. */
        get parent() { return parent; },
        /**
         * `{ state, why, code }` — `loading`, `ready` or `unreachable`.
         *
         * # A BINDING'S OBSERVABLE STATE IS ROWS **AND** STATUS
         *
         * A comparison over rows alone misses a transition. Twice now:
         *
         * * a row whose write state moved `PENDING` -> `CLEAN` changes
         *   neither `id` nor `updated`, so the snapshot comparison said
         *   "the same rows" and a person watched "saving" for seventy
         *   seconds over data that was already on the network (sdk#96);
         * * a range going from `unreachable` to readable-and-empty is a
         *   different screen with identical rows, so anything watching only
         *   the rows leaves the error on screen for ever.
         *
         * Anything deciding whether a binding changed must read both.
         *
         * Read it beside `getSnapshot()`: no rows with `ready` is an empty
         * range, and no rows with `unreachable` is a range nobody could read.
         * `why` is the SDK's own sentence, and `code` its stable code, so a
         * caller can show the first and branch on the second.
         */
        status: () => status,
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
          session.refresh_domain(key);
          let next;
          try {
            // THE PAGE, not the domain. `limit: 0` is unbounded and is what
            // the scan has always done. With a parent, the PAGE OF ITS BAND.
            next = parent
              ? await self.children(domain, parent, { limit, reverse })
              : await self.scan(domain, { limit, reverse });
          } catch (e) {
            // UNREACHABLE IS AN ANSWER, not an exception to swallow.
            //
            // This rejected, and its own caller — `rerun`, below — called it
            // without awaiting, so the rejection was unhandled and the
            // component was told nothing at all. It kept whatever rows it had
            // and no one could see that the read had failed.
            const was = status;
            status = {
              state: "unreachable",
              why: String(e?.message ?? e),
              code: String(e?.code ?? ""),
            };
            // ALL THREE FIELDS, not just `state`.
            //
            // This compared `state` alone, so a binding that failed twice for
            // DIFFERENT reasons replaced `why` and `code` and told nobody —
            // and the doc above tells callers to show `why`, so a component
            // displayed a stale sentence for ever while the real reason
            // changed underneath it.
            //
            // Third instance of one shape, and this one inside the fix for
            // the second: a row's write state moving and the snapshot
            // comparison missing it (sdk#96), a range becoming readable and
            // the row comparison missing it (above), and now a reason
            // changing and the state comparison missing it. `state` is not
            // the whole of status, exactly as rows are not the whole of a
            // binding.
            if (changed(was, status)) for (const cb of listeners) cb();
            return false;
          }
          root = self.root();
          const was = status;
          status = { state: "ready", why: "", code: "" };
          if (sameRows(rows, next)) {
            // The ROWS did not change but the STATUS may have: a range that
            // was unreachable and is now readable-and-empty is a different
            // screen, and a component that only watched the rows would never
            // redraw.
            const moved = changed(was, status);
            if (moved) for (const cb of listeners) cb();
            return moved;
          }
          rows = next;
          for (const cb of listeners) cb();
          return true;
        },
      };
      // Its own client's writes reach it whatever `live` says.
      // The rejection is handled INSIDE `reload` now, which records it as a
      // state rather than throwing. This stays deliberately fire-and-forget:
      // it is called from a notification, and there is nobody to await it.
      const rerun = () => { b.reload(); };
      if (!mine.has(domain)) mine.set(domain, new Set());
      mine.get(domain).add(rerun);

      // A LIVE binding is additionally re-run when the session says this
      // domain is stale — that is, when somebody else changed it. A plain one
      // takes out no watch at all, which is the whole cost difference.
      const unwatch = live ? self.watch(key, rerun) : null;
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
    drain: () => { drain(); reloadStale(); reloadOwnStates(); },

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
