import { describe, expect, it } from 'vitest';

import { probeCodecs } from './codecSupport';

describe('probeCodecs', () => {
  it('keeps the agent preference order and falls back to software decode', async () => {
    const seen: string[] = [];
    const codecs = await probeCodecs(async (config) => {
      seen.push(`${config.codec}:${config.hardwareAcceleration}`);
      const supported = config.codec.startsWith('av01') || (config.codec.startsWith('avc1') && config.hardwareAcceleration === 'no-preference');
      return { supported, config };
    });
    expect(codecs).toEqual(['h264', 'av1']);
    expect(seen).toContain('avc1.42E033:no-preference');
  });

  it('treats rejected probes as unsupported and never advertises nothing', async () => {
    const codecs = await probeCodecs(async () => {
      throw new TypeError('bad codec');
    });
    expect(codecs).toEqual(['h264']);
  });
});
