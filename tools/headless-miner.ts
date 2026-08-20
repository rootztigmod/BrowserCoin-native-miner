/**
 * Headless BrowserCoin miner for a trusted server you control.
 *
 * It intentionally reuses the project's consensus implementation rather than
 * reimplementing block/state encoding. This keeps it compatible with consensus
 * changes when run from a pinned BrowserCoin checkout.
 *
 * Usage:
 *   browsercoin-miner --address YOUR_64_HEX_WALLET_ADDRESS --workers 4
 *   browsercoin-miner --generate-key /path/to/miner.key
 */
import { readFile, writeFile, chmod, mkdir } from 'node:fs/promises';
import { dirname } from 'node:path';
import { availableParallelism } from 'node:os';
import { Worker } from 'node:worker_threads';

import { computeTxRoot, decodeBlock, encodeBlock, encodeHeader, type Block, type BlockHeader } from '../src/chain/block.js';
import { Blockchain } from '../src/chain/blockchain.js';
import { checkPoW } from '../src/chain/consensus.js';
import { MAX_BLOCK_BYTES, SANDGLASS_FORK_HEIGHT } from '../src/chain/genesis.js';
import { Mempool } from '../src/chain/mempool.js';
import { applyBlockTxs, cloneState, stateRoot } from '../src/chain/state.js';
import { decodeTx, type Transaction } from '../src/chain/transaction.js';
import { addressFromHex, fromPrivateKey, generateKeyPair } from '../src/crypto/keys.js';
import { attemptFastSync, fastSyncEligible } from '../src/net/fastSync.js';
import { compactToTarget, bytesToHex, hexToBytes } from '../src/util/binary.js';
import { NativeMiner, type NativeMinerEvent } from './native-miner.js';
import { MINER_VERSION } from './miner-version.js';
import { normalizePoolUrl, runPoolClient } from './pool-client.js';

const DEFAULT_HELPERS = ['https://api1.browsercoin.org', 'https://api2.browsercoin.org'];
const SYNC_INTERVAL_MS = 10_000;
const REQUEST_TIMEOUT_MS = 12_000;
const MAX_NATIVE_TEMPLATE_AGE_S = 60;
const NONCE_SPACE = 0x1_0000_0000;
export { MINER_VERSION };

export interface MinerRuntime {
  nativeMinerPath?: string;
  jsWorkerPath?: string;
  standalone?: boolean;
}

interface Options {
  helpers: string[];
  address?: string;
  keyFile?: string;
  generateKey?: string;
  workers: number;
  nonceOffset: number;
  nonceStride: number;
  engine: 'native' | 'js';
  pool?: string;
  nonceLaneExplicit: boolean;
}

interface WorkerJob {
  headerHex: string;
  targetHex: string;
  startNonce: number;
  stride: number;
}

interface Candidate {
  block: Block;
  targetHex: string;
}

interface Tip {
  height: number;
  tipHash: string;
  helper: string;
}

export async function runMiner(args = process.argv.slice(2), runtime: MinerRuntime = {}): Promise<void> {
  if (args.length === 1 && args[0] === '--version') {
    console.log(`browsercoin-miner ${MINER_VERSION}${runtime.standalone ? ' (standalone)' : ''}`);
    return;
  }
  const options = parseOptions(args);
  if (options.generateKey) {
    await generateAndStoreKey(options.generateKey);
    return;
  }
  if (!options.address && !options.keyFile) {
    throw new Error('pass --address YOUR_64_HEX_WALLET_ADDRESS (recommended), or --key-file PATH for legacy use');
  }
  if (options.address && options.keyFile) throw new Error('pass either --address or --key-file, not both');

  // Mining needs only the public address written into the coinbase. Retain
  // --key-file solely for existing installations that derive that address
  // locally; never require a private key on a remote mining host.
  const miner = options.address
    ? parseAddress(options.address)
    : fromPrivateKey(await loadPrivateKey(options.keyFile!)).publicKey;
  const addressHex = bytesToHex(miner);
  console.log(`[miner] address=${addressHex}`);

  if (options.pool) {
    if (options.engine !== 'native') {
      throw new Error('pool mode requires --engine native');
    }
    if (options.nonceLaneExplicit) {
      throw new Error('pool mode ignores solo --nonce-offset/--nonce-stride; omit them (the pool assigns the nonce slot)');
    }
    const poolUrl = normalizePoolUrl(options.pool);
    console.log(`[miner] mode=pool; engine=native; workers=${options.workers}; pool=${poolUrl}`);
    const controller = new AbortController();
    const onSignal = (): void => controller.abort();
    process.once('SIGINT', onSignal);
    process.once('SIGTERM', onSignal);
    try {
      await runPoolClient({
        poolUrl,
        payoutAddress: addressHex,
        workers: options.workers,
        nativeMinerPath: runtime.nativeMinerPath,
        signal: controller.signal,
      });
    } catch (error) {
      if ((error as Error)?.name !== 'AbortError') throw error;
    } finally {
      process.off('SIGINT', onSignal);
      process.off('SIGTERM', onSignal);
    }
    return;
  }

  console.log(
    `[miner] mode=solo; engine=${options.engine}; workers=${options.workers}; nonce lanes=${options.nonceOffset}-${options.nonceOffset + options.workers - 1} ` +
    `mod ${options.nonceStride}; helpers=${options.helpers.join(',')}`,
  );

  const chain = new Blockchain();
  const mempool = new Mempool();
  await initialSync(chain, mempool, options.helpers);
  console.log(`[miner] synced height=${chain.height} tip=${bytesToHex(chain.tip.hash)}`);

  let activeWorkers: Worker[] = [];
  let stopping = false;
  let statusTimer: ReturnType<typeof setInterval> | undefined;
  let nativeJobId = 0;
  let nativeJobHandler: ((event: NativeMinerEvent) => void) | undefined;
  const nativeMiner = options.engine === 'native'
    ? new NativeMiner(options.workers, (event) => nativeJobHandler?.(event), runtime.nativeMinerPath)
    : undefined;
  if (nativeMiner) {
    await nativeMiner.start();
    console.log('[miner] native Sandglass worker pool ready');
  } else {
    console.warn('[miner] using explicit JavaScript fallback engine');
  }
  const stopWorkers = async (): Promise<void> => {
    if (statusTimer) {
      clearInterval(statusTimer);
      statusTimer = undefined;
    }
    nativeMiner?.stop();
    const workers = activeWorkers;
    activeWorkers = [];
    await Promise.all(workers.map((worker) => worker.terminate().catch(() => 0)));
  };
  const shutdown = (): void => {
    if (stopping) return;
    stopping = true;
    void stopWorkers().finally(() => nativeMiner?.close().finally(() => process.exit(0)) ?? process.exit(0));
  };
  process.once('SIGINT', shutdown);
  process.once('SIGTERM', shutdown);

  let activeNativeCandidate: Candidate | undefined;
  while (!stopping) {
    const candidate = buildCandidate(chain, mempool, miner);
    if (nativeMiner && candidate.block.header.height < SANDGLASS_FORK_HEIGHT) {
      throw new Error(`native engine supports Sandglass blocks at height ${SANDGLASS_FORK_HEIGHT} and above; pass --engine js for historical mining`);
    }
    if (nativeMiner && activeNativeCandidate && canRetainNativeTemplate(activeNativeCandidate, candidate)) {
      const age = Math.floor(Date.now() / 1_000) - activeNativeCandidate.block.header.timestamp;
      if (age < MAX_NATIVE_TEMPLATE_AGE_S) {
        console.log(`[miner] retained template height=${activeNativeCandidate.block.header.height}; age=${age}s`);
        await sleep(SYNC_INTERVAL_MS);
        await syncSoft(chain, mempool, options.helpers);
        continue;
      }
      console.log(`[miner] rebuilding template: timestamp age=${age}s`);
    }
    activeNativeCandidate = nativeMiner ? candidate : undefined;
    let solved = false;
    const workerRates = new Map<number, number>();
    await stopWorkers();
    const finishSolved = async (nonce: number): Promise<void> => {
      if (solved || stopping) return;
      solved = true;
      activeNativeCandidate = undefined;
      await stopWorkers();
      const block: Block = {
        header: { ...candidate.block.header, nonce },
        transactions: candidate.block.transactions,
      };
      const validationError = await chain.addBlock(block);
      if (validationError) {
        console.warn(`[miner] discarded local solution: ${validationError}`);
        return;
      }
      const responses = await submitBlock(block, options.helpers);
      logSolved(`[miner] solved height=${block.header.height}; ${responses.join('; ')}`);
    };

    if (nativeMiner) {
      const jobId = ++nativeJobId;
      nativeJobHandler = (event) => {
        if (event.type === 'error') {
          console.error(`[miner] native worker error: ${event.message}`);
          return;
        }
        if (event.type === 'ready' || event.jobId !== jobId) return;
        if (event.type === 'hashrate') {
          workerRates.set(event.worker, (event.hashes * 1_000) / Math.max(1, event.elapsedMs));
          return;
        }
        if (event.type === 'exhausted' && !solved && !stopping) {
          console.log('[miner] nonce space exhausted; rebuilding template');
          solved = true;
          activeNativeCandidate = undefined;
          nativeMiner.stop();
          return;
        }
        if (event.type === 'solved') void finishSolved(event.nonce);
      };
      nativeMiner.startJob({
        jobId,
        headerHex: bytesToHex(encodeHeader(candidate.block.header)),
        targetHex: candidate.targetHex,
        nonceOffset: options.nonceOffset,
        nonceStride: options.nonceStride,
      });
    } else {
      activeWorkers = Array.from({ length: options.workers }, (_, index) => {
        const worker = new Worker(runtime.jsWorkerPath ?? new URL('./dist/headless-miner.worker.mjs', import.meta.url), {
          workerData: {
            headerHex: bytesToHex(encodeHeader(candidate.block.header)),
            targetHex: candidate.targetHex,
            startNonce: (options.nonceOffset + index) >>> 0,
            stride: options.nonceStride,
          } satisfies WorkerJob,
        });
        worker.on('message', async (message: { type: string; nonce?: number; hashes?: number; elapsedMs?: number }) => {
          if (message.type === 'hashrate') {
            workerRates.set(index, ((message.hashes ?? 0) * 1_000) / Math.max(1, message.elapsedMs ?? 1));
            return;
          }
          if (message.type === 'exhausted' && !solved && !stopping) {
            console.log('[miner] nonce space exhausted; rebuilding template');
            solved = true;
            await stopWorkers();
            return;
          }
          if (message.type === 'solved' && message.nonce !== undefined) void finishSolved(message.nonce);
        });
        worker.on('error', (error) => console.error(`[miner] worker error: ${error.message}`));
        return worker;
      });
    }
    statusTimer = setInterval(() => {
      const hashrate = [...workerRates.values()].reduce((total, rate) => total + rate, 0);
      console.log(`[miner] ${Math.round(hashrate)} H/s height=${candidate.block.header.height}`);
    }, 1_000);

    await sleep(SYNC_INTERVAL_MS);
    if (!solved) await syncSoft(chain, mempool, options.helpers);
  }
  await stopWorkers();
  await nativeMiner?.close();
}

function buildCandidate(chain: Blockchain, mempool: Mempool, miner: Uint8Array): Candidate {
  const parent = chain.tip.block.header;
  const height = parent.height + 1;
  const timestamp = Math.floor(Date.now() / 1_000);
  const difficulty = chain.expectedNextDifficulty(timestamp);
  const scriptCtx = chain.nextBlockScriptContext();
  let txs = mempool.selectForBlock(chain.tipState, MAX_BLOCK_BYTES - 1_024, { ...scriptCtx, blockHeight: height });
  let state = cloneState(chain.tipState);
  let applyError = applyBlockTxs(state, height, miner, txs, scriptCtx);
  if (applyError) {
    console.warn(`[miner] template transaction fallback: ${applyError}`);
    txs = [];
    state = cloneState(chain.tipState);
    applyError = applyBlockTxs(state, height, miner, txs, scriptCtx);
    if (applyError) throw new Error(`empty template rejected: ${applyError}`);
  }
  const header: BlockHeader = {
    height,
    prevHash: chain.tip.hash,
    txRoot: computeTxRoot(txs),
    stateRoot: stateRoot(state),
    timestamp,
    difficulty,
    nonce: 0,
    miner,
  };
  return { block: { header, transactions: txs }, targetHex: compactToTarget(difficulty).toString(16).padStart(64, '0') };
}

function canRetainNativeTemplate(current: Candidate, next: Candidate): boolean {
  const currentHeader = current.block.header;
  const nextHeader = next.block.header;
  return currentHeader.height === nextHeader.height
    && currentHeader.difficulty === nextHeader.difficulty
    && bytesToHex(currentHeader.prevHash) === bytesToHex(nextHeader.prevHash)
    && bytesToHex(currentHeader.txRoot) === bytesToHex(nextHeader.txRoot)
    && bytesToHex(currentHeader.stateRoot) === bytesToHex(nextHeader.stateRoot);
}

async function initialSync(chain: Blockchain, mempool: Mempool, helpers: string[]): Promise<void> {
  const tip = await getBestTip(helpers);
  if (fastSyncEligible(chain.height, tip.height)) {
    console.log(`[miner] fast-syncing headers and snapshot toward height=${tip.height}`);
    const result = await attemptFastSync({
      chain,
      servers: helpers,
      fetchImpl: (url) => request(url),
      verifier: {
        // The sampled set is intentionally small (224 headers). Mining workers
        // are not started until this completes, so sequential verification
        // avoids competing with a full worker pool during boot.
        verifyAll: async (blocks) => Promise.all(blocks.map((block) => checkPoW(block.header))),
      },
      onProgress: (progress) => {
        if (progress.phase === 'headers' && progress.done % 4_000 === 0) {
          console.log(`[miner] fast-sync headers ${progress.done}/${progress.total}`);
        }
      },
    }, tip);
    console.log(`[miner] fast-sync ${result.status}${result.status === 'failed' ? `: ${result.reason}` : ''}`);
  }
  await sync(chain, mempool, helpers);
}

async function syncSoft(chain: Blockchain, mempool: Mempool, helpers: string[]): Promise<void> {
  try {
    await sync(chain, mempool, helpers);
    console.log(`[miner] refreshed height=${chain.height} tip=${bytesToHex(chain.tip.hash)}`);
  } catch (error) {
    // Helper blips must not kill the mining loop: an uncaught throw used to print
    // "fatal" while leaving native workers + the status timer alive at a stale height.
    console.warn(
      `[miner] sync deferred (helpers unreachable): ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

async function sync(chain: Blockchain, mempool: Mempool, helpers: string[]): Promise<void> {
  const tip = await getBestTip(helpers);

  // Contiguous pull only. Never treat lookback duplicates as "progress" that
  // lets us jump past a rejected height — that permanently stalls tip advance
  // while the miner keeps grinding the same stale template.
  let cursor = Math.max(0, chain.height + 1 - 5);
  while (cursor <= tip.height) {
    const batch = await blocksFromTipHelper(tip, helpers, cursor);
    if (batch.blocks.length === 0) break;

    const cursorBefore = cursor;
    let nextExpected = cursor;
    let backoff = false;
    let hardReject = false;

    for (const encoded of batch.blocks) {
      let block: Block;
      try {
        block = decodeBlockHex(encoded);
      } catch (error) {
        console.warn(`[miner] ignored malformed block near height ${nextExpected}: ${String(error)}`);
        hardReject = true;
        break;
      }

      if (block.header.height < nextExpected) continue;
      if (block.header.height > nextExpected) {
        console.warn(
          `[miner] helper block gap at height ${nextExpected} (got ${block.header.height}); stopping sync round`,
        );
        hardReject = true;
        break;
      }

      const error = await chain.addBlock(block);
      if (error === null) {
        nextExpected = block.header.height + 1;
        continue;
      }
      if (error === 'parent block unknown') {
        backoff = true;
        break;
      }
      console.warn(`[miner] ignored block ${block.header.height}: ${error}`);
      // Consensus reject of the next needed block: retry next tick, do not skip.
      hardReject = true;
      break;
    }

    if (backoff) {
      cursor = Math.max(0, cursorBefore - 200);
      continue;
    }
    if (hardReject) break;
    if (nextExpected <= cursorBefore) break;
    cursor = nextExpected;
  }

  try {
    const pending = await firstJson<{ txs: string[] }>(helpers, '/mempool');
    mempool.clear();
    for (const encoded of pending.txs) {
      try {
        const { tx, next } = decodeTx(hexToBytes(encoded));
        if (next === encoded.length / 2) mempool.add(tx, chain.tipState);
      } catch {
        // Untrusted helper data is ignored; only locally validated transactions
        // are admitted to the template.
      }
    }
  } catch (error) {
    console.warn(
      `[miner] mempool refresh skipped: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

async function blocksFromTipHelper(
  tip: Tip,
  helpers: string[],
  cursor: number,
): Promise<{ blocks: string[] }> {
  const path = `/blocks?fromHeight=${cursor}&max=200`;
  try {
    return await getJson<{ blocks: string[] }>(`${tip.helper}${path}`);
  } catch {
    return firstJson<{ blocks: string[] }>(helpers, path);
  }
}

async function getBestTip(helpers: string[]): Promise<Tip> {
  const tips = await Promise.allSettled(helpers.map(async (helper) => {
    const tip = await getJson<{ height: number; tipHash: string }>(`${helper}/tip`);
    return { height: tip.height, tipHash: tip.tipHash, helper };
  }));
  const tip = tips
    .filter((result): result is PromiseFulfilledResult<Tip> => result.status === 'fulfilled')
    .map((result) => result.value)
    .sort((a, b) => b.height - a.height)[0];
  if (!tip) throw new Error('no BrowserCoin helper responded to /tip');
  return tip;
}

function decodeBlockHex(encoded: string): Block {
  return decodeBlock(hexToBytes(encoded));
}

async function submitBlock(block: Block, helpers: string[]): Promise<string[]> {
  const body = JSON.stringify({ block: bytesToHex(encodeBlock(block)) });
  const results = await Promise.allSettled(helpers.map(async (helper) => {
    const response = await request(`${helper}/block`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body,
    });
    return `${helper}: ${response.ok ? JSON.stringify(await response.json()) : `HTTP ${response.status}`}`;
  }));
  return results.map((result) => result.status === 'fulfilled' ? result.value : `submit failed: ${String(result.reason)}`);
}

async function firstJson<T>(helpers: string[], path: string): Promise<T> {
  const results = await Promise.allSettled(helpers.map((helper) => getJson<T>(`${helper}${path}`)));
  for (const result of results) {
    if (result.status === 'fulfilled') return result.value;
  }
  throw new Error(`all helpers failed for ${path}`);
}

async function getJson<T>(url: string): Promise<T> {
  const response = await request(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  return response.json() as Promise<T>;
}

async function request(url: string, init?: RequestInit): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    return await fetch(url, { ...init, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

function parseOptions(args: string[]): Options {
  const workers = Math.max(1, availableParallelism());
  let nonceStrideExplicit = false;
  let nonceOffsetExplicit = false;
  const options: Options = {
    helpers: [...DEFAULT_HELPERS],
    workers,
    nonceOffset: 0,
    nonceStride: workers,
    engine: 'native',
    nonceLaneExplicit: false,
  };
  for (let index = 0; index < args.length; index++) {
    const value = args[index];
    const next = args[index + 1];
    if (value === '--address' && next) { options.address = next; index++; }
    else if (value === '--key-file' && next) { options.keyFile = next; index++; }
    else if (value === '--generate-key' && next) { options.generateKey = next; index++; }
    else if (value === '--workers' && next) { options.workers = positiveInt(next, '--workers'); index++; }
    else if (value === '--engine' && next && (next === 'native' || next === 'js')) { options.engine = next; index++; }
    else if (value === '--pool' && next) { options.pool = next; index++; }
    else if (value === '--nonce-offset' && next) {
      options.nonceOffset = nonceInteger(next, '--nonce-offset', true);
      nonceOffsetExplicit = true;
      index++;
    }
    else if (value === '--nonce-stride' && next) {
      options.nonceStride = nonceInteger(next, '--nonce-stride');
      nonceStrideExplicit = true;
      index++;
    }
    else if (value === '--helper' && next) { options.helpers.push(next.replace(/\/$/, '')); index++; }
    else if (value === '--help') {
      console.log('Usage: browsercoin-miner --address ADDRESS [--pool URL] [--engine native|js] [--workers N] [--nonce-offset N] [--nonce-stride N] [--helper URL]');
      console.log('       browsercoin-miner --pool https://pool.fulgurpool.xyz --address ADDRESS --workers 16');
      console.log('       browsercoin-miner --key-file PATH [options]  # legacy; exposes a private key to this host');
      console.log('       browsercoin-miner --generate-key PATH');
      process.exit(0);
    } else throw new Error(`unknown or incomplete option: ${value}`);
  }
  if (options.helpers.length > DEFAULT_HELPERS.length) options.helpers = options.helpers.slice(DEFAULT_HELPERS.length);
  if (!nonceStrideExplicit) options.nonceStride = options.workers;
  options.nonceLaneExplicit = nonceOffsetExplicit || nonceStrideExplicit;
  if (!options.pool) {
    if (options.nonceStride < options.workers) {
      throw new Error('--nonce-stride must be at least --workers');
    }
    if (options.nonceOffset + options.workers > options.nonceStride) {
      throw new Error('--nonce-offset plus --workers must not exceed --nonce-stride');
    }
  }
  return options;
}

async function generateAndStoreKey(path: string): Promise<void> {
  const pair = generateKeyPair();
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  await writeFile(path, `${bytesToHex(pair.privateKey)}\n`, { mode: 0o600, flag: 'wx' });
  await chmod(path, 0o600);
  console.log(`[miner] generated key at ${path}`);
  console.log(`[miner] address=${pair.address}`);
  console.log('[miner] back up the key file securely; anyone with it controls mined funds.');
}

async function loadPrivateKey(path: string): Promise<Uint8Array> {
  const encoded = (await readFile(path, 'utf8')).trim();
  if (!/^[0-9a-f]{64}$/i.test(encoded)) throw new Error('key file must contain exactly 32 bytes as 64 hex characters');
  return hexToBytes(encoded);
}

function parseAddress(value: string): Uint8Array {
  if (!/^[0-9a-f]{64}$/i.test(value)) {
    throw new Error('--address must be exactly 32 bytes as 64 hexadecimal characters');
  }
  return addressFromHex(value);
}

function positiveInt(value: string, name: string): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1 || parsed > NONCE_SPACE) {
    throw new Error(`${name} must be an integer between 1 and ${NONCE_SPACE}`);
  }
  return parsed;
}

function nonceInteger(value: string, name: string, allowZero = false): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < (allowZero ? 0 : 1) || parsed > NONCE_SPACE) {
    throw new Error(`${name} must be an integer between ${allowZero ? 0 : 1} and 4294967296`);
  }
  return parsed;
}

function logSolved(message: string): void {
  // Keep journal output clean when launched by systemd, while making a locally
  // launched miner's successful solve conspicuous in an interactive terminal.
  if (process.stdout.isTTY) console.log(`\x1b[1;32m${message}\x1b[0m`);
  else console.log(message);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
