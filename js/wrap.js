import { openSession, SHIPPED_ARTEFACTS } from "./session.js";
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
    define(domain, schema) { this.#db.define(domain, JSON.stringify(schema)); }
    schema(domain) { return JSON.parse(this.#db.schema(domain)); }
    domains() { return JSON.parse(this.#db.domains()); }
    put(domain, fields) { return JSON.parse(this.#db.put(domain, JSON.stringify(fields))); }
    update(domain, id, patch) { return JSON.parse(this.#db.update(domain, id, JSON.stringify(patch))); }
    get(domain, id) { return JSON.parse(this.#db.get(domain, id)); }
    delete(domain, id) { return this.#db.delete(domain, id); }
    scan(domain, { reverse = false, limit = 0, after = "" } = {}) {
      return JSON.parse(this.#db.scan(domain, reverse, limit, after));
    }
    count(domain) { return this.#db.count(domain); }
    // The tree behind the database: its root hash, and what it holds.
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
    bind(domain, { live = false } = {}) { return new Binding(this, domain, live); }
  }

  // One domain's rows, with a referentially STABLE snapshot.
  //
  // The stability is the contract, not a nicety: useSyncExternalStore
  // re-renders whenever getSnapshot() is not the same object as last time, so
  // a binding that rebuilt its array on every call would re-render on every
  // render for ever, whatever the data did. The array is replaced only when
  // the rows differ.
  class Binding {
    #db; #domain; #live; #rows = []; #listeners = new Set(); #root = null;
    constructor(db, domain, live) {
      this.#db = db; this.#domain = domain; this.#live = live;
      // Bound once, so React sees the SAME function identity across renders;
      // a changing subscribe re-subscribes on every render.
      this.subscribe = this.subscribe.bind(this);
      this.getSnapshot = this.getSnapshot.bind(this);
      this.reload();
    }
    get live() { return this.#live; }
    // The rows, as the same array until they change.
    getSnapshot() { return this.#rows; }
    // subscribe(cb) -> unsubscribe. A NON-live binding takes one too: the
    // callback fires when these rows change, which is what the component
    // needs to know, and no subscription is taken out anywhere.
    subscribe(cb) { this.#listeners.add(cb); return () => this.#listeners.delete(cb); }
    // Bring the rows up to date. Cheap when nothing moved: the tree's root is
    // compared first, so a reload with nothing to do reads nothing.
    reload() {
      const root = this.#db.root();
      if (root === this.#root) return false;
      const rows = this.#db.scan(this.#domain);
      this.#root = root;
      if (same(this.#rows, rows)) return false;
      this.#rows = rows;
      for (const cb of this.#listeners) cb();
      return true;
    }
  }

  // Are these the same rows? Compared by id and updated stamp rather than
  // deeply: a record's contents cannot change without its `updated` moving,
  // and a deep compare of a long list on every reload is the cost this is
  // trying to avoid.
  function same(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) {
      if (a[i].id !== b[i].id || a[i].updated !== b[i].updated) return false;
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
    Db,
    Session: raw.Session,
    openSession,
    engineDb,
    SHIPPED_ARTEFACTS,
  };
}
