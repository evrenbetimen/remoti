import type { RemotiBridge, VideoAck, VideoPacket } from '../../../shared/protocol';

declare global {
  interface Window {
    remoti: RemotiBridge;
  }
}

export const bridge: RemotiBridge = window.remoti;

type VideoListener = (pkt: VideoPacket) => void;

let port: MessagePort | null = null;
let listener: VideoListener | null = null;

// The preload forwards a MessagePort owned by the main process; video packets
// arrive on it without passing through the IPC event channel.
window.addEventListener('message', (e: MessageEvent) => {
  if (e.source !== window || (e.data as { type?: string })?.type !== 'remoti:video-port') return;
  const p = e.ports[0];
  if (!p) return;
  port?.close();
  port = p;
  p.onmessage = (m: MessageEvent<VideoPacket>) => {
    const ack: VideoAck = { ack: m.data.seq };
    listener?.(m.data);
    // Ack after handing the packet to the decoder: this is the flow control.
    p.postMessage(ack);
  };
  p.start();
});

export function onVideo(fn: VideoListener | null): void {
  listener = fn;
}
