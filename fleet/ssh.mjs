// One open connection per machine. Every ssh the panel runs shares it (ControlMaster): the first opens it, it stays up
// (ControlPersist, kept alive both ways), and each later call is a new channel on it, not a new TCP, key exchange and
// login. To a far machine that is a few seconds saved every time.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const DIR = path.join(os.homedir(), '.fleet-panel');

/** The options every panel ssh takes, before the host. */
export function sshArgs() {
  try { fs.mkdirSync(DIR, { recursive: true, mode: 0o700 }); } catch {}
  return ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3',
          '-o', 'ControlMaster=auto', '-o', `ControlPath=${path.join(DIR, 'ssh-%C')}`, '-o', 'ControlPersist=yes'];
}

export const shq = (v) => `'${String(v).replace(/'/g, `'\\''`)}'`;
