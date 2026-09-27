// AN SDK VERSION IS READ FROM ITS PLATFORM TREE (§19, the SDK as data; the architect on #572): `handle.platformSdk(
// tree, rev)` opens the platform tree -- ANOTHER identity's, named by its Register id -- read-only through the ONE
// open-another's-tree path (`tree()`), asks it, and closes it. The handle's OWN tree is never consulted: a reader of
// an SDK is almost never the platform. What `tree()` does (a read-only reader Session standing on the named head,
// nothing installed) is tests/js/tree.test.mjs and view.test.mjs; what the record says is tests/platform_sdk.rs.
import assert from "node:assert/strict";
import { platformSdkOf } from "../../js/session.js";

let failures = 0;
const t = async (name, fn) => {
  try { await fn(); process.stdout.write(`  ok  ${name}\n`); }
  catch (e) { failures += 1; process.stdout.write(`  FAIL ${name}\n    ${e.stack}\n`); }
};

const P = "b0".repeat(32);            // the platform's Register: identity P
const REV = "0123456789abcdef0123456789abcdef01234567";

/** Identity O's handle: its own tree's db, and `tree()` opening P's (recorded). */
const handleOf = ({ refuse = false } = {}) => {
  const seen = { own: 0, opened: [], closed: 0, asked: [] };
  const handle = {
    db: new Proxy({}, { get: () => () => { seen.own += 1; throw new Error("the handle's own tree was consulted"); } }),
    tree: async (registerId, opts) => {
      seen.opened.push([registerId, opts]);
      return {
        db: { sdkVersion: async rev => { seen.asked.push([registerId, rev]); if (refuse) throw new Error(`the platform tree publishes no SDK ${rev}`); return { rev, name: "0.4", set: { k: 1, m: 0, pieces: [] } }; } },
        close: () => { seen.closed += 1; },
      };
    },
  };
  return { handle, seen };
};

await t("**a record written under identity P is read by O's session from P's tree, read-only, and closed**; O's own tree is never consulted", async () => {
  const { handle, seen } = handleOf();
  const v = await platformSdkOf(handle, P, REV);
  assert.equal(v.rev, REV);
  assert.deepEqual(seen.opened, [[P, { app: null }]], "the platform tree was not opened by its Register id, as a reader with no app of its own");
  assert.deepEqual(seen.asked, [[P, REV]], "the version was not asked of the platform tree's reader");
  assert.equal(seen.own, 0, "O's own tree was consulted");
  assert.equal(seen.closed, 1, "the platform tree's reader was left open");
});

await t("**a version the platform does not publish is refused by name, and the reader is still closed**", async () => {
  const { handle, seen } = handleOf({ refuse: true });
  await assert.rejects(platformSdkOf(handle, P, REV), /publishes no SDK/);
  assert.equal(seen.closed, 1, "a refused read left the reader open");
  assert.equal(seen.own, 0);
});

if (failures) { process.stdout.write(`\n${failures} failing\n`); process.exit(1); }
process.stdout.write("\nall passing\n");
