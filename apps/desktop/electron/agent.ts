// Spawns and supervises the Rust agent, translating its stdio frames into
// typed events (JSON) and video packets.

import { type ChildProcessWithoutNullStreams, spawn } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { existsSync } from 'node:fs';
import path from 'node:path';

import type { UiCommand, UiEvent } from '../shared/protocol';
import { FrameReader, FramingError, KIND_JSON, encodeJson } from './framing';

export interface AgentVideo {
  seq: number;
  ptsUs: number;
  keyframe: boolean;
  data: Uint8Array;
}

export interface AgentOptions {
  binary: string;
  args: string[];
  env?: NodeJS.ProcessEnv;
}

interface AgentEvents {
  event: [UiEvent];
  video: [AgentVideo];
}

const MAX_RESTARTS = 5;
const RESTART_WINDOW_MS = 60_000;
/** Above this, input commands are dropped instead of queued in Node. */
const MAX_STDIN_BACKLOG = 1024 * 1024;
const KILL_GRACE_MS = 2_000;

export function resolveAgentBinary(isPackaged: boolean, resourcesPath: string, appPath: string): string {
  const exe = process.platform === 'win32' ? 'remoti-agent.exe' : 'remoti-agent';
  const fromEnv = process.env.REMOTI_AGENT_PATH;
  if (fromEnv) return fromEnv;
  if (isPackaged) return path.join(resourcesPath, 'bin', exe);
  // Dev: prefer a release build, fall back to debug.
  const repo = path.resolve(appPath, '..', '..');
  const release = path.join(repo, 'target', 'release', exe);
  return existsSync(release) ? release : path.join(repo, 'target', 'debug', exe);
}

export class AgentProcess extends EventEmitter<AgentEvents> {
  private child: ChildProcessWithoutNullStreams | null = null;
  private restarts: number[] = [];
  private stopping = false;
  /** Set while `restart` replaces the agent, so its exit is not reported as a crash. */
  private reconfiguring = false;

  constructor(private opts: AgentOptions) {
    super();
  }

  start(): void {
    this.stopping = false;
    this.spawnChild();
  }

  /** Replaces the agent's arguments; takes effect on the next start. */
  setArgs(args: string[]): void {
    this.opts = { ...this.opts, args };
  }

  /** Stops the running agent and starts it again with new arguments. */
  async restart(args: string[]): Promise<void> {
    this.setArgs(args);
    this.reconfiguring = true;
    try {
      await this.stop();
    } finally {
      this.reconfiguring = false;
    }
    this.start();
  }

  private spawnChild(): void {
    const child = spawn(this.opts.binary, [...this.opts.args, 'ipc'], {
      env: { ...process.env, ...this.opts.env },
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    this.child = child;
    const reader = new FrameReader();

    child.stdout.on('data', (chunk: Buffer) => {
      let frames;
      try {
        frames = reader.push(chunk);
      } catch (err) {
        // A corrupted stream cannot be resynchronised: restart the agent.
        const message = err instanceof FramingError ? err.message : String(err);
        this.emit('event', { type: 'error', code: 'ipc_framing', message });
        child.kill();
        return;
      }
      for (const f of frames) {
        if (f.kind === KIND_JSON) {
          this.emit('event', f.json as UiEvent);
        } else {
          this.emit('video', { seq: f.seq, ptsUs: f.ptsUs, keyframe: f.keyframe, data: f.data });
        }
      }
    });

    // Agent logs (tracing) arrive on stderr; forward them to the main log.
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (line: string) => process.stderr.write(`[agent] ${line}`));

    child.stdin.on('error', () => {
      // EPIPE when the agent exits; handled by the 'exit' listener.
    });

    child.on('error', (err) => {
      this.emit('event', { type: 'error', code: 'agent_spawn', message: `${this.opts.binary}: ${err.message}` });
    });

    child.on('exit', (code) => {
      // A child killed after its stop timed out can exit after its
      // replacement has started; it no longer owns the agent slot.
      if (this.child !== child) return;
      this.child = null;
      if (this.reconfiguring) {
        this.emit('event', { type: 'agent.exit', code, restarting: true });
        return;
      }
      const now = Date.now();
      this.restarts = this.restarts.filter((t) => now - t < RESTART_WINDOW_MS);
      const restarting = !this.stopping && this.restarts.length < MAX_RESTARTS;
      this.emit('event', { type: 'agent.exit', code, restarting });
      if (restarting) {
        this.restarts.push(now);
        const delay = 250 * 2 ** this.restarts.length;
        setTimeout(() => {
          if (!this.stopping) this.spawnChild();
        }, delay);
      }
    });
  }

  send(cmd: UiCommand): void {
    const child = this.child;
    if (!child || !child.stdin.writable) return;
    if (cmd.type === 'viewer.input' && child.stdin.writableLength > MAX_STDIN_BACKLOG) {
      return; // agent is not keeping up; never queue unbounded input
    }
    child.stdin.write(encodeJson(cmd));
  }

  /** Graceful stop: closing stdin makes the agent exit on its own. */
  async stop(): Promise<void> {
    this.stopping = true;
    const child = this.child;
    if (!child) return;
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        child.kill();
        resolve();
      }, KILL_GRACE_MS);
      child.once('exit', () => {
        clearTimeout(timer);
        resolve();
      });
      child.stdin.end();
    });
  }
}
