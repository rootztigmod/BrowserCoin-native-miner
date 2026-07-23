import { parentPort, workerData } from 'node:worker_threads';

import { SANDGLASS_FORK_HEIGHT } from '../src/chain/genesis.js';
import { powHash } from '../src/crypto/pow.js';
import { sandglassHash } from '../src/crypto/sandglass.js';
import { bytesToHex, hashMeetsTarget, hexToBytes } from '../src/util/binary.js';

const NONCE_OFFSET = 112;
const REPORT_INTERVAL_MS = 2_000;

interface WorkerJob {
  headerHex: string;
  targetHex: string;
  startNonce: number;
  stride: number;
  benchmarkDurationMs?: number;
}

async function grind(job: WorkerJob): Promise<void> {
  const header = hexToBytes(job.headerHex);
  const target = BigInt(`0x${job.targetHex}`);
  const height = ((header[0]! << 24) | (header[1]! << 16) | (header[2]! << 8) | header[3]!) >>> 0;
  const useSandglass = height >= SANDGLASS_FORK_HEIGHT;
  let nonce = job.startNonce >>> 0;
  let hashes = 0;
  let reportedAt = Date.now();
  let totalHashes = 0;
  const startedAt = reportedAt;
  const benchmarkEndsAt = job.benchmarkDurationMs === undefined ? undefined : startedAt + job.benchmarkDurationMs;

  while (true) {
    writeNonce(header, nonce);
    // Sandglass is synchronous. Calling its async consensus wrapper creates a
    // Promise/microtask per nonce in Node, which is unnecessary once the
    // height-gated algorithm has been selected for this immutable template.
    const hash = useSandglass ? sandglassHash(header) : await powHash(header);
    hashes++;
    totalHashes++;
    if (hashMeetsTarget(hash, target)) {
      parentPort?.postMessage({ type: 'solved', nonce, hash: bytesToHex(hash) });
      return;
    }
    nonce = (nonce + job.stride) >>> 0;
    if (nonce === (job.startNonce >>> 0)) {
      parentPort?.postMessage({ type: 'exhausted' });
      return;
    }
    const now = Date.now();
    if (benchmarkEndsAt !== undefined && now >= benchmarkEndsAt) {
      parentPort?.postMessage({ type: 'benchmark', hashes: totalHashes, elapsedMs: now - startedAt });
      return;
    }
    if (now - reportedAt >= REPORT_INTERVAL_MS) {
      parentPort?.postMessage({ type: 'hashrate', hashes, elapsedMs: now - reportedAt });
      hashes = 0;
      reportedAt = now;
    }
  }
}

function writeNonce(header: Uint8Array, nonce: number): void {
  header[NONCE_OFFSET] = (nonce >>> 24) & 0xff;
  header[NONCE_OFFSET + 1] = (nonce >>> 16) & 0xff;
  header[NONCE_OFFSET + 2] = (nonce >>> 8) & 0xff;
  header[NONCE_OFFSET + 3] = nonce & 0xff;
}

void grind(workerData as WorkerJob).catch((error: unknown) => {
  parentPort?.postMessage({ type: 'error', message: error instanceof Error ? error.message : String(error) });
});
