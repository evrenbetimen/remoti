import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import { loadSettings, normalizeSignalingUrl, saveSettings } from './settings';

describe('normalizeSignalingUrl', () => {
  it('fills in scheme and path', () => {
    expect(normalizeSignalingUrl(' abc.trycloudflare.com ')).toEqual({ url: 'wss://abc.trycloudflare.com/ws' });
    expect(normalizeSignalingUrl('https://abc.trycloudflare.com')).toEqual({ url: 'wss://abc.trycloudflare.com/ws' });
    expect(normalizeSignalingUrl('wss://example.com/custom#x')).toEqual({ url: 'wss://example.com/custom' });
  });

  it('allows plain ws only on loopback', () => {
    expect(normalizeSignalingUrl('ws://127.0.0.1:8080/ws')).toEqual({ url: 'ws://127.0.0.1:8080/ws' });
    expect(normalizeSignalingUrl('http://localhost:8080')).toEqual({ url: 'ws://localhost:8080/ws' });
    expect('error' in normalizeSignalingUrl('ws://example.com/ws')).toBe(true);
  });

  it('rejects other schemes, credentials and garbage', () => {
    expect('error' in normalizeSignalingUrl('ftp://example.com')).toBe(true);
    expect('error' in normalizeSignalingUrl('wss://user:pw@example.com')).toBe(true);
    expect('error' in normalizeSignalingUrl('   ')).toBe(true);
    expect('error' in normalizeSignalingUrl('wss://')).toBe(true);
  });
});

describe('settings file', () => {
  let dir = '';
  afterEach(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it('round-trips and ignores corrupt or invalid content', async () => {
    dir = await mkdtemp(path.join(tmpdir(), 'remoti-settings-'));
    const file = path.join(dir, 'nested', 'settings.json');
    expect(await loadSettings(file)).toEqual({});
    await saveSettings(file, { signalingUrl: 'wss://example.com/ws' });
    expect(await loadSettings(file)).toEqual({ signalingUrl: 'wss://example.com/ws' });
    expect(JSON.parse(await readFile(file, 'utf8'))).toEqual({ signalingUrl: 'wss://example.com/ws' });
    await writeFile(file, '{not json');
    expect(await loadSettings(file)).toEqual({});
    await writeFile(file, JSON.stringify({ signalingUrl: 'ws://evil.example/ws' }));
    expect(await loadSettings(file)).toEqual({});
  });
});
