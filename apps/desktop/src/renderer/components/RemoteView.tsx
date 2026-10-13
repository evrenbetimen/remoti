import { useEffect, useRef, useState } from 'react';

import { bridge, onVideo } from '../lib/bridge';
import { attachInput } from '../lib/input';
import { LowLatencyDecoder } from '../lib/videoDecoder';
import { type AppState, formatBitrate, formatDeviceId } from '../state';

const REPORT_INTERVAL_MS = 500;

export function RemoteView({ state }: { state: AppState }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const decoderRef = useRef<LowLatencyDecoder | null>(null);
  const [decodeError, setDecodeError] = useState<string>();
  const session = state.viewerSession;
  const video = state.video;

  // Decoder lifetime follows the canvas; packets flow straight from the port.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const decoder = new LowLatencyDecoder(canvas, () => bridge.send({ type: 'viewer.requestKeyframe' }));
    decoderRef.current = decoder;
    onVideo((pkt) => decoder.push(pkt));
    const timer = window.setInterval(() => {
      const s = decoder.takeStats();
      bridge.send({ type: 'viewer.report', framesDropped: s.dropped, decodeQueue: s.queue });
    }, REPORT_INTERVAL_MS);
    return () => {
      window.clearInterval(timer);
      onVideo(null);
      decoder.reset();
      decoderRef.current = null;
    };
  }, []);

  useEffect(() => {
    if (!video || !decoderRef.current) return;
    setDecodeError(undefined);
    decoderRef.current.configure(video).catch((e: unknown) => setDecodeError(String(e)));
  }, [video?.codec, video?.width, video?.height]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || session?.state !== 'connected' || !state.remoteControlAllowed) return;
    canvas.focus();
    return attachInput(canvas, (event) => bridge.send({ type: 'viewer.input', event }));
  }, [session?.state, state.remoteControlAllowed]);

  const stats = state.viewerStats;
  return (
    <div className="remote">
      <div className="remote-toolbar">
        <span className={`dot ${session?.state ?? 'ended'}`} />
        <b className="mono">{formatDeviceId(session?.peer)}</b>
        {session?.transport && <span className="pill">{session.transport === 'quic' ? 'P2P QUIC' : 'Relay (TCP)'}</span>}
        {!state.remoteControlAllowed && <span className="pill warn">Yalnızca izleme</span>}
        {stats && (
          <span className="muted">
            {formatBitrate(stats.bitrateBps)} · {stats.fps} fps · {stats.rttMs} ms
          </span>
        )}
        <span className="spacer" />
        <button className="danger ghost" onClick={() => bridge.send({ type: 'viewer.disconnect' })}>
          Bağlantıyı kes
        </button>
      </div>
      <div className="remote-stage">
        <canvas ref={canvasRef} tabIndex={0} className="remote-canvas" />
        {session?.state !== 'connected' && (
          <div className="overlay">{session?.state === 'reconnecting' ? `Yeniden bağlanıyor… ${session.reason ?? ''}` : 'Bağlanıyor…'}</div>
        )}
        {decodeError && <div className="overlay error-text">{decodeError}</div>}
      </div>
    </div>
  );
}
