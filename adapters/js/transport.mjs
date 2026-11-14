// Client side of the Testudo wire protocol. Keep in sync with
// crates/protocol/src/lib.rs.
//
//   message := u32 len | u8 kind | body
//   kind 1  := Event  (JSON)
//   kind 2  := Frame  (u32 header_len | JSON header | JPEG)
//
// When TESTUDO_SOCKET is unset (runner started outside Testudo) every call is a
// no-op, so adapters never break a normal test run.

import net from 'node:net';

const KIND_EVENT = 1;
const KIND_FRAME = 2;
const PROTOCOL_VERSION = 1;

// Above this many buffered bytes, frames are dropped (events are always kept).
const FRAME_HIGH_WATER = 8 * 1024 * 1024;

const noop = { send() {}, frame() {}, async flush() {}, async close() {}, enabled: false };

/**
 * @param {{ role: 'reporter' | 'preload', runner: string }} hello
 */
export function connect({ role, runner }) {
  const path = process.env.TESTUDO_SOCKET;
  const runId = process.env.TESTUDO_RUN_ID;
  if (!path || !runId) return noop;

  const socket = net.connect(path);
  let dead = false;
  socket.on('error', () => {
    dead = true;
  });
  // Never keep a test process alive just for telemetry.
  socket.unref();

  const write = (buf) => {
    if (!dead) socket.write(buf);
  };

  const send = (event) => {
    const json = Buffer.from(JSON.stringify(event));
    const head = Buffer.allocUnsafe(5);
    head.writeUInt32BE(json.length + 1, 0);
    head.writeUInt8(KIND_EVENT, 4);
    write(Buffer.concat([head, json]));
  };

  const frame = (header, jpeg) => {
    if (dead || socket.writableLength > FRAME_HIGH_WATER) return;
    const h = Buffer.from(JSON.stringify(header));
    const head = Buffer.allocUnsafe(9);
    head.writeUInt32BE(1 + 4 + h.length + jpeg.length, 0);
    head.writeUInt8(KIND_FRAME, 4);
    head.writeUInt32BE(h.length, 5);
    write(Buffer.concat([head, h, jpeg]));
  };

  // Wait until buffered writes reach the kernel, without closing (watch mode).
  const flush = () =>
    new Promise((resolve) => {
      if (dead || socket.writableLength === 0) return resolve();
      socket.once('drain', resolve);
      setTimeout(resolve, 1000).unref();
    });

  const close = () =>
    new Promise((resolve) => {
      if (dead) return resolve();
      socket.end(resolve);
      setTimeout(resolve, 1000).unref();
    });

  send({ type: 'hello', role, runner, pid: process.pid, runId, protocol: PROTOCOL_VERSION });
  return { send, frame, flush, close, enabled: true };
}
