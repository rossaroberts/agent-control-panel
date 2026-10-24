#!/usr/bin/env node
// Fleet's status line: Claude Code hands its status line command a JSON snapshot after each turn, carrying the
// account's plan limits (rate_limits.five_hour / seven_day) and the session's context window. Nothing else in
// Claude Code says how much room an account has left, so this keeps the newest snapshot per session in
// <config dir>/fleet-status/<session id>.json for the Fleet page (collect.mjs) to read, and prints a short line.
//
// Wire it as a profile's statusLine: { "type": "command", "command": "node /path/to/fleet/statusline.mjs" }.
// When FLEET_STATUSLINE_NEXT names another command, its output is printed instead, so an existing status line stays.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

let raw = '';
try { raw = fs.readFileSync(0, 'utf8'); } catch {}
let d = null;
try { d = JSON.parse(raw); } catch {}

const dir = process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), '.claude');
const id = typeof d?.session_id === 'string' && /^[\w-]{8,80}$/.test(d.session_id) ? d.session_id : null;
if (id) {
  try {
    const out = path.join(dir, 'fleet-status');
    fs.mkdirSync(out, { recursive: true });
    const keep = { at: Date.now(), session_id: id, cwd: d.cwd ?? d.workspace?.current_dir ?? null, model: d.model ?? null,
                   context_window: d.context_window ?? null, rate_limits: d.rate_limits ?? null,
                   rate_limits_available: d.rate_limits_available ?? null };
    const tmp = path.join(out, `.${id}.tmp`);
    fs.writeFileSync(tmp, JSON.stringify(keep));
    fs.renameSync(tmp, path.join(out, `${id}.json`));
  } catch {}
}

/** Its context against the point it compacts at, by this profile's settings (the same rule as fleet/machine.mjs's
 *  compactAt), else against its window. */
function contextPct(d) {
  const cw = d.context_window, u = cw.current_usage, window = cw.context_window_size;
  if (!u || !window) return cw.used_percentage;
  const tokens = (u.input_tokens ?? 0) + (u.cache_creation_input_tokens ?? 0) + (u.cache_read_input_tokens ?? 0);
  let settings = {};
  try { settings = JSON.parse(fs.readFileSync(path.join(dir, 'settings.json'), 'utf8')); } catch {}
  const id = String(d.model?.id ?? '').replace(/\[1m\]$/, '');
  const per = settings?.modelSettings && typeof settings.modelSettings === 'object' ? settings.modelSettings : {};
  const key = Object.keys(per).filter((k) => id === k || id.startsWith(`${k}-`)).sort((a, b) => b.length - a.length)[0];
  const n = (key && per[key]?.autoCompactWindow) ?? settings?.autoCompactWindow;
  const at = Number.isFinite(n) && n > 0 && n < window && tokens <= n * 1.02 ? n : window;
  return (100 * tokens) / at;
}

const next = process.env.FLEET_STATUSLINE_NEXT;
if (next) {
  const r = spawnSync('/bin/sh', ['-c', next], { input: raw, encoding: 'utf8', timeout: 4000 });
  process.stdout.write(r.stdout ?? '');
} else {
  const pct = (v) => (typeof v === 'number' ? `${Math.round(v)}%` : '–');
  const parts = [];
  if (d?.context_window) parts.push(`ctx ${pct(contextPct(d))}`);
  if (d?.rate_limits?.five_hour) parts.push(`5h ${pct(d.rate_limits.five_hour.used_percentage)}`);
  if (d?.rate_limits?.seven_day) parts.push(`7d ${pct(d.rate_limits.seven_day.used_percentage)}`);
  process.stdout.write(parts.join(' · '));
}
