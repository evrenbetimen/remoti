import { chmod, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { afterEach, describe, expect, it } from 'vitest';

import type { UiEvent } from '../shared/protocol';
import { AgentProcess } from './agent';

// A stand-in agent: records its arguments, then waits for stdin to close.
const FAKE_AGENT = '#!/bin/sh\necho "$@" >> "$REMOTI_FAKE_LOG"\ncat > /dev/null\n';

async function until(check: () => Promise<boolean>): Promise<void> {
  for (let i = 0; i < 100; i++) {
    if (await check()) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  throw new Error('condition not met');
}

describe.skipIf(process.platform === 'win32')('AgentProcess.restart', () => {
  let dir = '';
  afterEach(async () => {
    if (dir) await rm(dir, { recursive: true, force: true });
  });

  it('respawns with the new arguments and reports the exit as a restart', async () => {
    dir = await mkdtemp(path.join(tmpdir(), 'remoti-agent-'));
    const binary = path.join(dir, 'agent.sh');
    const log = path.join(dir, 'args.log');
    await writeFile(binary, FAKE_AGENT);
    await chmod(binary, 0o755);
    await writeFile(log, '');

    const agent = new AgentProcess({ binary, args: ['--signaling', 'wss://a/ws'], env: { REMOTI_FAKE_LOG: log } });
    const exits: UiEvent[] = [];
    agent.on('event', (ev) => {
      if (ev.type === 'agent.exit') exits.push(ev);
    });
    const lines = async () => (await readFile(log, 'utf8')).trim().split('\n').filter(Boolean);

    agent.start();
    await until(async () => (await lines()).length === 1);
    await agent.restart(['--signaling', 'wss://b/ws']);
    await until(async () => (await lines()).length === 2);
    await agent.stop();

    expect(await lines()).toEqual(['--signaling wss://a/ws ipc', '--signaling wss://b/ws ipc']);
    expect(exits).toEqual([
      { type: 'agent.exit', code: 0, restarting: true },
      { type: 'agent.exit', code: 0, restarting: false },
    ]);
  });
});
