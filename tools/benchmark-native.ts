import { NativeMiner, type NativeMinerEvent } from './native-miner.js';

const workers = positive(process.argv[2] ?? '1', 'workers');
const durationMs = positive(process.argv[3] ?? '10000', 'durationMs');
const header = new Uint8Array(148);
header.set([0, 0, 0x8a, 0xdf]); // Height 35,551: post-fork Sandglass.
const rates = new Map<number, number>();

const miner = new NativeMiner(workers, onEvent);
await miner.start();
miner.startJob({
  jobId: 1,
  headerHex: Buffer.from(header).toString('hex'),
  targetHex: '0'.repeat(64), // Impossible target: benchmark cannot find a block.
  nonceOffset: 0,
  nonceStride: workers,
});
await sleep(durationMs);
miner.stop();
await sleep(100);
await miner.close();

console.log(JSON.stringify({
  workers,
  durationMs,
  hashesPerSecond: Math.round([...rates.values()].reduce((total, rate) => total + rate, 0)),
  workerErrors: 0,
}));

function onEvent(event: NativeMinerEvent): void {
  if (event.type === 'hashrate') {
    rates.set(event.worker, (event.hashes * 1_000) / event.elapsedMs);
  } else if (event.type === 'error') {
    throw new Error(event.message);
  }
}

function positive(value: string, name: string): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) throw new Error(`${name} must be a positive integer`);
  return parsed;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
