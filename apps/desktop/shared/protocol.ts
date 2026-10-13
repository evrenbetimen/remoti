// Types shared by the Electron main process, the preload bridge and the
// renderer. They mirror `crates/core_engine/src/ipc.rs` and
// `crates/proto/src/input.rs`; keep them in sync.

export type MouseButton = 'left' | 'right' | 'middle' | 'back' | 'forward';

/** DOM `KeyboardEvent.code` values the agent accepts (anything else is rejected there too). */
export const KEY_CODES = new Set<string>([
  ...'ABCDEFGHIJKLMNOPQRSTUVWXYZ'.split('').map((c) => `Key${c}`),
  ...'0123456789'.split('').map((d) => `Digit${d}`),
  ...Array.from({ length: 12 }, (_, i) => `F${i + 1}`),
  'Enter', 'Escape', 'Backspace', 'Tab', 'Space', 'Minus', 'Equal', 'BracketLeft', 'BracketRight',
  'Backslash', 'Semicolon', 'Quote', 'Backquote', 'Comma', 'Period', 'Slash', 'CapsLock',
  'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown', 'Insert', 'Delete',
  'ShiftLeft', 'ShiftRight', 'ControlLeft', 'ControlRight', 'AltLeft', 'AltRight', 'MetaLeft', 'MetaRight',
]);

/** Externally tagged, matching serde's default enum representation. */
export type InputEvent =
  | { mouseMove: { x: number; y: number } }
  | { mouseButton: { button: MouseButton; down: boolean } }
  | { wheel: { dx: number; dy: number } }
  | { key: { code: string; down: boolean } }
  | { text: { text: string } }
  | 'releaseAll';

export type UiCommand =
  | { type: 'host.start'; allowControl: boolean }
  | { type: 'host.stop' }
  | { type: 'host.rotatePassword' }
  | { type: 'host.setControl'; allowed: boolean }
  | { type: 'viewer.connect'; deviceId: string; password: string }
  | { type: 'viewer.disconnect' }
  | { type: 'viewer.input'; event: InputEvent }
  | { type: 'viewer.requestKeyframe' }
  | { type: 'viewer.report'; framesDropped: number; decodeQueue: number }
  | { type: 'permissions.check' }
  | { type: 'permissions.request' };

export type SessionRole = 'host' | 'viewer';
export type SessionState = 'connecting' | 'connected' | 'reconnecting' | 'ended';
export type LinkKind = 'quic' | 'relay';

export type UiEvent =
  | { type: 'ready'; version: string; platform: string }
  | { type: 'signaling'; connected: boolean; deviceId?: string; reason?: string }
  | { type: 'host.ready'; deviceId: string; password: string }
  | { type: 'host.stopped' }
  | { type: 'session'; role: SessionRole; state: SessionState; peer?: string; transport?: LinkKind; reason?: string }
  | { type: 'video.config'; codec: string; width: number; height: number; fps: number }
  | { type: 'control'; allowed: boolean }
  | { type: 'stats'; role: SessionRole; bitrateBps: number; fps: number; rttMs: number; loss: number; transport: LinkKind }
  | { type: 'permissions'; screen: boolean; input: boolean }
  | { type: 'error'; code: string; message: string }
  /** Synthesized by the main process, not the agent. */
  | { type: 'agent.exit'; code: number | null; restarting: boolean };

/** One encoded access unit delivered to the renderer over a MessagePort. */
export interface VideoPacket {
  seq: number;
  ptsUs: number;
  keyframe: boolean;
  data: Uint8Array;
}

/** Renderer -> main flow control on the video port. */
export interface VideoAck {
  ack: number;
}

/** API exposed on `window.remoti` by the preload script. */
export interface RemotiBridge {
  send(cmd: UiCommand): void;
  onEvent(listener: (ev: UiEvent) => void): () => void;
  openPrivacySettings(kind: 'screen' | 'accessibility'): void;
  platform: string;
}
