// Live terminals: a background session's own Claude Code screen in the page, typed into as you.
//
// A pseudo-terminal on this Mac runs `claude attach <id>`: here, or through `ssh -tt` on the machine the session runs on,
// as a channel on that machine's open connection (ssh.mjs). A page reaches all of its terminals over one WebSocket
// (ws.mjs): what each prints goes to it as binary frames, and its keys, sizes and closes come back as small JSON messages.
// The pty is a few lines of Python, so there is no native module to build: it relays bytes both ways and sets the window
// size it is told on fd 3. When the page lets go (or its tab closes), the attach ends; the session keeps running.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { sshArgs, shq } from './ssh.mjs';

const PTY = String.raw`
import os, sys, pty, select, signal, struct, fcntl, termios, time
cols, rows = int(sys.argv[1]), int(sys.argv[2])
def size(fd, c, r): fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', r, c, 0, 0))
pid, fd = pty.fork()
if pid == 0:
    size(0, cols, rows)
    # The size pipe is this relay's alone: a program that outlives the terminal (an ssh master, a server a step started)
    # must not hold it open, or the panel waits on it and the terminal's slot never frees.
    try: os.close(3)
    except OSError: pass
    os.execvp(sys.argv[3], sys.argv[3:])
def gone():
    try: return os.waitpid(pid, os.WNOHANG)[0] != 0
    except ChildProcessError: return True
def stop(*_):
    # Hang up on it, as closing a terminal window does; whatever is still there a moment later is killed.
    for sig, wait in ((signal.SIGHUP, 1.5), (signal.SIGKILL, 0.5)):
        try: os.killpg(pid, sig)
        except OSError: break
        end = time.time() + wait
        while time.time() < end:
            if gone(): os._exit(0)
            time.sleep(0.05)
    os._exit(0)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGHUP, stop)
size(fd, cols, rows)
def put(to, data):
    while data: data = data[os.write(to, data):]
watch, pending = [0, 3, fd], b''
try:
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
except OSError: pass
stop()
`;

const HOME = os.homedir();
const MAX = 24, PER_PAGE = 8;
const PROFILE = /^[\w.-]{1,40}$/, JOB = /^[0-9a-f]{8}$/;
const DIR = /^(~|~\/[^\0\n\r]{0,400}|\/[^\0\n\r]{0,400})$/; // a folder for a trust prompt: absolute or under ~, one line
const TMUX_SOCKET = /^[\w.-]{1,40}$/, TMUX_TARGET = /^[\w.-]{1,60}:\d+\.\d+$/; // a session in tmux: its server and pane
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const codexBin = () => [path.join(HOME, '.local', 'bin', 'codex'), '/opt/homebrew/bin/codex', '/usr/local/bin/codex'].find((f) => fs.existsSync(f)) ?? 'codex';
const tmuxBin = () => ['/opt/homebrew/bin/tmux', '/usr/local/bin/tmux', '/usr/bin/tmux'].find((f) => fs.existsSync(f)) ?? 'tmux';
const python = () => (fs.existsSync('/usr/bin/python3') ? '/usr/bin/python3' : 'python3');
const claudeBin = () => [path.join(HOME, '.local', 'bin', 'claude'), '/opt/homebrew/bin/claude', '/usr/local/bin/claude'].find((f) => fs.existsSync(f)) ?? 'claude';
const clamp = (n, lo, hi, d) => (Number.isInteger(n) ? Math.min(hi, Math.max(lo, n)) : d);

/** `config()` is fleet.json as createFleet reads it: { self, machines: [{ name, ssh, user }] }. `codexBusy(machine,
 *  sessionId)` says why a Codex thread must not be resumed here (it is live somewhere), or nothing. */
export function createTerms({ config, codexBusy = () => null }) {
  const all = new Set(); // every terminal open, for the cap
  const codexOpen = new Set(); // machine:thread of each Codex terminal open: one at a time, never two writers on a thread

  /** The program the pty runs: `claude attach <job>` here, or the same over SSH as the machine's user; with `dir` instead of
   *  a job, Claude Code itself in that folder, so its trust prompt waits for you there. Null for a bad request. */
  function command(machine, profile, job, dir = null, tmux = null, codex = null) {
    if (tmux && !(TMUX_SOCKET.test(tmux.socket ?? '') && TMUX_TARGET.test(tmux.target ?? ''))) return null;
    if (codex && !(UUID.test(codex.sessionId ?? '') && DIR.test(codex.cwd ?? ''))) return null;
    if (codex) dir = codex.cwd;
    if (!PROFILE.test(profile) || (tmux || codex ? false : dir == null ? !JOB.test(job) : !DIR.test(dir))) return null;
    const c = config();
    const env = { ...Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('CLAUDE') && !k.startsWith('FLEET_') && k !== 'TMUX' && k !== 'TMUX_PANE')),
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
    // -e none: what you type is the session's, never ssh's own escapes (~. at the start of a line would cut the connection).
    // LogLevel=ERROR: no "Shared connection … closed" in the pane as you let go; a real error still shows.
    return { argv: ['ssh', '-tt', '-e', 'none', '-o', 'LogLevel=ERROR', ...sshArgs(), m.ssh, remote], env, cwd: HOME };
  }

  /** One terminal, started now: `out(bytes)` gets what it prints, `end(error?)` is called once when it has gone. */
  function start(q, out, end) {
    const machine = String(q?.machine ?? '');
    const codex = q?.codex && typeof q.codex === 'object' ? { sessionId: String(q.codex.sessionId ?? ''), cwd: String(q.codex.cwd ?? '') } : null;
    const cmd = command(machine, String(q?.profile ?? ''), String(q?.id ?? ''), typeof q?.trust === 'string' ? q.trust : null,
      q?.tmux && typeof q.tmux === 'object' ? { socket: String(q.tmux.socket ?? ''), target: String(q.tmux.target ?? '') } : null, codex);
    if (!cmd) return { ok: false, error: 'Pick a background session, or a folder that exists.' };
    if (all.size >= MAX) return { ok: false, error: 'Too many terminals open.' };
    const codexKey = codex ? `${machine}:${codex.sessionId}` : null;
    if (codexKey) {
      if (codexOpen.has(codexKey)) return { ok: false, error: 'It is open in another Fleet terminal already.' };
      const busy = codexBusy(machine, codex.sessionId);
      if (busy) return { ok: false, error: busy };
    }
    const cols = clamp(q?.cols, 20, 400, 100), rows = clamp(q?.rows, 5, 200, 30);
    const child = spawn(python(), ['-c', PTY, String(cols), String(rows), ...cmd.argv], { env: cmd.env, cwd: cmd.cwd, stdio: ['pipe', 'pipe', 'pipe', 'pipe'] });
    const t = { child };
    all.add(t);
    if (codexKey) codexOpen.add(codexKey);
    let over = false;
    const finish = (error) => {
      if (over) return;
      over = true; all.delete(t);
      if (codexKey) codexOpen.delete(codexKey);
      end(error ?? null);
    };
    child.stdout.on('data', out);
    child.stderr.on('data', out);
    child.on('close', () => finish()); // after its output has all been read, so the last of it goes first
    child.on('error', (e) => finish(e.message));
    for (const s of [child.stdin, child.stdio[3]]) s?.on('error', () => {});
    return {
      ok: true,
      write: (d) => { if (!over) child.stdin.write(d); },
      size: (c, r) => { if (!over) child.stdio[3].write(`${clamp(c, 20, 400, 100)} ${clamp(r, 5, 200, 30)}\n`); },
      stop: () => { if (!over) try { child.kill('SIGTERM'); } catch {} },
      pause: () => child.stdout.pause(),
      resume: () => child.stdout.resume(),
    };
  }

  /** A page's connection (ws.mjs): it opens terminals by a number of its own, types into them, sizes and closes them; they
   *  all close when it does. What a terminal prints goes as a binary frame, its number (4 bytes) first. When the page
   *  can't keep up, every terminal of it waits (their programs block on output) until it has caught up. */
  function serve(ws) {
    const mine = new Map(); // number → terminal
    let held = false;
    const tell = (m) => ws.send(JSON.stringify(m));
    const frame = (ref, d) => {
      const b = Buffer.allocUnsafe(4 + d.length);
      b.writeUInt32BE(ref, 0);
      d.copy(b, 4);
      ws.send(b);
      if (!held && ws.backlog > 1 << 20) { held = true; for (const t of mine.values()) t.pause(); }
    };
    ws.on('drain', () => { if (!held) return; held = false; for (const t of mine.values()) t.resume(); });
    ws.on('message', (m) => {
      if (typeof m !== 'string') return;
      let q;
      try { q = JSON.parse(m); } catch { return; }
      const ref = Number.isInteger(q?.ref) && q.ref > 0 && q.ref < 2 ** 32 ? q.ref : null;
      if (!ref) return;
      const t = mine.get(ref);
      if (q.op === 'in') { if (t && typeof q.data === 'string') t.write(q.data); return; }
      if (q.op === 'size') { if (t) t.size(q.cols, q.rows); return; }
      if (q.op === 'close') { if (t) t.stop(); return; }
      if (q.op !== 'open' || t) return;
      if (mine.size >= PER_PAGE) return tell({ op: 'end', ref, error: 'Too many terminals open in this tab.' });
      const r = start(q, (d) => frame(ref, d), (error) => { mine.delete(ref); tell({ op: 'end', ref, ...(error ? { error } : {}) }); });
      if (!r.ok) return tell({ op: 'end', ref, error: r.error });
      mine.set(ref, r);
      if (held) r.pause();
    });
    ws.on('close', () => { for (const t of mine.values()) t.stop(); mine.clear(); });
  }

  return { start, serve, command, get open() { return all.size; } };
}
