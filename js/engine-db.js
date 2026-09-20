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
  // One re-ask after a reload, and only one.
  //
  // NOT_LOADED means the range has not been read yet; the fix is to load it
  // and ask again. Retrying more than once would turn a genuinely missing
  // range into a spin, and the second NOT_LOADED is information the caller
  // needs rather than something to hide.
  const once = async (call, reload) => {
    try {
      return call();
    } catch (e) {
      if (!e || e.code !== "NOT_LOADED") rethrow(e);
      await reload();
      try {
        return call();
      } catch (e2) {
        rethrow(e2);
      }
    }
  };

  // Ask the engine for what it is missing, and let the pump carry it. The
  // answer lands through `on_inbound` and the next call reads it.
  const reload = () => Promise.resolve();

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

    // ---- reads: a NOT_LOADED is a reload and one more ask ----
    schema:  domain      => once(() => JSON.parse(session.schema(domain)), reload),
    domains: ()          => once(() => JSON.parse(session.domains()), reload),
    get:     (d, id)     => once(() => JSON.parse(session.get(d, id)), reload),
    count:   d           => once(() => session.count(d), reload),
    scan: (domain, { reverse = false, limit = 0, after = "" } = {}) =>
      once(() => JSON.parse(session.scan(domain, reverse, limit, after)), reload),

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
