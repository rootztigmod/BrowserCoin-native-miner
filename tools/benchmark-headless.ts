import { Worker } from 'node:worker_threads';

const WORKERS = parsePositive(process.argv[2] ?? '1', 'workers');
const DURATION_MS = parsePositive(process.argv[3] ?? '10000', 'durationMs');

const header = new Uint8Array(148);
const height = 35_551;
header[0] = (height >>> 24) & 0xff;
header[1] = (height >>> 16) & 0xff;
header[2] = (height >>> 8) & 0xff;
header[3] = height & 0xff;

const results = await Promise.all(Array.from({ length: WORKERS }, (_, index) => runWorker(index)));
const totalHashes = results.reduce((sum, result) => sum + result.hashes, 0);
const totalElapsedMs = results.reduce((sum, result) => sum + result.elapsedMs, 0);
const averageElapsedMs = totalElapsedMs / WORKERS;

console.log(JSON.stringify({
  workers: WORKERS,
  durationMs: Math.round(averageElapsedMs),
  hashes: totalHashes,
  hashesPerSecond: Math.round((totalHashes * 1_000) / averageElapsedMs),
  workerErrors: 0,
}));

function runWorker(index: number): Promise<{ hashes: number; elapsedMs: number }> {
  const worker = new Worker(new URL('./dist/headless-miner.worker.mjs', import.meta.url), {
    workerData: {
      headerHex: Buffer.from(header).toString('hex'),
      // Zero is an impossible target, so this benchmark never submits a block.
      targetHex: '0'.repeat(64),
      startNonce: index,
      stride: WORKERS,
      benchmarkDurationMs: DURATION_MS,
    },
  });
  return new Promise((resolve, reject) => {
    worker.on('message', (message: { type: string; hashes?: number; elapsedMs?: number }) => {
      if (message.type !== 'benchmark') return;
      resolve({ hashes: message.hashes ?? 0, elapsedMs: message.elapsedMs ?? DURATION_MS });
      void worker.terminate();
    });
    worker.on('error', reject);
  });
}

function parsePositive(value: string, name: string): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) throw new Error(`${name} must be a positive integer`);
  return parsed;
}

