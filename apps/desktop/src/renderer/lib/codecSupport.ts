// Detects which stream codecs this runtime can decode, so the host only picks
// one the viewer can actually play. The strings must match the agent's
// `webcodecs_codec` mapping.

import type { StreamCodec } from '../../../shared/protocol';

export const WEBCODECS_STRINGS: Record<StreamCodec, string> = {
  h265: 'hev1.1.6.L153.B0',
  h264: 'avc1.42E033',
  av1: 'av01.0.13M.08',
};

/** Order the agent prefers when several codecs are decodable. */
const ORDER: readonly StreamCodec[] = ['h265', 'h264', 'av1'];

/** Fallback when detection is impossible: every Chromium build decodes H.264. */
const BASELINE: StreamCodec[] = ['h264'];

let cached: Promise<StreamCodec[]> | null = null;

type Probe = (config: VideoDecoderConfig) => Promise<VideoDecoderSupport>;

export async function probeCodecs(probe: Probe): Promise<StreamCodec[]> {
  const found: StreamCodec[] = [];
  for (const codec of ORDER) {
    for (const hardwareAcceleration of ['prefer-hardware', 'no-preference'] as const) {
      try {
        const support = await probe({
          codec: WEBCODECS_STRINGS[codec],
          codedWidth: 1920,
          codedHeight: 1080,
          hardwareAcceleration,
          optimizeForLatency: true,
        });
        if (support.supported) {
          found.push(codec);
          break;
        }
      } catch {
        // A malformed or unknown codec string rejects instead of resolving
        // `supported: false`; treat both the same.
      }
    }
  }
  return found.length > 0 ? found : [...BASELINE];
}

/** Decodable codecs for this window, detected once and then reused. */
export function decodableCodecs(): Promise<StreamCodec[]> {
  if (typeof VideoDecoder === 'undefined') return Promise.resolve([...BASELINE]);
  cached ??= probeCodecs((config) => VideoDecoder.isConfigSupported(config));
  return cached;
}
