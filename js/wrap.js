import { openSession, open as openWith, SHIPPED_ARTEFACTS } from "./session.js";
import { engineDb } from "./engine-db.js";

// Plain-object API over the wasm surface. `raw` is the wasm-bindgen module.
//
//   const db = new sdk.Db();
//   db.define("tasks", { type: "Task", fields: [{ name: "title", kind: "text", required: true }] });
//   const t = db.put("tasks", { title: "hello" });      // → { id, created, updated, fields }
//   db.scan("tasks", { reverse: true, limit: 50 });     // → [records]
export function wrap(raw) {
  class Db {
    #db = new raw.Db();

    // ---- ASYNC, ALL OF THEM, AND ON PURPOSE ----
    //
    // Every one of these answers IN HAND: this database is in the tab, so
    // nothing here waits for anything. They are `async` so that the two
    // backends answer the same SHAPE.
    //
    // The promise this surface makes is "an app that awaits its reads works
    // on both", and it was not true. The engine-backed database reads over
    // the network, so its reads return promises; these returned values. An
    // app written against this one and then published got a Promise where it
    // expected an object — and a Promise is truthy, so it passes an
    // `if (!schema)` guard and arrives at `schema.fields.map` as
    // `undefined.map`.
    //
    // What that cost, measured against a real node (sdk#87): four defects in
    // the builder, three of which replaced the entire app with an exception
    // string while the button said "Published", and one of which printed
    // "— [object Promise] records" to a person — a wrong number, no crash, so
    // nothing would ever have reported it.
    //
    // Awaiting a value already in hand is free. Being unable to tell which
    // backend you have from the shape of an answer is the whole point.
    async define(domain, schema) { this.#db.define(domain, JSON.stringify(schema)); }
    async schema(domain) { return JSON.parse(this.#db.schema(domain)); }
    async domains() { return JSON.parse(this.#db.domains()); }
    async put(domain, fields) { return JSON.parse(this.#db.put(domain, JSON.stringify(fields))); }
    // A create at a slot the caller derived with `slotFrom`, never an
    // overwrite: → { outcome: "created" | "exists", record } (sdk#149).
    async createAt(domain, slot, fields) { return JSON.parse(this.#db.create_at(domain, slot, JSON.stringify(fields))); }
    async update(domain, id, patch) { return JSON.parse(this.#db.update(domain, id, JSON.stringify(patch))); }
    async get(domain, id) { return JSON.parse(this.#db.get(domain, id)); }
    async delete(domain, id) { return this.#db.delete(domain, id); }
    async scan(domain, { reverse = false, limit = 0, after = "" } = {}) {
      return JSON.parse(this.#db.scan(domain, reverse, limit, after));
    }
    /**
     * The children of one parent, as a bounded read.
     *
     * The app names the PARENT; it never builds a key range. The cost is this
     * parent's band and does not grow with the records under other parents,
     * which is the whole difference from a scan filtered by a field
     * (craftworks-sdk#122).
     */
    async children(domain, parent, { reverse = false, limit = 0, after = "" } = {}) {
      return JSON.parse(this.#db.children(domain, parent, reverse, limit, after));
    }
    async count(domain) { return this.#db.count(domain); }
    // SYNCHRONOUS on both backends, and that is not an oversight. A root is
    // a value this client already holds, and `stats` describes what THIS TAB
    // has — which is why the engine-backed one answers null for the figures
    // that describe the tree on the node rather than waiting to ask it.
    root() { return this.#db.root(); }
    stats() { return JSON.parse(this.#db.stats()); }

    // A BINDING: one domain, as a component consumes it.
    //
    //   const tasks = db.bind("tasks");
    //   const rows = useSyncExternalStore(tasks.subscribe, tasks.getSnapshot);
    //
    // That is the whole integration with React, and the same object is a
    // Svelte store (`subscribe` returning an unsubscribe) and drives a Vue
    // ref. No adapter, because the shape a framework wants IS this shape.
    //
    // `live` is the app author's declaration that this data changes while
    // someone is looking at it — a feed, a presence list, a document two
    // people have open. Default FALSE, and it is the only thing it changes:
    // a non-live binding has the same getSnapshot, the same subscribe and the
    // same reload, and takes out no subscription anywhere. Flipping it never
    // changes the component.
    //
    // Do not set it on ordinary data. A subscription is a standing cost paid
    // continuously, and it is worth it only where being told sooner is worth
    // something — data read once and shown is correct when it is read.
    bind(domain, { live = false, limit = 0, reverse = false } = {}) { return new Binding(this, domain, live, limit, reverse); }
  }

  // One domain's rows, with a referentially STABLE snapshot.
  //
  // The stability is the contract, not a nicety: useSyncExternalStore
  // re-renders whenever getSnapshot() is not the same object as last time, so
  // a binding that rebuilt its array on every call would re-render on every
  // render for ever, whatever the data did. The array is replaced only when
  // the rows differ.
  class Binding {
    #db; #domain; #live; #limit; #reverse; #rows = []; #listeners = new Set(); #root = null;
    // The same shape the engine-backed binding answers. This database is in
    // the tab, so a range is never unreachable here — but an app must not be
    // able to tell which backend it has from what a call returns (sdk#87),
    // and a component that branched on `status()` existing would work in a
    // preview and throw once published.
    #status = { state: "loading", why: "", code: "" };
    constructor(db, domain, live, limit = 0, reverse = false) {
      this.#db = db; this.#domain = domain; this.#live = live; this.#limit = limit; this.#reverse = reverse;
      // Bound once, so React sees the SAME function identity across renders;
      // a changing subscribe re-subscribes on every render.
      this.subscribe = this.subscribe.bind(this);
      this.getSnapshot = this.getSnapshot.bind(this);
      // NOT populated here. The engine-backed binding starts empty and fills
      // on its first `reload`, and a caller that could skip the reload on one
      // backend and not the other is the same trap one level up: it would
      // work in the builder's preview and show an empty table the moment the
      // project was published.
    }
    get live() { return this.#live; }
    // The same shape the engine-backed binding answers. `0` is unbounded.
    get limit() { return this.#limit; }
    // Which END a page comes from. A limit without a direction is half a page.
    get reverse() { return this.#reverse; }
    // The rows, as the same array until they change.
    getSnapshot() { return this.#rows; }
    /**
     * `{ state, why, code }` — `loading`, then `ready`.
     *
     * Never `unreachable`: this database is in the tab, so a range that was
     * read and holds nothing is EMPTY and provably so. That is the whole
     * point of the distinction — see the engine-backed binding, where the
     * third state is real.
     */
    status() { return this.#status; }
    // subscribe(cb) -> unsubscribe. A NON-live binding takes one too: the
    // callback fires when these rows change, which is what the component
    // needs to know, and no subscription is taken out anywhere.
    subscribe(cb) { this.#listeners.add(cb); return () => this.#listeners.delete(cb); }
    // Bring the rows up to date. Cheap when nothing moved: the tree's root is
    // compared first, so a reload with nothing to do reads nothing.
    async reload() {
      const root = this.#db.root();
      const rows = await this.#db.scan(this.#domain, { limit: this.#limit, reverse: this.#reverse });
      this.#root = root;
      const was = this.#status.state;
      this.#status = { state: "ready", why: "", code: "" };
      if (was !== "ready" && same(this.#rows, rows)) {
        // Rows unchanged, STATE changed: the first load of an empty domain
        // moves `loading` to `ready`, and a component watching only the rows
        // would sit on "loading" for ever.
        for (const cb of this.#listeners) cb();
        return true;
      }
      if (same(this.#rows, rows)) return false;
      this.#rows = rows;
      for (const cb of this.#listeners) cb();
      return true;
    }
  }

  // Are these the same rows? Compared by id, updated stamp and write STATE
  // rather than deeply: a record's contents cannot change without its
  // `updated` moving, and a deep compare of a long list on every reload is
  // the cost this is trying to avoid. `state` is not content and moves on its
  // own, which is exactly why it has to be here.
  function same(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      // `state` TOO, and it is not an optimisation detail.
    //
    // A write moves PENDING -> CLEAN when it reaches the network, and that
    // changes neither `id` nor `updated` — `updated` is the record's own
    // timestamp and a write state is not a content change. So a comparison
    // of those two alone says "the same rows", the snapshot is not replaced,
    // no listener fires, and the component never re-renders.
    //
    // MEASURED against a real node: a row sat on screen saying "saving" for
    // 70 seconds while `db.scan()` returned it CLEAN the whole time. The data
    // was safely published within a second; only the screen was wrong, which
    // is the worst version of this — a person watching a spinner is told
    // their data is unsaved when it is on the network, and closing the tab
    // then feels like losing it.
    if (a[i].id !== b[i].id || a[i].updated !== b[i].updated || a[i].state !== b[i].state) {
      return false;
    }
    }
    return true;
  }
  // `blockId` and nothing beside it: an app is given ids that ADDRESS
  // something. The raw module also exposes `contentHash` (plain BLAKE3, which
  // fetches nothing) for the frozen vectors; it is not part of this surface.
  // `parseBlockId` is the other half of `blockId`: an id is self-describing so
  // that whoever RECEIVES it can check it. It throws on anything that is not a
  // block id, so callers that merely want to test a string should catch.
  const parseBlockId = (text) => JSON.parse(raw.parseBlockId(text));
  // What this build IS: { version, rev, prollyRev, formatTag }. Every field is
  // baked into the wasm, so a caller that pins a revision can check that the
  // bytes it loaded came from the revision it asked for. Nothing here supplies
  // a default: a value this file invented would be a version display that can
  // be confidently wrong, which is worse than none.
  const buildInfo = () => JSON.parse(raw.buildInfo());
  // The ENGINE-BACKED path, beside the in-memory one. An app uses `Db`
  // until it is published and the same methods afterwards; the SDK's own
  // `surfaces_agree` gate fails if the two ever differ.
  //
  // Exposed from here rather than imported directly by an app, so there is
  // ONE place that knows how the SDK is assembled — the same reason `load`
  // exists.
  return {
    version: raw.version,
    buildInfo,
    blockId: raw.blockId,
    parseBlockId,
    // The deterministic slot for a record copied from a source (sdk#149).
    slotFrom: raw.slotFrom,
    Db,
    // THE ONE CALL AN APP MAKES.
    //
    //   const { db } = await sdk.open();
    //
    // Bound to this build's `Session` and its own shipped artefacts, so an
    // app cannot pair the wrong ones. `open` wires message -> on_inbound ->
    // drain and tick -> drain itself; an app that wired those by hand has a
    // screen that never fills the first time it forgets one, and nothing
    // that says why.
    open: (opts = {}) => openWith(raw.Session, { artefacts: SHIPPED_ARTEFACTS, ...opts }),
    SHIPPED_ARTEFACTS,
    // The halves, for tests that drive the parts and for a host running its
    // own loop. NOT app-facing: handing an app the two things it can wire
    // wrongly, beside the one call it cannot, is how the wiring gets done by
    // hand and gets done wrong.
    internals: { Session: raw.Session, openSession, engineDb },
  };
}
