// THE CONTRACTS THE SDK SHIPS ARE THE RELEASED ONES: build.sh's ONE check, run before it copies
// `$CRAFTWORKS_CONTRACTS/build/*.wasm` into pkg/web.
//
// A contract's wasm hash is part of its network key (F37), so an SDK built on a STALE contracts checkout ships
// code that addresses different contracts -- and an old Block refuses the current tree's writes. That happened
// (2026-09-24): an epoch-2 block.wasm (d7918979…) in an epoch-3 SDK made every 300-row write "refused by the
// node", and one-leaf trees passed, so it read as a platform bug. It cost three engineers time in one day.
//
// So: every contract this SDK copies must hash to its row in the LATEST `[[epoch]]` of that checkout's
// `released.toml` (the contracts repo's table of released code). Anything else REFUSES, by name: the checkout's
// HEAD, the contract, the expected and actual sha256, and the fix. A contract with no row in the latest epoch
// refuses too (unreleased code), and so does a checkout with no `released.toml` (could not check = a failure,
// never a skip).
//
// THE ONE OVERRIDE, for a PR that builds a NEW epoch (as freenet-contracts#54 did):
// `CRAFTWORKS_CONTRACTS_UNRELEASED=1` passes, and says LOUDLY which contracts are not the released ones.
//
//   node tools/contracts-released.mjs <contracts-dir> <name> [<name> ...]
import { createHash } from "node:crypto";
import { readFileSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join } from "node:path";

/** The latest epoch's `{ number, tag, sha: { <contract>: <hex> } }` from released.toml's text, or null. */
export function latestEpoch(toml) {
  const chunks = toml.split(/^\[\[epoch\]\]\s*$/m).slice(1);
  if (chunks.length === 0) return null;
  const last = chunks[chunks.length - 1];
  const number = Number(/^number\s*=\s*(\d+)/m.exec(last)?.[1]);
  const tag = /^tag\s*=\s*"([^"]*)"/m.exec(last)?.[1] ?? null;
  const sha = {};
  // `[epoch.contract.<name>]` and its own lines, up to the next table header.
  for (const m of last.matchAll(/^\[epoch\.contract\.([A-Za-z0-9_-]+)\]\s*\n((?:(?!\[)[^\n]*\n?)*)/gm)) {
    const h = /^sha256\s*=\s*"sha256:([0-9a-f]{64})"/m.exec(m[2]);
    if (h) sha[m[1]] = h[1];
  }
  return Number.isInteger(number) ? { number, tag, sha } : null;
}

/** The check: `{ ok, lines }`; `lines` is what to print (the refusal, or the override's notice). */
export function check(contracts, names, { override = false, head = null } = {}) {
  const at = head ? ` (the checkout at ${head})` : "";
  const table = join(contracts, "released.toml");
  if (!existsSync(table)) {
    return { ok: false, lines: [`contracts: ${table} does not exist${at}: which code is released cannot be checked, so nothing is copied (set CRAFTWORKS_CONTRACTS to a freenet-contracts checkout)`] };
  }
  const epoch = latestEpoch(readFileSync(table, "utf8"));
  if (!epoch) return { ok: false, lines: [`contracts: ${table} names no [[epoch]]${at}: which code is released cannot be checked`] };
  const wrong = [];
  for (const n of names) {
    const file = join(contracts, "build", `${n}.wasm`);
    const actual = existsSync(file) ? createHash("sha256").update(readFileSync(file)).digest("hex") : null;
    const expected = epoch.sha[n] ?? null;
    if (actual !== expected || actual === null) wrong.push({ n, expected, actual });
  }
  if (wrong.length === 0) return { ok: true, lines: [`contracts: ${names.join(", ")} are epoch ${epoch.number}'s released code (${epoch.tag})`] };
  const said = wrong.map(w => `  ${w.n}.wasm: expected ${w.expected ?? "(no row in epoch " + epoch.number + ")"}, actual ${w.actual ?? "(no build/" + w.n + ".wasm)"}`);
  if (override) {
    return { ok: true, lines: [`contracts: OVERRIDE (CRAFTWORKS_CONTRACTS_UNRELEASED=1): shipping contracts that are NOT epoch ${epoch.number}'s released code${at} -- only for a PR that builds a new epoch:`, ...said] };
  }
  return {
    ok: false,
    lines: [
      `contracts: REFUSED -- the contracts build${at} is not epoch ${epoch.number}'s released code (${epoch.tag}), the latest row of ${table}:`,
      ...said,
      `  fix: rebuild the contracts at the released tag -- in a worktree of your own: git -C ${contracts} worktree add --detach <dir> ${epoch.tag ?? "<the tag>"} && (cd <dir> && ./build.sh), then CRAFTWORKS_CONTRACTS=<dir>`,
      `  (a PR that builds a NEW epoch sets CRAFTWORKS_CONTRACTS_UNRELEASED=1, and says so)`,
    ],
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [contracts, ...names] = process.argv.slice(2);
  if (!contracts || names.length === 0) {
    console.error("usage: node tools/contracts-released.mjs <contracts-dir> <name> [<name> ...]");
    process.exit(2);
  }
  let head = null;
  try { head = execFileSync("git", ["-C", contracts, "describe", "--always", "--tags", "--dirty"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch (_) { /* not a checkout: named as such below */ }
  const r = check(contracts, names, { override: process.env.CRAFTWORKS_CONTRACTS_UNRELEASED === "1", head: head ?? "(not a git checkout)" });
  for (const l of r.lines) (r.ok ? console.log : console.error)(l);
  process.exit(r.ok ? 0 : 1);
}
