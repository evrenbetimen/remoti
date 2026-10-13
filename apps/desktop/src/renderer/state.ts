import type { LinkKind, SessionState, UiEvent } from '../../shared/protocol';

export interface SessionInfo {
  state: SessionState;
  peer?: string;
  transport?: LinkKind;
  reason?: string;
}

export interface Stats {
  bitrateBps: number;
  fps: number;
  rttMs: number;
  loss: number;
  transport: LinkKind;
}

export interface AppState {
  agentReady: boolean;
  online: boolean;
  offlineReason?: string;
  deviceId?: string;
  hosting: boolean;
  password?: string;
  allowControl: boolean;
  hostSession?: SessionInfo;
  viewerSession?: SessionInfo;
  hostStats?: Stats;
  viewerStats?: Stats;
  video?: { codec: string; width: number; height: number; fps: number };
  remoteControlAllowed: boolean;
  permissions?: { screen: boolean; input: boolean };
  error?: { code: string; message: string };
}

export const initialState: AppState = {
  agentReady: false,
  online: false,
  hosting: false,
  allowControl: true,
  remoteControlAllowed: true,
};

export type Action = UiEvent | { type: 'local.allowControl'; allowed: boolean } | { type: 'local.dismissError' };

export function reducer(state: AppState, action: Action): AppState {
  switch (action.type) {
    case 'ready':
      return { ...state, agentReady: true };
    case 'agent.exit':
      return {
        ...state,
        agentReady: false,
        online: false,
        hosting: false,
        password: undefined,
        hostSession: undefined,
        viewerSession: undefined,
        error: action.restarting ? state.error : { code: 'agent', message: 'Remoti servisi durdu. Uygulamayı yeniden başlatın.' },
      };
    case 'signaling':
      return {
        ...state,
        online: action.connected,
        deviceId: action.deviceId ?? state.deviceId,
        offlineReason: action.connected ? undefined : action.reason,
      };
    case 'host.ready':
      return { ...state, hosting: true, deviceId: action.deviceId, password: action.password };
    case 'host.stopped':
      return { ...state, hosting: false, password: undefined, hostSession: undefined, hostStats: undefined };
    case 'session': {
      const info: SessionInfo = { state: action.state, peer: action.peer, transport: action.transport, reason: action.reason };
      if (action.role === 'host') {
        return { ...state, hostSession: info, hostStats: action.state === 'ended' ? undefined : state.hostStats };
      }
      const ended = action.state === 'ended';
      return {
        ...state,
        viewerSession: info,
        viewerStats: ended ? undefined : state.viewerStats,
        video: ended ? undefined : state.video,
      };
    }
    case 'video.config':
      return { ...state, video: { codec: action.codec, width: action.width, height: action.height, fps: action.fps } };
    case 'control':
      return { ...state, remoteControlAllowed: action.allowed };
    case 'stats': {
      const s: Stats = { bitrateBps: action.bitrateBps, fps: action.fps, rttMs: action.rttMs, loss: action.loss, transport: action.transport };
      return action.role === 'host' ? { ...state, hostStats: s } : { ...state, viewerStats: s };
    }
    case 'permissions':
      return { ...state, permissions: { screen: action.screen, input: action.input } };
    case 'error':
      return { ...state, error: { code: action.code, message: action.message } };
    case 'local.allowControl':
      return { ...state, allowControl: action.allowed };
    case 'local.dismissError':
      return { ...state, error: undefined };
    default:
      return state;
  }
}

export function formatDeviceId(id?: string): string {
  if (!id) return '— — —';
  return id.replace(/(\d{3})(?=\d)/g, '$1 ');
}

export function formatBitrate(bps: number): string {
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mb/s`;
  return `${Math.round(bps / 1000)} kb/s`;
}
