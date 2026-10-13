import { describe, expect, it } from 'vitest';

import { FrameReader, FramingError, KIND_JSON, KIND_VIDEO, encodeJson } from './framing';

function videoFrame(seq: number, data: number[]): Buffer {
  const header = Buffer.alloc(16);
  header.writeBigUInt64LE(1234n, 0);
  header.writeUInt32LE(seq, 8);
  header[12] = 1;
  header[13] = 1;
  const body = Buffer.concat([Buffer.from([KIND_VIDEO]), header, Buffer.from(data)]);
  const len = Buffer.alloc(4);
  len.writeUInt32LE(body.length, 0);
  return Buffer.concat([len, body]);
}

describe('FrameReader', () => {
  it('parses frames split across arbitrary chunk boundaries', () => {
    const stream = Buffer.concat([encodeJson({ type: 'ready' }), videoFrame(7, [0, 0, 1, 0x65])]);
    const reader = new FrameReader();
    const frames = [];
    for (let i = 0; i < stream.length; i += 3) {
      frames.push(...reader.push(stream.subarray(i, i + 3)));
    }
    expect(frames).toHaveLength(2);
    expect(frames[0]).toEqual({ kind: KIND_JSON, json: { type: 'ready' } });
    const v = frames[1];
    expect(v.kind).toBe(KIND_VIDEO);
    if (v.kind === KIND_VIDEO) {
      expect(v.seq).toBe(7);
      expect(v.keyframe).toBe(true);
      expect(Array.from(v.data)).toEqual([0, 0, 1, 0x65]);
    }
  });

  it('rejects oversized frames before buffering them', () => {
    const bad = Buffer.alloc(4);
    bad.writeUInt32LE(64 * 1024 * 1024, 0);
    expect(() => new FrameReader().push(bad)).toThrow(FramingError);
  });
});
