// Low-latency WebCodecs pipeline: Annex B H.264 access units in, frames on a
// canvas out. Only the newest decoded frame is ever drawn; older ones are
// closed immediately, so GPU memory stays bounded however far decode or
// paint fall behind.

import type { VideoPacket } from '../../../shared/protocol';

export interface DecoderStats {
  decoded: number;
  dropped: number;
  queue: number;
}

export interface StreamConfig {
  codec: string;
  width: number;
  height: number;
}

const MAX_DECODE_QUEUE = 4;

export class LowLatencyDecoder {
  private decoder: VideoDecoder | null = null;
  private config: StreamConfig | null = null;
  private pending: VideoFrame | null = null;
  private rafId = 0;
  private needKeyframe = true;
  private stats: DecoderStats = { decoded: 0, dropped: 0, queue: 0 };
  private readonly ctx: CanvasRenderingContext2D;

  constructor(
    private readonly canvas: HTMLCanvasElement,
    private readonly requestKeyframe: () => void,
  ) {
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    if (!ctx) throw new Error('2D canvas unavailable');
    this.ctx = ctx;
  }

  async configure(cfg: StreamConfig): Promise<void> {
    if (!('VideoDecoder' in window)) throw new Error('WebCodecs is not available in this runtime');
    // Prefer the GPU decoder, but fall back to software (VMs, remote desktops,
    // blocklisted drivers) rather than failing the session.
    let chosen: VideoDecoderConfig | null = null;
    for (const hardwareAcceleration of ['prefer-hardware', 'no-preference'] as const) {
      const candidate: VideoDecoderConfig = {
        codec: cfg.codec,
        codedWidth: cfg.width,
        codedHeight: cfg.height,
        hardwareAcceleration,
        optimizeForLatency: true,
      };
      const support = await VideoDecoder.isConfigSupported(candidate);
      if (support.supported) {
        chosen = candidate;
        break;
      }
    }
    if (!chosen) throw new Error(`codec ${cfg.codec} ${cfg.width}x${cfg.height} not supported`);
    this.reset();
    this.config = cfg;
    this.canvas.width = cfg.width;
    this.canvas.height = cfg.height;
    this.decoder = new VideoDecoder({
      output: (frame) => this.onFrame(frame),
      error: (e) => {
        console.warn('decoder error', e);
        this.recover();
      },
    });
    // No `description`: the Annex B stream carries SPS/PPS in-band with keyframes.
    this.decoder.configure(chosen);
    this.needKeyframe = true;
    this.requestKeyframe();
  }

  push(pkt: VideoPacket): void {
    const dec = this.decoder;
    if (!dec || dec.state !== 'configured') return;
    if (this.needKeyframe && !pkt.keyframe) {
      this.stats.dropped += 1;
      return;
    }
    // Decoder falling behind: skip deltas until the next keyframe.
    if (dec.decodeQueueSize > MAX_DECODE_QUEUE && !pkt.keyframe) {
      this.stats.dropped += 1;
      this.needKeyframe = true;
      this.requestKeyframe();
      return;
    }
    try {
      dec.decode(new EncodedVideoChunk({ type: pkt.keyframe ? 'key' : 'delta', timestamp: pkt.ptsUs, data: pkt.data }));
      this.needKeyframe = false;
    } catch (e) {
      console.warn('decode failed', e);
      this.recover();
    }
  }

  takeStats(): DecoderStats {
    const s = { ...this.stats, queue: this.decoder?.decodeQueueSize ?? 0 };
    this.stats = { decoded: 0, dropped: 0, queue: 0 };
    return s;
  }

  private onFrame(frame: VideoFrame): void {
    this.stats.decoded += 1;
    if (this.pending) {
      this.pending.close();
      this.stats.dropped += 1;
    }
    this.pending = frame;
    if (!this.rafId) this.rafId = requestAnimationFrame(() => this.paint());
  }

  private paint(): void {
    this.rafId = 0;
    const frame = this.pending;
    this.pending = null;
    if (!frame) return;
    try {
      this.ctx.drawImage(frame, 0, 0, this.canvas.width, this.canvas.height);
    } finally {
      frame.close();
    }
  }

  private recover(): void {
    const cfg = this.config;
    this.reset();
    if (cfg) {
      void this.configure(cfg).catch((e) => console.error('decoder reconfigure failed', e));
    }
  }

  reset(): void {
    if (this.rafId) cancelAnimationFrame(this.rafId);
    this.rafId = 0;
    this.pending?.close();
    this.pending = null;
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close();
    this.decoder = null;
    this.needKeyframe = true;
  }
}
