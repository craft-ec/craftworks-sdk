// Provisioning a node from a page, and connecting to it.
//
// Like `connection.js`, this makes NO decisions. It fetches three files,
// hands them to Rust, and drives two clocks. What to send, in what order,
// whether an ack answers the step in flight, whether the signer is
// provisioned at all — every one of those is in Rust (`page-io`, `wire`), tested against a
// transport that reorders, duplicates and drops, and against a node.

import { connect } from "./connection.js";
import { engineDb } from "./engine-db.js";
import { allArtefactBytes, artefactBytes } from "./artefacts.js";

/**
 * The artefacts as `build.sh` ships them, beside this file.
 *
 * Named here rather than by each caller: they are the SDK's own build output,
 * and a page that spelled one of them wrong would fetch a 404, hand in an
 * empty array and provision a signer with no contract code behind it.
 */
export const SHIPPED_ARTEFACTS = {
  signer: new URL("./signer.wasm", import.meta.url).href,
  block: new URL("./block.wasm", import.meta.url).href,
  register: new URL("./register.wasm", import.meta.url).href,
};

/**
 * The shipped artefacts WITH their hashes, read from `artefacts.json`.
 *
 * The hashed form is what can be SHARED: every app on a node is a path on one
 * origin, so an artefact cached under its content hash by one app is found by
 * the next without a download (sdk#5). A bare URL cannot be shared, because
 * two apps shipping identical bytes have two URLs and no way to know they
 * match — and it cannot be verified either.
 *
 * `SHIPPED_ARTEFACTS` stays as plain URLs for callers that pass them
 * straight through; this is the form to prefer.
 */
export async function shippedArtefacts(
  fetchWith = typeof fetch === "function" ? fetch : null,
  base = import.meta.url,
  {
    /**
     * The web contract that HOLDS the artefacts, if there is one.
     *
     * An app that names this carries none of the four files itself: they are
     * published ONCE on the network and every app points at the same bytes
     * (§19, sdk#108). ~1.7 MB out of every published app.
     *
     * Not baked in at SDK build time, because the key does not exist until
     * somebody publishes the artefacts — whoever publishes an app knows it
     * and passes it here.
     */
    artefactsKey = null,
    /** The origin serving this page; the artefacts contract is on the same node. */
    origin = typeof location === "object" ? location.origin : null,
  } = {},
) {
  const url = new URL("./artefacts.json", base).href;
  const r = await fetchWith(url);
  if (!r.ok) throw new Error(`could not fetch ${url}: ${r.status}`);
  const m = await r.json();
  // WHERE THE SAME BYTES CAN BE GOT, in the order to try.
  //
  // The hash is the identity, so a second source costs nothing in trust: it
  // cannot serve anything different, because anything different does not
  // hash to this. What it buys is that one unavailable source does not kill
  // every app at once — which is the coupling a single hardcoded artefacts
  // contract would otherwise introduce.
  //
  // The contract first when there is one, because that is the copy shared by
  // every app on the node and therefore the one most likely to be cached
  // already. The file beside this module second, which is what a development
  // build has and a published app does not — so for a published app it is a
  // 404 that costs one request and is skipped.
  //
  // NO CIRCULARITY: `/v1/contract/web/<key>/<file>` is a plain HTTP GET to
  // the node already serving this page. It resolves no Block contract, so it
  // needs none of the four artefacts to fetch the four artefacts.
  // NAMING A KEY IS A REQUIREMENT, NOT A PREFERENCE.
  //
  // An app names an artefacts contract because it carries none of the four
  // files itself. So a key with no origin to build a url from is not a case
  // to degrade quietly: the only remaining source is a file the app does not
  // ship, it 404s, and the failure names a missing LOCAL file — pointing at
  // the app's own bundle when the real fault is that there was nowhere to ask.
  // Fatal and misleading together, which is worse than either.
  if (artefactsKey && !origin) {
    throw new Error(
      `artefacts contract ${artefactsKey} was named but there is no origin to ` +
        "fetch it from. An app that names a contract carries none of the " +
        "artefacts itself, so this cannot fall back to the files beside it.",
    );
  }
  const sources = (file) => {
    const out = [];
    if (artefactsKey) {
      out.push(`${origin}/v1/contract/web/${artefactsKey}/${file}`);
    }
    out.push(new URL("./" + file, base).href);
    return out;
  };
  const at = name => {
    const e = m[name];
    if (!e || !e.sha256 || !e.file) {
      throw new Error(`artefacts.json has no usable ${name} entry`);
    }
    return { urls: sources(e.file), sha256: e.sha256 };
  };
  return {
    signer: at("signer"),
    block: at("block"),
    register: at("register"),
    sdk: at("sdk"),
  };
}

/**
 * Open a session against a node on THIS machine, provisioning it if needed.
 *
 * `artefacts` are URLs for the three wasm files the page ships beside its
 * own — `SHIPPED_ARTEFACTS` is where `build.sh` puts them. This is the DEVELOPMENT path: the long-term shape fetches both
 * contracts from the network by hash (§19, sdk#5).
 *
 * Pass no artefacts to connect to a node that is already provisioned — the
 * page then never sends a byte of contract code, and `provisioned()` still
 * answers, because the signer is asked rather than assumed.
 */
/** Trees one session reads at once: each is its own engine in this page. */
export const MAX_OPEN_TREES = 32;
/** Tree subscriptions on one socket: F57 allows 500, and the rest is headroom. */
export const MAX_TREE_SUBSCRIPTIONS = 400;

export async function openSession(Session, {
  // NO DEFAULT. Publishing provisions a signer and hands it a signing key,
  // so which node receives one is a decision, and a default makes it by
  // accident — it already did: a screenshot run in the builder, meant to
  // capture "there is no node running", connected to a node that was
  // listening on the default port and began provisioning it.
  //
  // This is not a refusal list. A person's own app legitimately targets
  // their own node, whatever port it is on. It is only that nobody gets to
  // skip saying which.
  port,
  artefacts = null,
  // Provision this person's OWN tree on the node (install the engine and
  // hand it a key). FALSE opens the socket and installs NOTHING: a visitor who
  // only reads other people's trees (`tree`) leaves no trace on the node, and
  // their own tree is not created until they have something to write
  // (builder#104). The artefacts are still used — `tree` needs the Block code.
  provision = true,
  // NO LITERAL. The rate comes from the session, which reads it from
  // `protocol`, because the page sends the time and the engine's deadlines
  // are counted in that unit — a 1000 written here would go on being right
  // until the day the other side moved. `null` means "ask the session";
  // a test passes a number.
  tickMs = null,
  onEvent = () => {},
  // Injected so the wiring can be tested without a node and without a
  // browser. A test that could only run against a real socket would not run,
  // and the wiring is exactly the part that must not be taken on trust.
  connect: connectWith = connect,
  fetch: fetchWith = (typeof fetch === "function" ? fetch : null),
  setInterval: everyMs = setInterval,
  clearInterval: stopEvery = clearInterval,
  // The cold reader's one-shot timer (sdk#212's follow-up), injected like the
  // interval so a test can drive it.
  setTimeout: afterMs = setTimeout,
  clearTimeout: stopAfter = clearTimeout,
  // The page lifecycle, injected for the same reason the timer is: the
  // wiring is the part that must not be taken on trust, and a test that
  // needed a real browser tab to close would not run.
  addEventListener: onWindow = (typeof addEventListener === "function" ? addEventListener : null),
  removeEventListener: offWindow = (typeof removeEventListener === "function" ? removeEventListener : null),
  // What a page reads to know it is being hidden rather than shown.
  documentOf = (typeof document === "object" ? document : null),
} = {}) {
  const session = new Session(port);
  // A tree reader's stream-id range, 1..=255 and never reused while open.
  let rangeCursor = 0;
  const nextRange = () => {
    for (let i = 0; i < 255; i += 1) {
      rangeCursor = (rangeCursor % 255) + 1;
      if (![...trees].some(t => t.range === rangeCursor)) return rangeCursor;
    }
    throw new Error("tree(): no stream range free");
  };

  // The Block code, once fetched: a tree reader (`tree`) needs it to name
  // the blocks it GETs, and fetches it itself when provisioning did not.
  let blockCode = null;
  const blockBytes = async () => {
    if (blockCode) return blockCode;
    const b = artefacts?.block;
    if (!b) throw new Error("tree(): no Block code — open the session with artefacts");
    if (typeof b === "object" && b.sha256) {
      blockCode = await artefactBytes(b, { fetch: fetchWith });
    } else {
      const r = await fetchWith(b);
      if (!r.ok) throw new Error(`could not fetch ${b}: ${r.status}`);
      blockCode = new Uint8Array(await r.arrayBuffer());
    }
    return blockCode;
  };

  if (artefacts && provision) {
    // Fetched in parallel and awaited TOGETHER: a partial set is not a
    // smaller provisioning, it is one that provisions a signer it cannot
    // then give contract code to.
    // TWO SHAPES, and the difference is whether the bytes can be shared.
    //
    // `{ url, sha256 }` goes through the content-addressed cache: verified
    // before use, and found without a download by the next app on this node.
    // A bare URL string is fetched per app, unverified and unshared — kept
    // because callers pass `SHIPPED_ARTEFACTS` straight through, and because
    // a caller who has no hash must not get a cache entry nobody can check.
    const hashed = ["signer", "block", "register"].every(
      k => artefacts[k] && typeof artefacts[k] === "object" && artefacts[k].sha256,
    );
    let signer, block, register;
    if (hashed) {
      ({ signer, block, register } = await allArtefactBytes(artefacts, {
        fetch: fetchWith,
      }));
    } else {
      [signer, block, register] = await Promise.all(
        [artefacts.signer, artefacts.block, artefacts.register].map(async url => {
          const r = await fetchWith(url);
          if (!r.ok) throw new Error(`could not fetch ${url}: ${r.status}`);
          return new Uint8Array(await r.arrayBuffer());
        }),
      );
    }
    blockCode = block;
    session.provision(signer, block, register);
  }

  // THE UNSAVED-CHANGES GUARD (craftworks-sdk#163).
  //
  // A write is safe from a closing tab only once it is PUBLISHED. Until then
  // — sent, accepted, or HELD by the outbox's window, which the node has
  // never seen — closing the tab loses it. So while any write is unsaved the
  // page asks the browser's standard "leave site?" question, and the moment
  // the last one publishes it stops asking. Registered only while needed: a
  // `beforeunload` listener left on a page with nothing to lose costs it the
  // back/forward cache and asks a question with no reason behind it.
  //
  // And the page is told the count as it changes — `{ kind: "saving", count }`
  // — so it can say "saving N…" until the count is 0, never "saved" at
  // `Accepted`.
  const onBeforeUnload = e => {
    e.preventDefault?.();
    // The legacy form some browsers still need to show the prompt.
    e.returnValue = "";
    return "";
  };
  let guarded = false;
  let saving = 0;
  const guard = () => {
    const n = session.unsaved_writes();
    if (n > 0 && !guarded && onWindow) {
      onWindow("beforeunload", onBeforeUnload);
      guarded = true;
    } else if (n === 0 && guarded) {
      offWindow?.("beforeunload", onBeforeUnload);
      guarded = false;
    }
    if (n !== saving) {
      saving = n;
      onEvent({ kind: "saving", count: n });
    }
  };

  // THE TREES THIS PERSON READS (sdk#239): each is its own reader Session —
  // the same type, the same read path, with writing switched off — sharing
  // this ONE socket. `{ session, drain }`, in the order they were opened.
  const trees = new Set();
  // Subscriptions this SOCKET carries: the person's own head, plus one per
  // tree opened since it connected. The client API has no unsubscribe
  // (stdlib 0.10), so a closed tree's subscription is held until the socket
  // reconnects — counted, not forgotten (F57: 500 per connection).
  let treeSubscriptions = 0;

  // THE SOCKET'S ENGINE: every session on it. Each frame out, in order —
  // the person's own first; each frame in is offered to every session, and
  // each takes only what it asked for (by contract id). A frame nobody took
  // is counted once, on the person's own session. Mechanics only: which
  // frame is whose is decided in Rust (`on_inbound`).
  let lastBatch = [];
  const members = () => [session, ...[...trees].map(t => t.session)];
  const socketEngine = {
    outbound() {
      lastBatch = [];
      const out = [];
      for (const m of members()) {
        const b = m.outbound();
        lastBatch.push([m, b.length]);
        out.push(...b);
      }
      return out;
    },
    sent(n) {
      for (const [m, len] of lastBatch) {
        const k = Math.min(n, len);
        if (k > 0) m.sent(k);
        n -= k;
        if (n <= 0) break;
      }
    },
    on_inbound(bytes) {
      let owned = false;
      for (const m of members()) owned = m.on_inbound(bytes) || owned;
      if (!owned) session.unowned();
    },
  };

  const conn = connectWith(socketEngine, {
    url: session.url(),
    onEvent: e => {
      // A new socket means the stream ids restart, so a half-received
      // chunked reply from the old one must not be completed with bytes
      // from this one.
      if (e.kind === "open") {
        session.reconnected();
        for (const t of trees) t.session.reconnected();
        treeSubscriptions = trees.size;
      }
      // A message arrived and has been handed to the session: any load it
      // completed can now wake the reads parked on it. On the task that
      // handled the message, not on a timer.
      if (e.kind === "message") { drainReads(); for (const t of trees) t.drain(); guard(); armCold(); }
      onEvent(e);
    },
  });

  // THE COLD READER'S OWN TIMER. Its fetches time out at an RTO of ≈ 100 ms –
  // 2 s; the tick below is a second apart, and with no answer arriving at all
  // a late fetch would wait for it. So after anything that can change what is
  // in flight — a message, a tick, this timer itself — the one-shot timer is
  // set again for exactly when the earliest fetch is due.
  let coldTimer = null;
  let closed = false;
  const armCold = () => {
    if (coldTimer !== null) stopAfter(coldTimer);
    coldTimer = null;
    if (closed) return;
    // The EARLIEST due across every session on the socket: a tree's reader
    // re-asks on its RTO exactly as the person's own session does.
    const dues = members().map(m => m.cold_due_ms()).filter(d => d >= 0);
    if (!dues.length) return;
    const due = Math.min(...dues);
    coldTimer = afterMs(() => {
      coldTimer = null;
      // A timer the host fired after close() (or could not cancel) does
      // nothing: the session is done.
      if (closed) return;
      for (const m of members()) m.cold_tick();
      drainReads();
      for (const t of trees) t.drain();
      conn.pump();
      armCold();
    }, due);
  };

  // What wakes a parked read. `connection.js` calls back on every message;
  // this is how the session's "that load ended" reaches the promise waiting
  // on it. Set by `engineDb` when one is built over this session.
  let drainReads = () => {};

  const timer = everyMs(() => {
    const report = JSON.parse(session.tick());
    if (report.rolledBack > 0) onEvent({ kind: "rolledBack", count: report.rolledBack });
    // M2 (sdk#148): a write refused because what it READ had moved. The row
    // is rolled back and shows the current version; this says why.
    for (const c of report.conflicts ?? []) onEvent({ kind: "conflict", ...c });
    // sdk#225b: rows of a saved write that another device replaced.
    for (const s of report.superseded ?? []) onEvent({ kind: "superseded", ...s });
    // sdk#143/#144: a conflicted update was RE-RUN on the new version. Only
    // what it could not keep is told: fields changed elsewhere too
    // ("dropped"), a record deleted elsewhere, or a named failure.
    for (const r of report.reruns ?? []) onEvent({ kind: "rerun", ...r });
    const done = JSON.parse(session.take_progress());
    for (const step of done) onEvent({ kind: "provisioned", step });
    // The tick is also where a load that nobody answered is given up on, so
    // the reads parked on it are woken with a fact rather than left hanging.
    // Nothing else exercises this path: every other route delivers a message.
    drainReads();
    for (const t of trees) { t.session.tick(); t.drain(); }
    // A write rolled back at its timeout is no longer unsaved either.
    guard();
    // AND THE FRAME HAS TO LEAVE.
    //
    // `session.tick()` QUEUES a `Tick` for the in-page engine; `pump` is the only
    // thing that puts bytes on the socket, and the socket otherwise pumps on
    // open and on a message. A tick on an idle connection produces neither,
    // so without this the time the page generates every second would sit in
    // the outbox until something else happened to send — which, on the quiet
    // connection where a stuck commit actually lives, is nothing.
    conn.pump();
    armCold();
  }, tickMs ?? session.tick_ms());

  // THE LAST THING THIS PAGE SAYS.
  //
  // A tab that goes away stops ticking, and the engine coalesces on the
  // assumption that another tick is coming. Owed parity would sit unwritten
  // until somebody opened the app again — which, for the tab that made the
  // writes, may be never.
  //
  // Both events, because neither is reliable alone: `pagehide` is what fires
  // on a real navigation away and on mobile, `visibilitychange` is what fires
  // when a tab is backgrounded or the phone is locked, and a page can be
  // discarded from the background without ever firing the first. A `Flush`
  // is idempotent, so firing both costs one frame the engine answers with
  // nothing to do.
  //
  // `flush()` only QUEUES the frame; `pump` is what puts it on the socket,
  // and it has to happen in the same task — a page being unloaded gets no
  // second one.
  const flush = () => {
    session.flush();
    conn.pump();
  };
  const onHide = () => { if (documentOf?.visibilityState === "hidden") flush(); };
  const listeners = [];

  if (onWindow) {
    onWindow("pagehide", flush);
    listeners.push(["pagehide", flush]);
    // On the DOCUMENT: `visibilitychange` does not fire on `window` in every
    // browser, and a listener nothing calls is the failure this whole file
    // exists to avoid.
    if (documentOf?.addEventListener) {
      documentOf.addEventListener("visibilitychange", onHide);
    } else {
      onWindow("visibilitychange", onHide);
      listeners.push(["visibilitychange", onHide]);
    }
  }

  return {
    session,
    provisioned: () => session.provisioned(),
    // This person's own session writes; a tree handle (below) is read-only.
    readOnly: () => session.read_only(),
    // The head this session stands on, as `tree()` takes it (hex; "" until
    // Identity has named it). What a publisher records so others can read it.
    headId: () => session.head_id(),
    /**
     * READ SOMEBODY'S TREE (sdk#239): the data forest, one tree per identity.
     * `registerId` is that tree's head Register (their `headId()`). Returns
     * `{ db, headId, close }`: `db` is the same engine-backed surface as this
     * session's own, reading through the SAME path, and refusing every write.
     * Nothing is installed on the node; the head is watched on this socket.
     *
     * BOUNDED, and a refusal past either bound says which: MAX_OPEN_TREES
     * engines at once (memory), and MAX_TREE_SUBSCRIPTIONS per socket (F57).
     */
    tree: async registerId => {
      if (closed) throw new Error("tree(): this session is closed");
      if (trees.size >= MAX_OPEN_TREES) {
        throw new Error(`tree(): ${MAX_OPEN_TREES} trees are already open, each its own engine — close one first`);
      }
      if (treeSubscriptions >= MAX_TREE_SUBSCRIPTIONS) {
        throw new Error(
          `tree(): this connection already carries ${treeSubscriptions} tree subscriptions ` +
          `(the node allows 500 per connection, F57, and closed ones are held until it reconnects)`);
      }
      const block = await blockBytes();
      const reader = new Session(port);
      const range = nextRange();
      reader.open_named(block, registerId, range);
      const t = { session: reader, drain: () => {}, range };
      trees.add(t);
      treeSubscriptions += 1;
      const db = engineDb({ session: reader, onReadsWake: fn => { t.drain = fn; } });
      conn.pump();
      armCold();
      return {
        db,
        headId: () => reader.head_id(),
        close: () => {
          if (!trees.delete(t)) return;
          reader.free();
        },
      };
    },
    // The engine delegate's install plan: gone with it. Always false on the
    // page path (the signer's refusals are in `unusable()`); kept so a
    // caller that asks is not broken.
    exhausted: () => session.exhausted(),
    refused: () => session.refused(),
    unusable: () => JSON.parse(session.unusable()),
    // `engineDb` registers its drain here, so a completed load reaches the
    // reads waiting on it. Without this every read that missed the cache
    // waits for ever — which is why `open()` exists and why a page should
    // not be wiring this by hand.
    onReadsWake: fn => { drainReads = fn; },
    // `engineDb` calls this after every write it makes, so the guard is
    // armed in the same task as the write — not at the next message.
    wrote: guard,
    /// Writes made here and not yet published, held ones included.
    unsaved: () => session.unsaved_writes(),
    /// Ship what is waiting, now. Wired to the page lifecycle above; exposed
    /// because an app that knows it is finishing can say so sooner.
    flush,
    close: () => {
      // FLUSH BEFORE THE SOCKET GOES. A close is a page saying it is done,
      // and it is the one moment the frame can still be sent.
      flush();
      stopEvery(timer);
      closed = true;
      if (coldTimer !== null) { stopAfter(coldTimer); coldTimer = null; }
      if (offWindow) for (const [name, fn] of listeners) offWindow(name, fn);
      if (guarded) { offWindow?.("beforeunload", onBeforeUnload); guarded = false; }
      documentOf?.removeEventListener?.("visibilitychange", onHide);
      conn.close();
    },
  };
}

/**
 * THE ONE CALL A PAGE MAKES.
 *
 *   const { db } = await open(Session, { artefacts: SHIPPED_ARTEFACTS });
 *   const rows = await db.scan("tasks");
 *
 * Everything between those two lines is wiring, and wiring is the part an app
 * must not be trusted to get right. A parked read is woken by `engineDb`s
 * `drain`, and `drain` has to be called on every message AND on every tick —
 * miss the first and a cold read waits for ever, miss the second and a read
 * whose data never arrives waits for ever too. Neither failure says anything:
 * the screen simply does not fill.
 *
 * So the SDK does it. `engineDb(session)` stays exported for tests, where
 * driving the parts separately is the whole point.
 */
export async function open(Session, opts = {}) {
  if (!Number.isInteger(opts.port) || opts.port <= 0 || opts.port > 65535) {
    throw new Error(
      "open() needs a port: which node this connects to is a decision, and " +
      "a default would eventually make it by accident");
  }
  const handle = await openSession(Session, opts);
  // Registers its own wake-up with the handle. The page never sees `drain`.
  const db = engineDb(handle);
  // A DB THAT CAN BE READ. When this call provisions the node, the db is
  // handed over only once the node says it is provisioned: a read made before
  // that has no engine to answer it and fails as UNAVAILABLE ("the range this
  // read needed could not be loaded"). Measured on the notes site (sdk#234):
  // define-then-scan straight after open() failed every time; waiting for
  // `provisioned()` first, all five of its checks passed. The builder's
  // publish always waited; an app should not have to know to.
  if (opts.artefacts && opts.provision !== false) {
    try {
      await untilProvisioned(handle, opts);
    } catch (e) {
      handle.close();
      throw e;
    }
  }
  return { ...handle, db };
}

/**
 * Until the node says it is provisioned — or that it cannot be. Three ends,
 * never one timeout: refused (the signer's or the node's words), exhausted
 * (page-io's re-asks are spent), or the budget.
 */
export async function untilProvisioned(handle, {
  provisionBudgetMs = 60_000, provisionEveryMs = 100,
  now: clock = () => Date.now(),
  setTimeout: afterMs = setTimeout,
} = {}) {
  const started = clock();
  for (;;) {
    if (handle.provisioned()) return;
    const refused = handle.refused?.();
    if (refused) throw new Error(`the node refused to set up: ${refused}`);
    if (handle.exhausted?.()) throw new Error("the signer is not answering: every re-ask was spent and the node is still not provisioned");
    if (clock() - started > provisionBudgetMs) {
      throw new Error(`the node did not finish setting up in ${Math.round(provisionBudgetMs / 1000)} s; is it running?`);
    }
    await new Promise(r => afterMs(r, provisionEveryMs));
  }
}
