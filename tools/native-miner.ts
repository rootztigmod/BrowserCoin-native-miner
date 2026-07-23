import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createInterface } from 'node:readline';

export interface NativeMinerJob {
  jobId: number;
  headerHex: string;
  targetHex: string;
  nonceOffset: number;
  nonceStride: number;
}

export type NativeMinerEvent =
  | { type: 'ready'; workers: number }
  | { type: 'hashrate'; worker: number; jobId: number; hashes: number; elapsedMs: number }
  | { type: 'solved'; worker: number; jobId: number; nonce: number; hash: string }
  | { type: 'exhausted'; worker: number; jobId: number }
  | { type: 'error'; message: string };

export class NativeMiner {
  private child: ChildProcessWithoutNullStreams | undefined;
  private ready: Promise<void> | undefined;

  constructor(
    private readonly workers: number,
    private readonly onEvent: (event: NativeMinerEvent) => void,
    private readonly executablePath?: string,
  ) {}

  async start(): Promise<void> {
    if (this.ready) return this.ready;
    const executable = this.executablePath
      ?? process.env.BRC_NATIVE_MINER
      ?? fileURLToPath(new URL('../native/target/release/browsercoin-native-miner', import.meta.url));
    await access(executable).catch(() => {
      throw new Error(`native miner not found at ${executable}; run npm run build:native-miner or explicitly pass --engine js`);
    });

    this.ready = new Promise<void>((resolve, reject) => {
      const child = spawn(executable, ['--workers', String(this.workers)], { stdio: ['pipe', 'pipe', 'pipe'] });
      this.child = child;
      let receivedReady = false;
      const fail = (message: string): void => {
        if (!receivedReady) reject(new Error(message));
        else this.onEvent({ type: 'error', message });
      };
      child.once('error', (error) => fail(`native miner failed to start: ${error.message}`));
      child.once('exit', (code, signal) => {
        if (code !== 0) fail(`native miner exited (${code ?? signal ?? 'unknown'})`);
      });
      createInterface({ input: child.stdout }).on('line', (line) => {
        try {
          const event = JSON.parse(line) as NativeMinerEvent;
          if (event.type === 'ready') {
            receivedReady = true;
            resolve();
          } else {
            this.onEvent(event);
          }
        } catch {
          fail(`native miner emitted invalid JSON: ${line}`);
        }
      });
      createInterface({ input: child.stderr }).on('line', (line) => {
        if (line) console.warn(`[miner] native stderr: ${line}`);
      });
    });
    return this.ready;
  }

  startJob(job: NativeMinerJob): void {
    this.send({ type: 'start', ...job });
  }

  stop(): void {
    this.send({ type: 'stop' });
  }

  async close(): Promise<void> {
    if (!this.child) return;
    const child = this.child;
    this.child = undefined;
    this.sendTo(child, { type: 'shutdown' });
    await new Promise<void>((resolve) => child.once('exit', () => resolve()));
  }

  private send(message: object): void {
    if (!this.child?.stdin.writable) throw new Error('native miner is not running');
    this.sendTo(this.child, message);
  }

  private sendTo(child: ChildProcessWithoutNullStreams, message: object): void {
    child.stdin.write(`${JSON.stringify(message)}\n`);
  }
}
