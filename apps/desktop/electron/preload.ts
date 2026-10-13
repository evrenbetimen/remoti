// Sandboxed preload: exposes a minimal, typed bridge. It must not import local
// modules (sandboxed preloads can only require 'electron'); shared types are
// type-only imports and vanish at compile time.

import { contextBridge, ipcRenderer, type IpcRendererEvent } from 'electron';

import type { RemotiBridge, UiCommand, UiEvent } from '../shared/protocol';

const bridge: RemotiBridge = {
  send(cmd: UiCommand) {
    ipcRenderer.send('remoti:command', cmd);
  },
  onEvent(listener: (ev: UiEvent) => void) {
    const handler = (_e: IpcRendererEvent, ev: UiEvent) => listener(ev);
    ipcRenderer.on('remoti:event', handler);
    return () => {
      ipcRenderer.removeListener('remoti:event', handler);
    };
  },
  openPrivacySettings(kind) {
    ipcRenderer.send('remoti:open-privacy', kind);
  },
  platform: process.platform,
};

contextBridge.exposeInMainWorld('remoti', bridge);

// MessagePorts cannot cross contextBridge; hand the video port to the page
// through window.postMessage (same-origin only).
ipcRenderer.on('remoti:video-port', (e) => {
  window.postMessage({ type: 'remoti:video-port' }, window.location.origin === 'null' ? '*' : window.location.origin, e.ports);
});
