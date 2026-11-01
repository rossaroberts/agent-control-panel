// The one name the panel answers to besides the loopback: the Mac's Tailscale name, which `tailscale serve` proxies in over
// HTTPS. It comes from ACP_PUBLIC_HOST, else the `public` field of fleet.json beside prefs.json; anything that is not a plain
// dotted hostname is ignored, so a typo can never widen the Host allow-list to something odd.
import fs from 'node:fs';
import path from 'node:path';

const NAME = /^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/i;

/** `v` as a lower-cased hostname, or null when it is not one. */
export const validPublic = (v) => (typeof v === 'string' && v.length <= 253 && NAME.test(v) ? v.toLowerCase() : null);

/** The configured public host for the panel whose prefs file is `prefsPath`, or null. */
export function publicHost(prefsPath, env = process.env) {
  const fromEnv = validPublic(env.ACP_PUBLIC_HOST);
  if (fromEnv) return fromEnv;
  try { return validPublic(JSON.parse(fs.readFileSync(path.join(path.dirname(prefsPath), 'fleet.json'), 'utf8'))?.public); } catch { return null; }
}
