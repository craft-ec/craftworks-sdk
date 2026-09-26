// THE TAB'S PASS DRIVER (sdk#479 P3; KEEPER.md §12). keep-tab.js decides; this carries its effects out and feeds the
// answers back as events. It owns no state of the table: the tab state is replaced only by `step`'s result (the
// one-writer rule; a source test enforces it here too). What it holds is the WORLD's: the locks the browser granted,
// and the timer.
//
// Every dependency is REQUIRED (builder#73: a dependency whose absence would skip a step has no default).
// - `page`: the Session's calls per asset: `audit(target)` (PageIo::audit(repair): a signer-less pass is decided at
//   start, so its report may be ready at once), `auditSince(target, oldRoot)`, `auditProgress(target)` (null = no pass
//   running), `cancelAudit(target)`, `takeAudit(target)`, `rootOf(target)`, and `backgroundInFlight()` (summed over
//   the tab's pages; the I6 seam until OP-LIFE).
// - `writeBack(target, report)`: keep_api::write_back's call site (it writes nothing for an unmeasured report).
// - `locks`: `navigator.locks`, or `null` for a browser without it. Null is said, never defaulted: without locks every
//   ask is granted at once and two tabs both run, idempotently (§12 "Without navigator.locks").
// - `clock`: `{ now(), every(ms, fn) -> stop }`.
// - `cadenceMs`: the full-pass cadence (provisional until §11(c)'s +24 h row sizes it).
// - `tickMs`: how often the cadence is looked at; `null` runs NO timer, so nothing starts on its own (the control).
//   The timer SCHEDULES and never ends anything (I3, rule 8).
import { step, tab as newTab } from "./keep-tab.js";

const LOCK_PREFIX = "craftworks-keep:";

export function keepDriver({ targets, page, writeBack, locks, clock, cadenceMs, tickMs }) {
  for (const [name, v] of Object.entries({ targets, page, writeBack, clock, cadenceMs })) {
    if (v === undefined || v === null) throw new Error(`keepDriver: ${name} is required`);
  }
  if (locks === undefined) throw new Error("keepDriver: locks is required (navigator.locks, or null for a browser without it)");
  if (tickMs === undefined) throw new Error("keepDriver: tickMs is required (null runs no timer)");

  let t = newTab(targets, clock.now());
  const held = new Map(); // target -> the granted lock's release (the world's, not the table's)
  const queue = [];
  let busy = false;
  let closed = false;

  const world = () => ({ backgroundInFlight: page.backgroundInFlight(), rootOf: x => page.rootOf(x), cadenceMs });

  // Events are taken one at a time, in order: an effect's answer that comes back synchronously (a pass decided at
  // start, a browser without locks) is queued behind the step that caused it, never run inside it.
  function dispatch(event) {
    if (closed) return;
    queue.push(event);
    if (busy) return;
    busy = true;
    try {
      while (queue.length) {
        const r = step(t, queue.shift(), world());
        t = r.tab;
        for (const eff of r.effects) carryOut(eff);
      }
    } finally {
      busy = false;
    }
  }

  function release(target) {
    const r = held.get(target);
    held.delete(target);
    if (r) r();
  }

  // A running asset whose pass is over: its report taken, handed back as `ended`. Called after a pass starts (a
  // signer-less pass is over at once) and whenever a page answered (`poke`).
  function checkEnded() {
    for (const [target, s] of t.assets) {
      if (s.k === "running" && page.auditProgress(target) == null) {
        dispatch({ e: "ended", target, report: page.takeAudit(target), now: clock.now() });
      }
    }
  }

  function carryOut(eff) {
    switch (eff.do) {
      case "askLock":
        if (locks === null) {
          dispatch({ e: "granted", target: eff.target });
          return;
        }
        locks.request(LOCK_PREFIX + eff.target, { ifAvailable: true }, lock => {
          if (!lock) {
            dispatch({ e: "heldElsewhere", target: eff.target, now: clock.now() });
            return undefined;
          }
          return new Promise(resolve => {
            held.set(eff.target, resolve);
            dispatch({ e: "granted", target: eff.target });
            // I2: a lock is held only by a RUNNING asset. Granted to an asset that is gone (Removed while asking) or a
            // driver that closed, it is given straight back.
            if (closed || t.assets.get(eff.target)?.k !== "running") release(eff.target);
          });
        });
        return;
      case "audit":
        page.audit(eff.target);
        queueMicrotask(checkEnded);
        return;
      case "auditSince":
        page.auditSince(eff.target, eff.oldRoot);
        queueMicrotask(checkEnded);
        return;
      case "cancelAudit":
        page.cancelAudit(eff.target);
        return;
      case "release":
        release(eff.target);
        return;
      case "writeBack":
        writeBack(eff.target, eff.report);
        return;
      default:
        console.error(`keep-driver: an effect with no carrier: ${eff.do}`);
    }
  }

  const stop = tickMs === null ? () => {} : clock.every(tickMs, () => dispatch({ e: "due", now: clock.now() }));

  return {
    /** A page answered (a pass may have ended, a background op may have left the wire): look, then re-schedule. */
    poke() {
      checkEnded();
      dispatch({ e: "poke" });
    },
    headMoved: target => dispatch({ e: "head", target }),
    auditNow: target => dispatch({ e: "auditNow", target }),
    cancel: target => dispatch({ e: "cancel", target }),
    add: target => dispatch({ e: "added", target, now: clock.now() }),
    remove: target => dispatch({ e: "removed", target }),
    /** The tab closes: no more events; every lock this tab holds is given back (the browser would too). */
    close() {
      stop();
      closed = true;
      for (const target of [...held.keys()]) release(target);
    },
    state: target => t.assets.get(target)?.k ?? null,
    stats: () => ({ impossible: t.impossible, unhandled: t.unhandled, held: [...held.keys()] }),
  };
}
