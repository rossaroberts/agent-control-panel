// Fleet's hook (hook.sh) into Claude Code profiles' settings.json, for the events that say what a session is doing.
//
//   node fleet/install-hooks.mjs <hook.sh path> <profile dir>... [--cap-http] [--remove] [--dry-run]
//
// Every running session picks a settings change up at once (Claude Code watches the file), so this is careful: what is
// there already stays as it is, a copy is kept beside it first (settings.json.before-fleet-hooks-<time>), the new file is
// written whole and swapped in, and it is read back and checked before this says it is done. Run again, it changes
// nothing. --cap-http gives the panel's own HTTP hooks (127.0.0.1:…/hook/event) a 5 s timeout where they had none: the
// default is ten minutes, so a panel that hung would hold every tool call that long. --remove takes Fleet's hook out.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const EVENTS = ['SessionStart', 'UserPromptSubmit', 'PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'PermissionRequest', 'PermissionDenied',
  'Notification', 'Elicitation', 'ElicitationResult', 'Stop', 'StopFailure', 'PreCompact', 'PostCompact', 'SessionEnd'];
// Ours: `sh '<path>'` where the path ends in fleet-panel/hook.sh or fleet/hook.sh (someone else's hook.sh under another name is theirs).
const OURS = (h) => typeof h?.command === 'string' && /^sh '[^']*\/\.?(fleet-panel|fleet)\/hook\.sh'$/.test(h.command);

/** The settings with Fleet's hook in (or out): a new object, and what changed. Pure, for the tests. */
export function withHook(settings, hookPath, { capHttp = false, remove = false } = {}) {
  const s = structuredClone(settings ?? {});
  if (s.hooks != null && (typeof s.hooks !== 'object' || Array.isArray(s.hooks))) throw new Error('its "hooks" setting is not an object');
  s.hooks ??= {};
  if (!remove && /'/.test(hookPath)) throw new Error('the hook path holds a quote, which sh would read as the end of the command');
  const changes = [];
  for (const event of EVENTS) {
    if (s.hooks[event] != null && !Array.isArray(s.hooks[event])) throw new Error(`its hooks.${event} is not a list`);
    const list = Array.isArray(s.hooks[event]) ? s.hooks[event].filter((m) => m && typeof m === 'object') : null;
    const has = (list ?? []).some((m) => (m?.hooks ?? []).some(OURS));
    if (remove) {
      if (!has) continue;
      s.hooks[event] = list.map((m) => ({ ...m, hooks: (m.hooks ?? []).filter((h) => !OURS(h)) })).filter((m) => m.hooks.length);
      if (!s.hooks[event].length) delete s.hooks[event];
      changes.push(`- ${event}`);
    } else if (!has) {
      (s.hooks[event] = list ?? []).push({ hooks: [{ type: 'command', command: `sh '${hookPath}'`, timeout: 5 }] });
      changes.push(`+ ${event}`);
    }
  }
  if (capHttp) for (const [event, list] of Object.entries(s.hooks)) for (const m of Array.isArray(list) ? list : []) for (const h of m?.hooks ?? []) {
    if (h?.type === 'http' && /\/hook\/event$/.test(h.url ?? '') && h.timeout == null) { h.timeout = 5; changes.push(`timeout 5s: ${event} → ${h.url}`); }
  }
  if (!Object.keys(s.hooks).length) delete s.hooks;
  return { settings: s, changes };
}

function install(dir, hookPath, opts) {
  const file = path.join(dir, 'settings.json');
  let before = {};
  if (fs.existsSync(file)) before = JSON.parse(fs.readFileSync(file, 'utf8')); // invalid JSON: stop here, untouched
  const { settings, changes } = withHook(before, hookPath, opts);
  if (!changes.length) return `${file}: as it should be`;
  if (opts.dryRun) return `${file}: would change\n  ${changes.join('\n  ')}`;
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  if (fs.existsSync(file)) fs.copyFileSync(file, `${file}.before-fleet-hooks-${stamp}`);
  // A settings.json that is a link (dotfiles kept elsewhere) stays one: the file it points to is the one swapped.
  const target = fs.existsSync(file) ? fs.realpathSync(file) : file;
  const tmp = `${target}.fleet-tmp-${process.pid}`;
  fs.writeFileSync(tmp, JSON.stringify(settings, null, 2) + '\n', { mode: fs.existsSync(target) ? fs.statSync(target).mode & 0o777 : 0o644 });
  JSON.parse(fs.readFileSync(tmp, 'utf8'));
  fs.renameSync(tmp, target);
  JSON.parse(fs.readFileSync(file, 'utf8'));
  return `${file}: changed (copy kept: settings.json.before-fleet-hooks-${stamp})\n  ${changes.join('\n  ')}`;
}

if (process.argv[1] && fileURLToPath(import.meta.url) === path.resolve(process.argv[1])) {
  const args = process.argv.slice(2), flags = new Set(args.filter((a) => a.startsWith('--')));
  const [hookPath, ...dirs] = args.filter((a) => !a.startsWith('--'));
  if (!hookPath || !dirs.length || !path.isAbsolute(hookPath)) { console.error('usage: install-hooks.mjs <absolute hook.sh path> <profile dir>... [--cap-http] [--remove] [--dry-run]'); process.exit(2); }
  // The hook must be there to run: a path with a typo would make every event of every session fail (exit 127).
  if (!flags.has('--remove') && !fs.existsSync(hookPath)) { console.error(`${hookPath}: no such file`); process.exit(2); }
  let failed = false;
  for (const d of dirs) {
    try { console.log(install(d, hookPath, { capHttp: flags.has('--cap-http'), remove: flags.has('--remove'), dryRun: flags.has('--dry-run') })); }
    catch (e) { failed = true; console.error(`${path.join(d, 'settings.json')}: left as it is (${e.message})`); }
  }
  process.exit(failed ? 1 : 0);
}
