// Persistent user settings, stored as JSON in the app's userData folder.

import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import path from 'node:path';

export interface Settings {
  /** Signaling WebSocket URL chosen by the user; absent means the built-in default. */
  signalingUrl?: string;
}

const LOOPBACK = new Set(['localhost', '127.0.0.1', '[::1]']);

/**
 * Turns user input into a signaling URL, or returns an error message.
 * Accepts bare hosts and http(s) URLs (mapped to ws(s)), defaults the path to
 * `/ws`, and only allows unencrypted `ws://` for loopback development servers.
 */
export function normalizeSignalingUrl(input: string): { url: string } | { error: string } {
  const raw = input.trim();
  if (!raw) return { error: 'Sunucu adresi boş olamaz.' };
  let url: URL;
  try {
    url = new URL(/^[a-z][a-z0-9+.-]*:\/\//i.test(raw) ? raw : `wss://${raw}`);
  } catch {
    return { error: 'Geçerli bir adres girin, örneğin wss://remoti.example.com/ws' };
  }
  if (url.protocol === 'https:') url.protocol = 'wss:';
  else if (url.protocol === 'http:') url.protocol = 'ws:';
  if (url.protocol !== 'wss:' && url.protocol !== 'ws:') {
    return { error: 'Adres wss:// ile başlamalı.' };
  }
  if (url.protocol === 'ws:' && !LOOPBACK.has(url.hostname)) {
    return { error: 'Şifresiz ws:// yalnızca bu bilgisayardaki bir sunucu için kullanılabilir; wss:// kullanın.' };
  }
  if (url.username || url.password) return { error: 'Adres kullanıcı adı veya parola içeremez.' };
  if (url.pathname === '' || url.pathname === '/') url.pathname = '/ws';
  url.hash = '';
  return { url: url.toString() };
}

export function settingsFile(userData: string): string {
  return path.join(userData, 'settings.json');
}

export async function loadSettings(file: string): Promise<Settings> {
  let text: string;
  try {
    text = await readFile(file, 'utf8');
  } catch {
    return {}; // first run, or unreadable: fall back to defaults
  }
  try {
    const parsed = JSON.parse(text) as unknown;
    if (typeof parsed !== 'object' || parsed === null) return {};
    const candidate = (parsed as { signalingUrl?: unknown }).signalingUrl;
    if (typeof candidate !== 'string') return {};
    const normalized = normalizeSignalingUrl(candidate);
    return 'url' in normalized ? { signalingUrl: normalized.url } : {};
  } catch {
    return {}; // corrupt file: ignore rather than refuse to start
  }
}

/** Writes atomically so a crash mid-write never leaves a truncated file. */
export async function saveSettings(file: string, settings: Settings): Promise<void> {
  await mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  await writeFile(tmp, `${JSON.stringify(settings, null, 2)}\n`, 'utf8');
  await rename(tmp, file);
}
