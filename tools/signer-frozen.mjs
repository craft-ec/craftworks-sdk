// THE FROZEN SIGNER (signer/frozen/signer.sha256): the signer.wasm the SDK ships is the committed one, and only if
// it hashes to the pin. build.sh's one check, before it copies the file into pkg/web. A different signer.wasm is a
// different delegate -- every user's identity -- so a mismatch REFUSES, naming both hashes and the rule.
//
//   node tools/signer-frozen.mjs <signer.wasm> <signer.sha256>
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

/** The pin: the one 64-hex line of the pin file (comments start with #). */
export function pinOf(text) {
  const lines = text.split("\n").map(l => l.trim()).filter(l => l && !l.startsWith("#"));
  if (lines.length !== 1 || !/^[0-9a-f]{64}$/.test(lines[0])) throw new Error("the signer pin file holds no single sha256 line");
  return lines[0];
}

/** `null` when `bytes` are the pinned signer; else the refusal, in words. */
export function refusal(bytes, pin) {
  const got = createHash("sha256").update(bytes).digest("hex");
  return got === pin
    ? null
    : `signer.wasm is ${got}, not the pinned ${pin}: the signer is FROZEN (a different signer is a different delegate, and every user a new identity). Ship signer/frozen/signer.wasm; a pin bump needs the owner's say (rule 15) until sdk#14's migration exists.`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [file, pinFile] = process.argv.slice(2);
  const why = refusal(readFileSync(file), pinOf(readFileSync(pinFile, "utf8")));
  if (why) {
    console.error(why);
    process.exit(1);
  }
}
