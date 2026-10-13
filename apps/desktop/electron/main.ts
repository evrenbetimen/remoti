// Electron main process: owns the window, the agent process and the video
// MessagePort. The renderer is sandboxed and never touches Node or stdio.

import path from 'node:path';

import { BrowserWindow, MessageChannelMain, type MessagePortMain, app, ipcMain, session, shell, systemPreferences } from 'electron';

import type { UiCommand, UiEvent, VideoAck, VideoPacket } from '../shared/protocol';
import { AgentProcess, type AgentVideo, resolveAgentBinary } from './agent';

/** Video packets sent to the renderer but not yet acknowledged. */
const MAX_UNACKED_FRAMES = 6;
const SIGNALING_URL = process.env.REMOTI_SIGNALING_URL ?? 'wss://signal.remoti.app/ws';

let win: BrowserWindow | null = null;
let videoPort: MessagePortMain | null = null;
let unacked = 0;
let waitingForKeyframe = false;

const agent = new AgentProcess({
  binary: resolveAgentBinary(app.isPackaged, process.resourcesPath, app.getAppPath()),
  args: ['--signaling', SIGNALING_URL, '--log-format', 'json'],
});

const COMMAND_TYPES = new Set<UiCommand['type']>([
  'host.start', 'host.stop', 'host.rotatePassword', 'host.setControl', 'viewer.connect', 'viewer.disconnect',
  'viewer.input', 'viewer.requestKeyframe', 'viewer.report', 'permissions.check', 'permissions.request',
]);

function isCommand(value: unknown): value is UiCommand {
  return typeof value === 'object' && value !== null && COMMAND_TYPES.has((value as { type: UiCommand['type'] }).type);
}

// Latest state-bearing event per kind, replayed when the renderer (re)loads so
// a page that subscribes late still sees agent state emitted before it.
const replay = new Map<string, UiEvent>();
const TRANSIENT = new Set<UiEvent['type']>(['error', 'stats']);

function remember(ev: UiEvent): void {
  if (ev.type === 'agent.exit') replay.clear();
  if (ev.type === 'host.stopped') replay.delete('host.ready');
  if (TRANSIENT.has(ev.type)) return;
  const key = 'role' in ev ? `${ev.type}:${ev.role}` : ev.type;
  replay.set(key, ev);
}

function sendEvent(ev: UiEvent): void {
  remember(ev);
  if (win && !win.isDestroyed()) win.webContents.send('remoti:event', ev);
}

function onVideo(v: AgentVideo): void {
  if (!videoPort) return;
  // Flow control: if the renderer lags, drop instead of buffering, then
  // resynchronise on the next keyframe.
  if (unacked >= MAX_UNACKED_FRAMES || (waitingForKeyframe && !v.keyframe)) {
    if (!waitingForKeyframe) agent.send({ type: 'viewer.requestKeyframe' });
    waitingForKeyframe = true;
    return;
  }
  waitingForKeyframe = false;
  unacked += 1;
  const packet: VideoPacket = { seq: v.seq, ptsUs: v.ptsUs, keyframe: v.keyframe, data: v.data };
  videoPort.postMessage(packet);
}

function attachVideoPort(target: BrowserWindow): void {
  videoPort?.close();
  const { port1, port2 } = new MessageChannelMain();
  videoPort = port1;
  unacked = 0;
  port1.on('message', (e) => {
    const msg = e.data as VideoAck;
    if (typeof msg?.ack === 'number') unacked = Math.max(0, unacked - 1);
  });
  port1.start();
  target.webContents.postMessage('remoti:video-port', null, [port2]);
}

function hardenSession(): void {
  // The UI needs no camera/mic/notifications/etc.; deny everything.
  session.defaultSession.setPermissionRequestHandler((_wc, _perm, cb) => cb(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
}

function createWindow(): void {
  win = new BrowserWindow({
    width: 1280,
    height: 800,
    minWidth: 900,
    minHeight: 600,
    backgroundColor: '#0b0d12',
    title: 'Remoti',
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
      webSecurity: true,
      spellcheck: false,
    },
  });

  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.once('ready-to-show', () => win?.show());
  win.webContents.on('did-finish-load', () => {
    if (!win) return;
    attachVideoPort(win);
    for (const ev of replay.values()) win.webContents.send('remoti:event', ev);
    agent.send({ type: 'permissions.check' });
  });
  win.on('closed', () => {
    win = null;
    videoPort?.close();
    videoPort = null;
  });

  const devServer = process.env.REMOTI_DEV_SERVER;
  if (devServer && !app.isPackaged) {
    void win.loadURL(devServer);
  } else {
    void win.loadFile(path.join(__dirname, '..', '..', 'dist', 'renderer', 'index.html'));
  }
}

ipcMain.on('remoti:command', (e, cmd: unknown) => {
  // Only accept commands from our own top-level frame.
  if (!win || e.sender !== win.webContents || e.senderFrame !== win.webContents.mainFrame) return;
  if (!isCommand(cmd)) return;
  if (cmd.type === 'permissions.request' && process.platform === 'darwin') {
    // Triggers the macOS Accessibility prompt; screen capture is prompted by the agent.
    systemPreferences.isTrustedAccessibilityClient(true);
  }
  agent.send(cmd);
});

ipcMain.on('remoti:open-privacy', (e, kind: unknown) => {
  if (!win || e.sender !== win.webContents || process.platform !== 'darwin') return;
  const pane = kind === 'accessibility' ? 'Privacy_Accessibility' : 'Privacy_ScreenCapture';
  void shell.openExternal(`x-apple.systempreferences:com.apple.preference.security?${pane}`);
});

agent.on('event', (ev) => {
  if (ev.type === 'session' && ev.role === 'viewer' && ev.state !== 'connected') {
    unacked = 0;
    waitingForKeyframe = false;
  }
  sendEvent(ev);
});
agent.on('video', onVideo);

const singleInstance = app.requestSingleInstanceLock();
if (!singleInstance) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (win) {
      if (win.isMinimized()) win.restore();
      win.focus();
    }
  });

  void app.whenReady().then(() => {
    hardenSession();
    agent.start();
    createWindow();
    app.on('activate', () => {
      if (BrowserWindow.getAllWindows().length === 0) createWindow();
    });
  });

  app.on('window-all-closed', () => {
    if (process.platform !== 'darwin') app.quit();
  });

  let quitting = false;
  app.on('before-quit', (e) => {
    if (quitting) return;
    quitting = true;
    e.preventDefault();
    void agent.stop().finally(() => app.quit());
  });
}
