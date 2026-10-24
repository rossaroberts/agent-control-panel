// Live terminals: a background session's own Claude Code screen in the page, typed into as you.
//
// A pseudo-terminal on this Mac runs `claude attach <id>`: here, or through `ssh -tt` on the machine the session runs on,
// as a channel on that machine's open connection (ssh.mjs).
// Its bytes stream to the page as they come (one long POST response) and the page's keys come back as small POSTs. The pty
// is a few lines of Python, so there is no native module to build: it relays bytes both ways and sets the window size it is
// told on fd 3. When the page lets go of the stream, the attach ends; the session keeps running in the background.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';
import { sshArgs, shq } from './ssh.mjs';

const PTY = String.raw`
import os, sys, pty, select, signal, struct, fcntl, termios
cols, rows = int(sys.argv[1]), int(sys.argv[2])
def size(fd, c, r): fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', r, c, 0, 0))
pid, fd = pty.fork()
if pid == 0:
    size(0, cols, rows)
    os.execvp(sys.argv[3], sys.argv[3:])
def stop(*_):
    try: os.kill(pid, signal.SIGHUP)
    except OSError: pass
    os._exit(0)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGHUP, stop)
size(fd, cols, rows)
def put(to, data):
    while data: data = data[os.write(to, data):]
watch, pending = [0, 3, fd], b''
while True:
    try: ready = select.select(watch, [], [])[0]
    except InterruptedError: continue
    if fd in ready:
        try: data = os.read(fd, 65536)
        except OSError: data = b''
        if not data: break
        put(1, data)
    if 0 in ready:
        data = os.read(0, 65536)
        if not data: break
        put(fd, data)
    if 3 in ready:
        data = os.read(3, 4096)
        if not data: watch.remove(3); continue
        pending += data
        while b'\n' in pending:
            line, pending = pending.split(b'\n', 1)
            try:
                c, r = (int(v) for v in line.split())
                size(fd, c, r)
            except ValueError: pass
stop()
`;

const HOME = os.homedir();
const MAX = 12;
const PROFILE = /^[\w.-]{1,40}$/, JOB = /^[0-9a-f]{8}$/;
const DIR = /^(~|~\/[^\0\n\r]{0,400}|\/[^\0\n\r]{0,400})$/; // a folder for a trust prompt: absolute or under ~, one line
const TMUX_SOCKET = /^[\w.-]{1,40}$/, TMUX_TARGET = /^[\w.-]{1,60}:\d+\.\d+$/; // a session in tmux: its server and pane
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const codexBin = () => [path.join(HOME, '.local', 'bin', 'codex'), '/opt/homebrew/bin/codex', '/usr/local/bin/codex'].find((f) => fs.existsSync(f)) ?? 'codex';
const tmuxBin = () => ['/opt/homebrew/bin/tmux', '/usr/local/bin/tmux', '/usr/bin/tmux'].find((f) => fs.existsSync(f)) ?? 'tmux';
const python = () => (fs.existsSync('/usr/bin/python3') ? '/usr/bin/python3' : 'python3');
const claudeBin = () => [path.join(HOME, '.local', 'bin', 'claude'), '/opt/homebrew/bin/claude', '/usr/local/bin/claude'].find((f) => fs.existsSync(f)) ?? 'claude';
const clamp = (n, lo, hi, d) => (Number.isInteger(n) ? Math.min(hi, Math.max(lo, n)) : d);

/** `config()` is fleet.json as createFleet reads it: { self, machines: [{ name, ssh, user }] }. */
export function createTerms({ config }) {
  const terms = new Map(); // id -> { child }

  /** The program the pty runs: `claude attach <job>` here, or the same over SSH as the machine's user; with `dir` instead of
   *  a job, Claude Code itself in that folder, so its trust prompt waits for you there. Null for a bad request. */
  function command(machine, profile, job, dir = null, tmux = null, codex = null) {
    if (tmux && !(TMUX_SOCKET.test(tmux.socket ?? '') && TMUX_TARGET.test(tmux.target ?? ''))) return null;
    if (codex && !(UUID.test(codex.sessionId ?? '') && DIR.test(codex.cwd ?? ''))) return null;
    if (codex) dir = codex.cwd;
    if (!PROFILE.test(profile) || (tmux || codex ? false : dir == null ? !JOB.test(job) : !DIR.test(dir))) return null;
    const c = config();
    const env = { ...Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE') && !k.startsWith('FLEET_'))),
                  TERM: 'xterm-256color', COLORTERM: 'truecolor', COLORFGBG: '15;0', LANG: process.env.LANG || 'en_US.UTF-8' };
    if (machine === c.self) {
      if (codex) {
        if (profile !== 'codex') env.CODEX_HOME = path.join(HOME, `.${profile}`);
        const cwd = dir === '~' ? HOME : dir.startsWith('~/') ? path.join(HOME, dir.slice(2)) : dir;
        // --no-daemon: without it Codex starts a shared background service that keeps the thread (and blocks the Codex app,
        // "open in another app") after this terminal closes.
        return fs.existsSync(cwd) ? { argv: [codexBin(), 'resume', '--no-daemon', codex.sessionId, '--dangerously-bypass-approvals-and-sandbox'], env, cwd } : null;
      }
      if (profile !== 'default') env.CLAUDE_CONFIG_DIR = path.join(HOME, `.claude-${profile}`);
      if (tmux) return { argv: [tmuxBin(), '-L', tmux.socket, 'attach', '-t', tmux.target], env, cwd: HOME };
      if (dir == null) return { argv: [claudeBin(), 'attach', job], env, cwd: HOME };
      const cwd = dir === '~' ? HOME : dir.startsWith('~/') ? path.join(HOME, dir.slice(2)) : dir;
      return fs.existsSync(cwd) ? { argv: [claudeBin()], env, cwd } : null;
    }
    const m = c.machines.find((x) => x.name === machine);
    if (!m) return null;
    const cfg = codex ? (profile === 'codex' ? '' : ` CODEX_HOME=$HOME/.${profile}`) : profile === 'default' ? '' : ` CLAUDE_CONFIG_DIR=$HOME/.claude-${profile}`;
    const there = dir == null ? null : dir === '~' ? '"$HOME"' : dir.startsWith('~/') ? `"$HOME"/${shq(dir.slice(2))}` : shq(dir);
    const inner = `export XDG_RUNTIME_DIR=/run/user/$(id -u) TERM=xterm-256color COLORTERM=truecolor COLORFGBG='15;0' LANG=C.UTF-8${cfg}; `
      + (tmux ? `exec tmux -L ${tmux.socket} attach -t ${tmux.target}`
        : codex ? `cd -- ${there} && exec codex resume --no-daemon ${codex.sessionId} --dangerously-bypass-approvals-and-sandbox`
        : there ? `cd -- ${there} && exec claude` : `exec claude attach ${job}`);
    const remote = m.user ? `sudo -n -u ${m.user} -H bash -lc ${shq(inner)}` : `bash -lc ${shq(inner)}`;
    return { argv: ['ssh', '-tt', ...sshArgs(), m.ssh, remote], env, cwd: HOME };
  }

  /** Open one: streams the terminal's bytes on `res` until either side lets go. The term id is in the X-Term-Id header. */
  function open(body, res) {
    const cmd = command(String(body?.machine ?? ''), String(body?.profile ?? ''), String(body?.id ?? ''), typeof body?.trust === 'string' ? body.trust : null,
      body?.tmux && typeof body.tmux === 'object' ? { socket: String(body.tmux.socket ?? ''), target: String(body.tmux.target ?? '') } : null,
      body?.codex && typeof body.codex === 'object' ? { sessionId: String(body.codex.sessionId ?? ''), cwd: String(body.codex.cwd ?? '') } : null);
    if (!cmd) { res.writeHead(422, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ ok: false, error: 'Pick a background session, or a folder that exists.' })); }
    if (terms.size >= MAX) { res.writeHead(429, { 'content-type': 'application/json' }); return res.end(JSON.stringify({ ok: false, error: 'Too many terminals open.' })); }
    const cols = clamp(body?.cols, 20, 400, 100), rows = clamp(body?.rows, 5, 200, 30);
    const id = crypto.randomBytes(12).toString('hex');
    const child = spawn(python(), ['-c', PTY, String(cols), String(rows), ...cmd.argv],
      { env: cmd.env, cwd: cmd.cwd, stdio: ['pipe', 'pipe', 'pipe', 'pipe'] });
    terms.set(id, { child });
    res.writeHead(200, { 'content-type': 'application/octet-stream', 'cache-control': 'no-store', 'x-term-id': id, 'x-content-type-options': 'nosniff' });
    res.flushHeaders();
    child.stdout.on('data', (d) => res.write(d));
    child.stderr.on('data', (d) => res.write(d));
    const done = () => { if (!terms.has(id)) return; terms.delete(id); try { child.kill('SIGTERM'); } catch {} };
    child.on('exit', () => { done(); res.end(); });
    child.on('error', (e) => { done(); res.end(`\r\n${e.message}\r\n`); });
    res.on('close', done);
    for (const s of [child.stdin, child.stdio[3]]) s?.on('error', () => {});
  }

  const find = (body) => terms.get(String(body?.term ?? ''));
  /** Keys typed in the page. */
  function input(body) {
    const t = find(body), data = typeof body?.data === 'string' ? body.data : '';
    if (!t) return { ok: false, error: 'That terminal has closed.' };
    if (data.length > 65_536) return { ok: false, error: 'Too much at once.' };
    t.child.stdin.write(data);
    return { ok: true };
  }
  /** The page's terminal changed size. */
  function resize(body) {
    const t = find(body);
    if (!t) return { ok: false, error: 'That terminal has closed.' };
    t.child.stdio[3].write(`${clamp(body?.cols, 20, 400, 100)} ${clamp(body?.rows, 5, 200, 30)}\n`);
    return { ok: true };
  }
  function closeAll() { for (const [id, t] of terms) { terms.delete(id); try { t.child.kill('SIGTERM'); } catch {} } }

  return { open, input, resize, closeAll, command };
}
