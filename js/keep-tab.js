// A TAB'S PASS, PER ASSET (sdk#479 P3): KEEPER.md §12's table, as its ONE transition function. Pure: no page, no
// timer, no lock. `step(tab, event, world)` returns the next tab state and the EFFECTS to carry out; the Session's
// driver carries them out (page calls, `navigator.locks`) and feeds the answers back as events. Only `step` changes a
// tab's state (the one-writer rule; a source test enforces it).
//
// What is DERIVED, never stored (I5, I6): whether a pass runs is the asset's state being "running"; whether the tab's
// SLOT is free is "no asset asking or running AND no background op of the tab's pages on the wire", read from
// `world.backgroundInFlight` (the pages' `background_in_flight()`, summed), never kept here. SEAM: `Page::
// background_in_flight` is pub(crate) until engineer4's OP-LIFE lane rebuild (batch 7) makes the query public; until
// then the Session passes it through this `world` field, which the tests stub. When it lands it is OP-LIFE's
// `place() == Slot` per page (which counts a WithdrawnOnWire op), not `sent && lane == Background` (the architect).
//
// `navigator.locks` is a per-browser CONVENIENCE that saves ops on the one node (KEEPER §6), never correctness: with
// no locks the driver answers every ask at once with `granted`, and two tabs both run -- idempotently.

/** An asset's states: ONE list (KEEPER §12). Each case's data lives on the object; no parallel flags. */
export const STATES = Object.freeze(["idle", "queued", "asking", "running"]);
/** The events: ONE list, the one `step` switches over. A test drives every STATES × EVENTS cell. */
export const EVENTS = Object.freeze(["due", "head", "auditNow", "granted", "heldElsewhere", "ended", "cancel", "removed", "added", "poke"]);

/** A fresh tab: its assets, all idle, first full pass due at `now`. `order` breaks the scheduler's ties. */
export function tab(targets, now) {
  const assets = new Map(targets.map(t => [t, { k: "idle", nextFullAt: now, seenRoot: null }]));
  return { assets, order: [...targets], seq: 0, impossible: 0, unhandled: 0 };
}

const FULL = Object.freeze({ full: true });
const inc = oldRoot => ({ incremental: oldRoot });
const isFull = kind => kind.full === true;

/** Is the tab's slot free (I6)? Derived: no asset asking/running, and no background op of its pages on the wire. */
export function slotFree(t, world) {
  for (const s of t.assets.values()) if (s.k === "asking" || s.k === "running") return false;
  return world.backgroundInFlight === 0;
}

/**
 * THE TRANSITION FUNCTION. `event`: { e, target?, now?, root?, report? }. `world`: { backgroundInFlight, rootOf(target),
 * cadenceMs }. Returns { tab, effects }: effects are { do: "askLock" | "audit" | "auditSince" | "cancelAudit" |
 * "release" | "writeBack", target, oldRoot?, report? }.
 */
export function step(t, event, world) {
  const effects = [];
  const assets = new Map(t.assets);
  let impossible = t.impossible;
  // NO THROW IN THE BROWSER (the architect; #450's unreachable! argument): a state or event step() has no branch for is
  // COUNTED and named, and the state is returned unchanged -- one asset's gap never stops the tab's event handling.
  // Exhaustiveness is proved at TEST time (every STATES × EVENTS cell reaches a defined branch).
  let unhandled = t.unhandled ?? 0;
  const unknown = (s, what) => { unhandled += 1; console.error(`keep-tab: no branch for ${what} in state ${s?.k} (event ${event.e})`); return s; };
  let seq = t.seq;
  const want = (s, kind) => ({ k: "queued", kind, since: ++seq, nextFullAt: s.nextFullAt, seenRoot: s.seenRoot });
  const impossibleCell = s => { impossible += 1; return s; };
  const one = (target, fn) => {
    const s = assets.get(target);
    if (!s) {
      // A lock answer or a pass end for an asset that is gone (Removed while asking): impossible cell (a)/(b), counted.
      if (event.e === "granted" || event.e === "heldElsewhere" || event.e === "ended") impossible += 1;
      return;
    }
    const next = fn(s);
    if (next === null) assets.delete(target);
    else assets.set(target, next);
  };
  const oldest = (a, b) => (a === null ? b : a); // the OLDEST old root is the first one remembered
  switch (event.e) {
    case "due":
      for (const [target, s] of assets) {
        if (event.now < s.nextFullAt) continue;
        switch (s.k) {
          case "idle": assets.set(target, want(s, FULL)); break;
          case "queued": if (!isFull(s.kind)) assets.set(target, { ...s, kind: FULL }); break;
          case "asking": if (!isFull(s.kind)) assets.set(target, { ...s, kind: FULL }); break;
          case "running": break; // I3: the cadence never touches a running pass
          default: unknown(s, "due");
        }
      }
      break;
    case "head":
      one(event.target, s => {
        switch (s.k) {
          case "idle": return want(s, inc(s.seenRoot));
          case "queued":
          case "asking": return s; // Full: starts from the current root anyway; Incremental: keeps its (OLDEST) old root
          case "running": return { ...s, headMoved: oldest(s.headMoved, s.seenRoot) };
          default: return unknown(s, event.e);
        }
      });
      break;
    case "auditNow":
      one(event.target, s => {
        switch (s.k) {
          case "idle": return want(s, FULL);
          case "queued":
          case "asking": return isFull(s.kind) ? s : { ...s, kind: FULL }; // never lose the person's full request
          case "running": return s; // the button is disabled
          default: return unknown(s, event.e);
        }
      });
      break;
    case "granted":
      one(event.target, s => {
        switch (s.k) {
          case "asking": {
            const root = world.rootOf(event.target);
            effects.push(isFull(s.kind) ? { do: "audit", target: event.target } : { do: "auditSince", target: event.target, oldRoot: s.kind.incremental });
            return { k: "running", kind: s.kind, nextFullAt: s.nextFullAt, seenRoot: root, headMoved: null };
          }
          case "idle":
          case "queued":
          case "running": return impossibleCell(s); // (a)
          default: return unknown(s, event.e);
        }
      });
      break;
    case "heldElsewhere":
      one(event.target, s => {
        switch (s.k) {
          // Another tab covers it. Only a FULL ask's refusal moves the full cadence.
          case "asking": return { k: "idle", nextFullAt: isFull(s.kind) ? event.now + world.cadenceMs : s.nextFullAt, seenRoot: s.seenRoot };
          case "idle":
          case "queued":
          case "running": return impossibleCell(s); // (a)
          default: return unknown(s, event.e);
        }
      });
      break;
    case "ended":
      one(event.target, s => {
        switch (s.k) {
          case "running": {
            if (isFull(s.kind)) effects.push({ do: "writeBack", target: event.target, report: event.report }); // I4
            effects.push({ do: "release", target: event.target });
            const nextFullAt = isFull(s.kind) ? event.now + world.cadenceMs : s.nextFullAt;
            const idle = { k: "idle", nextFullAt, seenRoot: s.seenRoot };
            return s.headMoved === null ? idle : want(idle, inc(s.headMoved));
          }
          case "idle":
          case "queued":
          case "asking": return impossibleCell(s); // (b)
          default: return unknown(s, event.e);
        }
      });
      break;
    case "cancel":
      one(event.target, s => {
        switch (s.k) {
          case "running": effects.push({ do: "cancelAudit", target: event.target }, { do: "release", target: event.target }); return { k: "idle", nextFullAt: s.nextFullAt, seenRoot: s.seenRoot };
          case "queued": return { k: "idle", nextFullAt: s.nextFullAt, seenRoot: s.seenRoot };
          case "idle":
          case "asking": return s; // a person cancels a RUNNING pass; an ask's answer must still arrive in asking (a)
          default: return unknown(s, event.e);
        }
      });
      break;
    case "removed":
      one(event.target, s => {
        switch (s.k) {
          case "running": effects.push({ do: "cancelAudit", target: event.target }, { do: "release", target: event.target }); return null;
          case "idle":
          case "queued":
          case "asking": return null; // an asking asset's lock answer then arrives to nothing: counted as (a)
          default: return unknown(s, event.e);
        }
      });
      break;
    case "added":
      if (!assets.has(event.target)) assets.set(event.target, { k: "idle", nextFullAt: event.now, seenRoot: null });
      break;
    case "poke":
      break; // a page answered (maybe a background op ended): only the scheduler below runs
    default:
      unknown(null, `the unknown event ${event.e}`);
  }
  const order = event.e === "added" && !t.order.includes(event.target) ? [...t.order, event.target] : t.order.filter(x => assets.has(x));
  let next = { assets, order, seq, impossible, unhandled };
  // THE SCHEDULER (one function, not a state): the free slot goes to the OLDEST queued asset (ties: list order).
  if (slotFree(next, world)) {
    const queued = order.filter(x => assets.get(x)?.k === "queued").sort((a, b) => assets.get(a).since - assets.get(b).since);
    if (queued.length) {
      const target = queued[0], s = assets.get(target);
      assets.set(target, { k: "asking", kind: s.kind, nextFullAt: s.nextFullAt, seenRoot: s.seenRoot });
      effects.push({ do: "askLock", target });
      next = { ...next, assets };
    }
  }
  return { tab: next, effects };
}
