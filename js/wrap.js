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
  }
  // `blockId` and nothing beside it: an app is given ids that ADDRESS
  // something. The raw module also exposes `contentHash` (plain BLAKE3, which
  // fetches nothing) for the frozen vectors; it is not part of this surface.
  // `parseBlockId` is the other half of `blockId`: an id is self-describing so
  // that whoever RECEIVES it can check it. It throws on anything that is not a
  // block id, so callers that merely want to test a string should catch.
  const parseBlockId = (text) => JSON.parse(raw.parseBlockId(text));
  return { version: raw.version, blockId: raw.blockId, parseBlockId, Db };
}
