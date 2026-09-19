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
  return { version: raw.version, cidHex: raw.cidHex, Db };
}
