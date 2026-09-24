// THE ONE FETCH, and the race of the load pieces (split from artefacts.js, sdk#347): what a published app's
// STARTER carries, because it runs before the SDK exists. The content-hash CACHE of the SDK's artefacts is
// artefacts.js, which fetches through this.
//
// # A node that does not hold it YET has not answered (sdk#312)
//
// A 404, a 503, a 400 or a network error for an artefact the app names is NOT
// an end: a node serves a web container only once it holds it, and one that
// is still fetching or re-storing it answers "not found" in the meantime (the
// owner saw an app fail on a 404 and open on a reload). This fetch is a path
// to the node like any other, so it keeps the owner's rules 7 and 8: re-asked
// until answered, on the PAGE'S back-off (`rto.js`, generated from
// page/src/rto.rs — not a second one written here), with no give-up timer.
// While it waits it says so (`onWait`). What ends it: bytes that do not hash
// to the name (a MISMATCH, refused by name), or a person cancelling
// (`signal`).

import { RTO_SCHEDULE_MS, WINDOW_AFTER_ANSWERS } from "./rto.js";

const hex = bytes =>
  [...new Uint8Array(bytes)].map(b => b.toString(16).padStart(2, "0")).join("");

/** Storage (and digesting) may throw for reasons that are not this app's fault. */
export async function best(effort, fallback = null) {
  try {
    return await effort();
  } catch {
    return fallback;
  }
}

/** THE one wait between rounds: `ms`, or less if a person cancels (`sig`). */
export const sleepFor = (ms, sig) =>
  new Promise(ok => {
    const t = setTimeout(ok, ms);
    sig?.addEventListener?.("abort", () => { clearTimeout(t); ok(); }, { once: true });
  });

/**
 * THE ONE FETCH (#126 ruling): every file a page or a tool fetches from the
 * node over HTTP comes through here — a web container's own files, the SDK's
 * artefacts, a builder's files. Re-asked until answered on the PAGE'S back-off
 * (`rto.js`, generated from page/src/rto.rs and pinned to it by a test), with
 * no give-up of its own: a 404, a 5xx or a network error is "not held yet",
 * never an end (rules 7, 8). While it waits it says so (`onWait`, naming the
 * file). What ends it: a person's cancel (`signal`), or — only where the
 * caller passes a `check` — every source ANSWERING with bytes the check
 * refuses as final.
 *
 * THE BOOTSTRAP EXCEPTION to rule 5 (architect, #126): node DATA goes through
 * the page's one sender, `Page::send`, which does not exist until the SDK's
 * wasm is loaded. The files that load it — and the code and web files a node
 * serves over HTTP — can only be fetched here. Nothing fetched AFTER load that
 * is node data may come this way.
 *
 * `{ url }` or `{ urls }` (never both: taking either silently would drop a
 * source the caller supplied). `check(bytes, from)`: `null` to accept, or
 * `{ says, answer }` — `answer` true when this source has given its final word
 * (then, from EVERY source, the fetch is refused, by `refusal`).
 */
export async function served(
  { url, urls },
  {
    fetch: fetchWith = typeof fetch === "function" ? fetch : null,
    onWait = null,
    signal = null,
    sleep = sleepFor,
    now = () => Date.now(),
    check = null,
    name = null,
    refusal = "a refusal",
    // Options for every request (e.g. `{ cache: "no-store" }` for a file that
    // must be read fresh). Passed as given; never a reason to end.
    init = null,
  } = {},
) {
  const label = name ?? (url ?? (urls ?? []).join(", "));
  if (url && urls) {
    throw new Error(`${label} was given both \`url\` and \`urls\`; pass one. Taking either silently would drop a source the caller supplied.`);
  }
  const sources = urls ?? (url ? [url] : []);
  if (sources.length === 0) throw new Error(`${label} has no url to fetch it from`);
  const started = now();
  for (let round = 0; ; round += 1) {
    // Every failure is kept, so a wait can say what each source actually did.
    const failures = [];
    // The HTTP statuses of the answers that were not the file (a node that does not hold it answers one): told to
    // `onWait`, so a caller can tell "answered, not held" from "no answer yet" (the load pieces' repair, sdk#347).
    const statuses = [];
    let answered = 0;
    for (const from of sources) {
      let res;
      try {
        res = await (init ? fetchWith(from, init) : fetchWith(from));
      } catch (e) {
        failures.push(`${from}: ${e?.message ?? e}`);
        continue;
      }
      if (!res.ok) {
        // NOT AN ANSWER about the file: the node does not hold it yet.
        failures.push(`${from}: ${res.status}`);
        statuses.push(res.status);
        continue;
      }
      // `fetch` resolves on the HEADERS; the body can still fail, and that is
      // this source's failure too — the next source is asked.
      let got;
      try {
        got = new Uint8Array(await res.arrayBuffer());
      } catch (e) {
        failures.push(`${from}: ${e?.message ?? e}`);
        continue;
      }
      const refused = check ? await check(got, from) : null;
      if (!refused) return got;
      if (refused.answer) answered += 1;
      failures.push(`${from}: ${refused.says}`);
    }
    // NAMES WHICH FILE AND WHAT EACH SOURCE DID: the first thing a person can
    // send when an app will not open.
    const what = failures.join("\n  ");
    if (answered === sources.length) {
      throw new Error(`${label} refused: ${refusal} from every source:\n  ${what}`);
    }
    const waitedMs = now() - started;
    if (signal?.aborted) {
      throw new Error(`${label}: cancelled after ${Math.round(waitedMs / 1000)} s unanswered:\n  ${what}`);
    }
    const nextMs = RTO_SCHEDULE_MS[Math.min(round, RTO_SCHEDULE_MS.length - 1)];
    onWait?.({
      waitedMs,
      nextMs,
      failures,
      statuses,
      says: `loading the app… not available on this node yet (${Math.round(waitedMs / 1000)} s)`,
    });
    await sleep(nextMs, signal);
    if (signal?.aborted) {
      throw new Error(`${label}: cancelled after ${Math.round((now() - started) / 1000)} s unanswered:\n  ${what}`);
    }
  }
}

/**
 * RACE THE LOAD PIECES (sdk#347, the owner's rule 11 applied to loading the SDK): `spec.pieces` is the `k + m`
 * pieces of the SDK's bundle (`{ url | urls, sha256 }`, data first), and the bundle needs ANY `k` of them. Each
 * piece is asked through `served()` (the one fetch: re-asked on the RTO, never ended by a status) and checked by its
 * sha256; a piece that verifies counts, one with the wrong bytes does not. At most `WINDOW_AFTER_ANSWERS[answers]`
 * are in flight (the page's own window, generated from page/src/rto.rs: no second pacing rule). At the FIRST `k`
 * verified it resolves and ABORTS every piece still asked: a stalled piece never holds the page.
 *
 * No time cut-off (rule 8): while fewer than `k` have verified, it waits and `onWait` says so. It rejects only
 * when every piece has REFUSED (wrong bytes from every source), or on the person's cancel (`signal`).
 *
 * Resolves `{ pieces, verified, asked, notHeld }`: `notHeld` the pieces a source answered NOT FOUND (404) and that
 * never arrived -- the only ones a repair may PUT back (a piece cancelled at k was not missing). `pieces[i]` the
 * verified bytes of piece `i` or `null`; `asked` the
 * indices that were asked (the rest were never needed). What the loader's repair later re-PUTs is what was asked
 * and did not arrive.
 */
export async function raceK(
  spec,
  {
    fetch: fetchWith = typeof fetch === "function" ? fetch : null,
    subtle = typeof crypto === "object" ? crypto.subtle : null,
    onWait = null,
    signal = null,
    sleep = sleepFor,
    now = () => Date.now(),
  } = {},
) {
  const { k, m, pieces } = spec;
  if (!Array.isArray(pieces) || pieces.length !== k + m || k < 1) {
    throw new Error(`raceK: ${pieces?.length} pieces, not k + m = ${k + m}`);
  }
  if (!subtle) throw new Error("no crypto.subtle: cannot verify a piece");
  const stop = new AbortController();
  const cancel = () => stop.abort();
  signal?.addEventListener?.("abort", cancel, { once: true });
  const got = new Array(k + m).fill(null);
  const asked = [];
  // Pieces the node ANSWERED "not held" (404) at least once: the only ones the repair may PUT back. A piece
  // still in flight when the k-th verified was cancelled by this race, not missing, and is never repaired.
  const notHeld = new Set();
  let verified = 0;
  let refused = 0;
  let answers = 0;
  let inFlight = 0;
  let next = 0;
  const window = () => WINDOW_AFTER_ANSWERS[Math.min(answers, WINDOW_AFTER_ANSWERS.length - 1)];
  try {
    return await new Promise((resolve, reject) => {
      const settle = () => {
        if (verified >= k) {
          stop.abort();
          resolve({ pieces: got, verified, asked, notHeld: [...notHeld].filter(i => !got[i]).sort((a, b) => a - b) });
        } else if (stop.signal.aborted) {
          reject(new Error(`the SDK's pieces: cancelled with ${verified} of ${k} verified`));
        } else if (refused > m) {
          stop.abort();
          reject(new Error(`the SDK's pieces refused: ${refused} of ${k + m} gave bytes that do not hash to their name, so fewer than k = ${k} can verify`));
        } else {
          launch();
        }
      };
      const launch = () => {
        while (next < k + m && inFlight < window()) {
          const i = next;
          next += 1;
          inFlight += 1;
          asked.push(i);
          const p = pieces[i];
          served(p.urls ? { urls: p.urls } : { url: p.url }, {
            // The abort reaches the request itself, not only the next round: a piece no longer needed stops now.
            fetch: (u, init) => fetchWith(u, { ...(init ?? {}), signal: stop.signal }),
            signal: stop.signal,
            sleep,
            now,
            name: `SDK piece ${i}`,
            refusal: "bytes that do not hash to the piece's sha256",
            check: async bytes => ((await matches(bytes, p.sha256, subtle)) ? null : { answer: true, says: "the wrong bytes" }),
            onWait: w => {
              // A source's own NOT FOUND (404) only: a 503 is "not ready" (a node still joining), and silence or a
              // network error is no answer at all (rule 8: re-asked, never taken for "missing").
              if (w.statuses?.includes(404)) notHeld.add(i);
              onWait?.({ ...w, piece: i, verified, k });
            },
          }).then(
            bytes => {
              got[i] = bytes;
              verified += 1;
              answers += 1;
            },
            () => {
              // Cancelled (the race is over) or refused (the wrong bytes from every source): not counted.
              if (!stop.signal.aborted) refused += 1;
            },
          ).finally(() => {
            inFlight -= 1;
            settle();
          });
        }
      };
      launch();
    });
  } finally {
    signal?.removeEventListener?.("abort", cancel);
  }
}

/** A file's text, through the one fetch. */
export async function servedText(source, deps) {
  return new TextDecoder().decode(await served(source, deps));
}

export async function digestOf(bytes, subtle) {
  if (!subtle) throw new Error("no crypto.subtle: cannot verify an artefact");
  return best(async () => hex(await subtle.digest("SHA-256", bytes)));
}

export async function matches(bytes, sha256, subtle) {
  return (await digestOf(bytes, subtle)) === sha256;
}
