import vectors from '../src/crypto/sandglass.vectors.json' with { type: 'json' };
import { sandglassHash } from '../src/crypto/sandglass.js';
import { bytesToHex, hexToBytes } from '../src/util/binary.js';
import { NativeMiner, type NativeMinerEvent } from './native-miner.js';

type Solution = Extract<NativeMinerEvent, { type: 'solved' }>;
let resolveSolution: ((event: Solution) => void) | undefined;
const miner = new NativeMiner(1, (event) => {
  if (event.type === 'solved') resolveSolution?.(event);
  if (event.type === 'error') throw new Error(event.message);
});

await miner.start();
await verifyVector(vectors[0]!, 1);
await verifyVector(vectors[1]!, 2);
await miner.close();

async function verifyVector(vector: typeof vectors[number], jobId: number): Promise<void> {
  const header = hexToBytes(vector.headerHex);
  const nonce = ((header[112]! << 24) | (header[113]! << 16) | (header[114]! << 8) | header[115]!) >>> 0;
  const expected = bytesToHex(sandglassHash(header));
  if (expected !== vector.digestHex) throw new Error('TypeScript Sandglass vector mismatch');
  const solution = new Promise<Solution>((resolve) => { resolveSolution = resolve; });
  miner.startJob({
    jobId,
    headerHex: vector.headerHex,
    targetHex: 'f'.repeat(64),
    nonceOffset: nonce,
    nonceStride: 1,
  });
  const result = await Promise.race([
    solution,
    new Promise<never>((_, reject) => setTimeout(() => reject(new Error('native hash timed out')), 10_000)),
  ]);
  if (result.nonce !== nonce || result.hash !== expected) {
    throw new Error(`native digest mismatch: expected ${expected}, got ${result.hash}`);
  }
  console.log(`native Sandglass digest matches TypeScript: ${result.hash}`);
}
