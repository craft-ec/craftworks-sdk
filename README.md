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
