// One-time pairing codes for the "Pair a phone" button. The page asks the panel for an address to show as a QR; it must never be
// handed the panel's standing key (a one-year login, kept in a file beside prefs.json), because that would put the key in page
// JS, in a screenshot, in the clipboard. It gets a code instead: random, good for one use and two minutes, and at most a few
// alive at once. Scanning it trades the code for the usual cookie (server.mjs, the `?p=` branch).
import crypto from 'node:crypto';

export function createPairCodes({ ttlMs = 120_000, max = 5, now = Date.now } = {}) {
  const codes = new Map(); // code → expiry (ms); insertion order is age
  const prune = () => { for (const [c, exp] of codes) if (exp <= now()) codes.delete(c); };
  return {
    /** A fresh code. Expired ones go first; if five are still alive the oldest makes room. */
    mint() {
      prune();
      while (codes.size >= max) codes.delete(codes.keys().next().value);
      const code = crypto.randomBytes(24).toString('hex');
      codes.set(code, now() + ttlMs);
      return code;
    },
    /** Whether `code` was live; it is spent either way. */
    consume(code) {
      const exp = typeof code === 'string' ? codes.get(code) : undefined;
      codes.delete(code);
      return exp !== undefined && exp > now();
    },
    size: () => codes.size,
    ttlMs,
  };
}
