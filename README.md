# craftworks-sdk

App-facing SDK. Every substrate capability is written once in Rust and exposed to
JavaScript the phase it lands: `collection` (1) · `identity` `query` `subscribe` (3) ·
`inbox` `edge` `stream` (6) · `cap` (8) · `file` `blob` (9) · `ledger` (10).

    ./build.sh                     # → pkg/web (ES module) and pkg/node (CommonJS)
    cargo test                     # Rust side of the shared vectors
    npm test                       # the same behaviour through JavaScript

## Collections

```js
import { load } from "./sdk/index.js";
const sdk = await load();
const db = new sdk.Db();                         // in-memory for now
db.define("tasks", { type: "Task", fields: [
  { name: "title", kind: "text", required: true }, { name: "done", kind: "bool" }] });
const t = db.put("tasks", { title: "hello" });   // { id, created, updated, fields }
db.update("tasks", t.id, { done: true });
db.scan("tasks", { reverse: true, limit: 50 });  // newest first
```

- Field kinds: `text int float bool time bytes ref`. A write that violates the schema is refused.
- **Schemas only grow** — append optional fields; nothing else. Old records stay valid, old readers ignore new fields: no migrations.
- Record ids are 16 bytes (ms ‖ device ‖ tail), time-sortable, strictly increasing per generator.
- Everything sits on a four-operation `Store` (get · put · delete · scan). Today an in-memory map; the prolly tree, then the node's engine, plug in behind it.

`tests/vectors.txt` is checked from both languages, so a binding that mangles
bytes on the way in or out cannot pass both. Needs `wasm-bindgen-cli` 0.2.128
(pinned to the crate version in `Cargo.toml`).

## What a build says it is

```js
sdk.buildInfo();
// { version: "0.1.0", rev: "a178e3a", prollyRev: "afcada3", formatTag: "PT01" }
```

Every field comes from the build, and each from the artefact that actually
decides it:

| field | where it comes from | why that source |
|---|---|---|
| `version` | `CARGO_PKG_VERSION` | the crate's own |
| `rev` | `src/build_rev.txt` inside a `git archive`, else `git rev-parse` (with `-dirty`) | see below — it must not be passed in |
| `prollyRev` | `Cargo.lock` | the manifest says what was ASKED for; the lock says what was RESOLVED, and only that was compiled |
| `formatTag` | `freenet_prolly::node::MAGIC` | a tag this crate spelled itself would keep saying `PT01` the day the library moved on |

**Why `rev` is stamped rather than passed in.** A consumer pins a revision and
copies the built wasm into its own tree; the check worth having is "are these
bytes from the revision I asked for". If the consumer handed us its own pinned
rev to bake, both sides of that comparison would come from the same value and a
stale wasm would still match. So the rev comes from the source: `git archive`
rewrites `$Format:%H$` in `src/build_rev.txt` for files marked `export-subst`,
which is how a tree with no `.git` still knows its own commit.

That mechanism fails silently — lose `.gitattributes` and `rev` becomes
`unknown` — and the unit tests cannot see it, because they run in a checkout
where `git rev-parse` answers and the placeholder is never used. So it has its
own gate, which builds the way a consumer builds:

    ./tests/archive-provenance.sh

**Do not carry this into a contract.** Baking a rev into a wasm changes that
wasm's hash on every commit. That is free here, because this wasm is not
addressed by its hash — but a contract's wasm hash is part of its network key.
