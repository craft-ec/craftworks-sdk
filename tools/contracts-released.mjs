// THE CONTRACTS THE SDK SHIPS ARE THE RELEASED ONES: build.sh's ONE check, run before it copies
// `$CRAFTWORKS_CONTRACTS/build/*.wasm` into pkg/web.
//
// A contract's wasm hash is part of its network key (F37), so an SDK built on a STALE contracts checkout ships
// code that addresses different contracts -- and an old Block refuses the current tree's writes. That happened
// (2026-09-24): an epoch-2 block.wasm (d7918979…) in an epoch-3 SDK made every 300-row write "refused by the
// node", and one-leaf trees passed, so it read as a platform bug. It cost three engineers time in one day.
//
// So: the SDK states the ONE epoch it needs (build.sh's `CONTRACTS_EPOCH`: only the SDK knows that its format
// needs it), and every contract it copies must hash to its row in THAT `[[epoch]]` of the checkout's
// `released.toml` (the contracts repo's own table of which code each epoch IS). Not the table's LATEST epoch: a
// checkout BEHIND the release has a table behind it too -- the incident's checkout ended at epoch 2, its block
// WAS epoch 2's row, and "latest" would have passed it (the architect, sdk#388). A table without the needed
// epoch REFUSES, naming the epochs it has; a table with LATER epochs as well is fine (the SDK still ships its
// own epoch's code). Anything else REFUSES by name too: the checkout's HEAD, the contract, the expected and actual
// sha256, and the fix. A contract with no row in the needed epoch refuses (unreleased code), and so does a
// checkout with no `released.toml` (could not check = a failure, never a skip).
//
// THE ONE OVERRIDE, for a PR that builds a NEW epoch (as freenet-contracts#54 did):
// `CRAFTWORKS_CONTRACTS_UNRELEASED=1` passes, and says LOUDLY which contracts are not the released ones.
//
//   node tools/contracts-released.mjs <contracts-dir> <epoch> <name> [<name> ...]
import { createHash } from "node:crypto";
import { readFileSync, existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join } from "node:path";

/** Every epoch's `{ number, tag, sha: { <contract>: <hex> } }` from released.toml's text, in file order. */
export function epochs(toml) {
  return toml
    .split(/^\[\[epoch\]\]\s*$/m)
    .slice(1)
    .map(chunk => {
      const number = Number(/^number\s*=\s*(\d+)/m.exec(chunk)?.[1]);
      const tag = /^tag\s*=\s*"([^"]*)"/m.exec(chunk)?.[1] ?? null;
      const sha = {};
      // `[epoch.contract.<name>]` and its own lines, up to the next table header.
      for (const m of chunk.matchAll(/^\[epoch\.contract\.([A-Za-z0-9_-]+)\]\s*\n((?:(?!\[)[^\n]*\n?)*)/gm)) {
        const h = /^sha256\s*=\s*"sha256:([0-9a-f]{64})"/m.exec(m[2]);
        if (h) sha[m[1]] = h[1];
      }
      return { number, tag, sha };
    })
    .filter(e => Number.isInteger(e.number));
}

/** The check: `{ ok, lines }`; `lines` is what to print (the refusal, or the override's notice). */
export function check(contracts, need, names, { override = false, head = null } = {}) {
  const at = head ? ` (the checkout at ${head})` : "";
  const table = join(contracts, "released.toml");
  if (!existsSync(table)) {
    return { ok: false, lines: [`contracts: ${table} does not exist${at}: which code is released cannot be checked, so nothing is copied (set CRAFTWORKS_CONTRACTS to a freenet-contracts checkout)`] };
  }
  const all = epochs(readFileSync(table, "utf8"));
  const epoch = all.find(e => e.number === need);
  if (!epoch) {
    const has = all.length ? `epochs ${all.map(e => e.number).join(", ")}` : "no [[epoch]] at all";
    return {
      ok: false,
      lines: [
        `contracts: REFUSED -- ${table}${at} has ${has}; this SDK needs epoch ${need}: the checkout is BEHIND the release this SDK is built for`,
        `  fix: build the contracts at a release that has epoch ${need} -- in a worktree of your own: git -C ${contracts} fetch && git -C ${contracts} worktree add --detach <dir> origin/main && (cd <dir> && ./build.sh), then CRAFTWORKS_CONTRACTS=<dir>`,
      ],
    };
  }
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
  const [contracts, needArg, ...names] = process.argv.slice(2);
  const need = Number(needArg);
  if (!contracts || !Number.isInteger(need) || need < 1 || names.length === 0) {
    console.error("usage: node tools/contracts-released.mjs <contracts-dir> <epoch> <name> [<name> ...]");
    process.exit(2);
  }
  let head = null;
  try { head = execFileSync("git", ["-C", contracts, "describe", "--always", "--tags", "--dirty"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch (_) { /* not a checkout: named as such below */ }
  const r = check(contracts, need, names, { override: process.env.CRAFTWORKS_CONTRACTS_UNRELEASED === "1", head: head ?? "(not a git checkout)" });
  for (const l of r.lines) (r.ok ? console.log : console.error)(l);
  process.exit(r.ok ? 0 : 1);
}
