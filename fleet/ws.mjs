// A WebSocket, the server's side, for the live terminals: one connection per tab carries every terminal it has open. A
// browser keeps at most six connections to one site, so a stream per terminal starved everything else on the page.
// RFC 6455, only what a browser on this machine sends: masked frames, text and binary, fragments, ping, close.

import crypto from 'node:crypto';
import { EventEmitter } from 'node:events';

const GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
const MAX_MESSAGE = 1 << 20; // what one message from the page may be (a paste), after its fragments are joined

/** Answer an upgrade request; the connection, or null when it isn't a WebSocket handshake (refused). */
export function accept(req, socket, head) {
  const key = req.headers['sec-websocket-key'];
  if (String(req.headers.upgrade ?? '').toLowerCase() !== 'websocket' || req.headers['sec-websocket-version'] !== '13' || !/^[A-Za-z0-9+/]{22}==$/.test(key ?? '')) {
    refuse(socket, 400, 'Bad Request');
    return null;
  }
  const answer = crypto.createHash('sha1').update(key + GUID).digest('base64');
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${answer}\r\n\r\n`);
  socket.setNoDelay(true);
  const ws = new Socket(socket);
  if (head?.length) ws.feed(head);
  return ws;
}

/** Turn an upgrade request away with an HTTP status. */
export function refuse(socket, status, text) {
  try { socket.end(`HTTP/1.1 ${status} ${text}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`); } catch {}
}

/** Events: 'message' (string | Buffer), 'drain', 'close'. */
class Socket extends EventEmitter {
  constructor(socket) {
    super();
    this.socket = socket;
    this.buf = Buffer.alloc(0);
    this.parts = null; // a fragmented message: { op, list, size }
    this.closed = false;
    socket.on('data', (d) => this.feed(d));
    // A page that went without a goodbye (a tab killed, the browser quit) only ends its side: that is a close too, or its
    // terminals would run on, held by a connection nobody is at.
    socket.on('end', () => this.close(1001));
    socket.on('drain', () => this.emit('drain'));
    socket.on('error', () => socket.destroy());
    socket.on('close', () => { if (!this.closed) { this.closed = true; this.emit('close'); } });
  }

  /** How much is waiting to go out: past a megabyte, whoever writes should wait for 'drain'. */
  get backlog() { return this.socket.writableLength; }

  feed(d) {
    this.buf = this.buf.length ? Buffer.concat([this.buf, d]) : d;
    while (!this.closed && this.buf.length >= 2) {
      const b0 = this.buf[0], b1 = this.buf[1];
      const fin = (b0 & 0x80) !== 0, op = b0 & 0x0f;
      let len = b1 & 0x7f, at = 2;
      if (!(b1 & 0x80)) return this.close(1002); // a browser always masks
      if (len === 126) { if (this.buf.length < 4) return; len = this.buf.readUInt16BE(2); at = 4; }
      else if (len === 127) {
        if (this.buf.length < 10) return;
        const big = this.buf.readBigUInt64BE(2);
        if (big > BigInt(MAX_MESSAGE)) return this.close(1009);
        len = Number(big); at = 10;
      }
      if (len > MAX_MESSAGE) return this.close(1009);
      if (this.buf.length < at + 4 + len) return;
      const mask = this.buf.subarray(at, at + 4);
      const data = Buffer.from(this.buf.subarray(at + 4, at + 4 + len));
      for (let i = 0; i < data.length; i++) data[i] ^= mask[i & 3];
      this.buf = this.buf.subarray(at + 4 + len);
      this.frame(fin, op, data);
    }
  }

  frame(fin, op, data) {
    if (op === 0x8) return this.close(1000);
    if (op === 0x9) return this.write(0xa, data);
    if (op === 0xa) return;
    if (op === 0x0) {
      if (!this.parts) return this.close(1002);
      this.parts.list.push(data); this.parts.size += data.length;
      if (this.parts.size > MAX_MESSAGE) return this.close(1009);
      if (!fin) return;
      const { op: first, list } = this.parts;
      this.parts = null;
      return this.deliver(first, Buffer.concat(list));
    }
    if (op !== 0x1 && op !== 0x2) return this.close(1002);
    if (this.parts) return this.close(1002);
    if (!fin) { this.parts = { op, list: [data], size: data.length }; return; }
    this.deliver(op, data);
  }

  deliver(op, data) { this.emit('message', op === 0x1 ? data.toString('utf8') : data); }

  /** Send a string as text, a Buffer as binary. False once the socket has a backlog (see 'drain'). */
  send(data) { return this.write(typeof data === 'string' ? 0x1 : 0x2, typeof data === 'string' ? Buffer.from(data) : data); }

  write(op, payload) {
    if (this.closed || this.socket.destroyed) return false;
    const n = payload.length;
    const head = Buffer.alloc(n < 126 ? 2 : n < 65536 ? 4 : 10);
    head[0] = 0x80 | op;
    if (n < 126) head[1] = n;
    else if (n < 65536) { head[1] = 126; head.writeUInt16BE(n, 2); }
    else { head[1] = 127; head.writeBigUInt64BE(BigInt(n), 2); }
    this.socket.write(head);
    return this.socket.write(payload);
  }

  close(code = 1000) {
    if (this.closed) return;
    const body = Buffer.alloc(2);
    body.writeUInt16BE(code, 0);
    this.write(0x8, body);
    this.closed = true;
    this.socket.end();
    setTimeout(() => this.socket.destroy(), 1000).unref?.();
    this.emit('close');
  }
}
