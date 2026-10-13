// Length-prefixed framing for the agent's stdio channel.
// Frame: len u32 LE (kind + payload) | kind u8 | payload.
// Mirrors crates/core_engine/src/ipc.rs.

export const KIND_JSON = 0x01;
export const KIND_VIDEO = 0x02;
export const MAX_FRAME = 16 * 1024 * 1024;
export const VIDEO_HEADER = 16;

export type Frame =
  | { kind: typeof KIND_JSON; json: unknown }
  | { kind: typeof KIND_VIDEO; seq: number; ptsUs: number; keyframe: boolean; codec: number; data: Uint8Array };

export class FramingError extends Error {}

/**
 * Incremental decoder. Chunks are kept in a list and only merged when a frame
 * spans them, so steady-state parsing does not re-copy the whole buffer.
 */
export class FrameReader {
  private chunks: Buffer[] = [];
  private buffered = 0;

  push(chunk: Buffer): Frame[] {
    this.chunks.push(chunk);
    this.buffered += chunk.length;
    const out: Frame[] = [];
    for (;;) {
      if (this.buffered < 4) break;
      const head = this.peek(4);
      const len = head.readUInt32LE(0);
      if (len === 0 || len > MAX_FRAME) {
        throw new FramingError(`invalid frame length ${len}`);
      }
      if (this.buffered < 4 + len) break;
      const frame = this.take(4 + len).subarray(4);
      out.push(decodeFrame(frame));
    }
    return out;
  }

  private peek(n: number): Buffer {
    if (this.chunks[0].length >= n) return this.chunks[0];
    this.chunks = [Buffer.concat(this.chunks)];
    return this.chunks[0];
  }

  private take(n: number): Buffer {
    const first = this.peek(n);
    // Copy out so the returned frame does not pin a large shared chunk.
    const frame = Buffer.from(first.subarray(0, n));
    const rest = first.subarray(n);
    this.chunks[0] = rest;
    if (rest.length === 0) this.chunks.shift();
    this.buffered -= n;
    return frame;
  }
}

function decodeFrame(frame: Buffer): Frame {
  const kind = frame[0];
  const payload = frame.subarray(1);
  if (kind === KIND_JSON) {
    return { kind: KIND_JSON, json: JSON.parse(payload.toString('utf8')) };
  }
  if (kind === KIND_VIDEO) {
    if (payload.length < VIDEO_HEADER) throw new FramingError('short video frame');
    const ptsUs = Number(payload.readBigUInt64LE(0));
    const seq = payload.readUInt32LE(8);
    const keyframe = (payload[12] & 1) === 1;
    const codec = payload[13];
    const data = new Uint8Array(payload.buffer, payload.byteOffset + VIDEO_HEADER, payload.length - VIDEO_HEADER);
    return { kind: KIND_VIDEO, seq, ptsUs, keyframe, codec, data };
  }
  throw new FramingError(`unknown frame kind ${kind}`);
}

export function encodeJson(value: unknown): Buffer {
  const body = Buffer.from(JSON.stringify(value), 'utf8');
  const out = Buffer.allocUnsafe(5 + body.length);
  out.writeUInt32LE(body.length + 1, 0);
  out[4] = KIND_JSON;
  body.copy(out, 5);
  return out;
}
