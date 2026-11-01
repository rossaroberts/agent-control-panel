// The service worker as served: public/sw.js with its cache name made from a hash of offline.html. A changed offline page then
// gives a phone a new cache name, and the worker's activate step deletes the old one, so a phone never keeps a stale page.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';

export function swSource(dir) {
  const hash = crypto.createHash('sha256').update(fs.readFileSync(path.join(dir, 'offline.html'))).digest('hex').slice(0, 12);
  return fs.readFileSync(path.join(dir, 'sw.js'), 'utf8').replaceAll('__OFFLINE_HASH__', hash);
}
