import { MINER_VERSION } from './miner-version.js';
import { NativeMiner, type NativeMinerEvent } from './native-miner.js';
import {
  PoolError,
  backoffDelay,
  classify,
  parseRetryAfterMs,
  poolFetch,
  withPoolRetry,
} from './pool-http.js';
import { NONCE_SPACE } from './pool-partition.js';

export interface PoolJob {
  jobId: string;
  headerHex: string;
  shareTargetHex: string;
  nonceStart: number;
  nonceEnd: number;
}

export type ShareVerdict = 'accepted' | 'block-strike' | 'rejected';

const HEX_RE = /^[0-9a-fA-F]+$/;
const isHexOfLen = (value: unknown, length: number): boolean =>
  typeof value === 'string' && value.length === length && HEX_RE.test(value);

export function classifyShare(body: { result?: string; block?: boolean } | null | undefined): ShareVerdict {
  if (!body || body.result !== 'accepted') return 'rejected';
  return body.block === true ? 'block-strike' : 'accepted';
}

export function shareOutcome(
  status: number,
  body: { result?: string; block?: boolean } | null | undefined,
): 'retry' | ShareVerdict {
  if (classify(status) === 'transient') return 'retry';
  if (status === 408 || status === 425 || (status >= 500 && status <= 599)) return 'retry';
  return classifyShare(body);
}

export function isValidSlot(nonceStart: unknown, nonceEnd: unknown): boolean {
  return Number.isInteger(nonceStart)
    && Number.isInteger(nonceEnd)
    && (nonceStart as number) >= 0
    && (nonceStart as number) < (nonceEnd as number)
    && (nonceEnd as number) <= NONCE_SPACE;
}

export function isValidJob(job: unknown): job is PoolJob {
  if (typeof job !== 'object' || job === null) return false;
  const value = job as Record<string, unknown>;
  return typeof value.jobId === 'string'
    && value.jobId.length > 0
    && isHexOfLen(value.headerHex, 296)
    && isHexOfLen(value.shareTargetHex, 64)
    && isValidSlot(value.nonceStart, value.nonceEnd);
}

export function jobRestartKey(job: PoolJob): string {
  return `${job.jobId}|${job.shareTargetHex}|${job.headerHex}|${job.nonceStart}|${job.nonceEnd}`;
}

export function normalizePoolUrl(url: string): string {
  const trimmed = url.trim().replace(/\/$/, '');
  if (!trimmed) throw new Error('--pool URL is required');
  if (/^https?:\/\//i.test(trimmed)) return trimmed;
  return `https://${trimmed}`;
}

export function buildJobUrl(
  poolUrl: string,
  workerId: string,
  opts: { waitS?: number; have?: string | null } = {},
): string {
  const params = new URLSearchParams({ workerId });
  if (opts.waitS && opts.waitS > 0 && opts.have) {
    params.set('wait', String(opts.waitS));
    params.set('have', opts.have);
  }
  return `${poolUrl}/job?${params.toString()}`;
}

export interface PoolClientOptions {
  poolUrl: string;
  payoutAddress: string;
  workers: number;
  nativeMinerPath?: string;
  signal?: AbortSignal;
  jobWaitS?: number;
  jobPollMs?: number;
}

export async function runPoolClient(options: PoolClientOptions): Promise<void> {
  const poolUrl = normalizePoolUrl(options.poolUrl);
  const waitS = clamp(options.jobWaitS ?? 25, 0, 30);
  const jobPollMs = clamp(options.jobPollMs ?? 1000, 250, 60_000);
  let stopping = false;
  const onAbort = (): void => {
    stopping = true;
  };
  options.signal?.addEventListener('abort', onAbort, { once: true });

  console.log(`[pool-miner] connecting to ${poolUrl}…`);
  const registration = await withPoolRetry(
    () => poolFetch(`${poolUrl}/register`, {
      method: 'POST',
      body: JSON.stringify({ payoutAddress: options.payoutAddress, minerVersion: MINER_VERSION }),
      signal: options.signal,
    }),
    {
      signal: options.signal,
      onWait: () => console.log(`[pool-miner] ${poolUrl} unavailable — retrying…`),
    },
  ).catch((error: unknown) => {
    if (error instanceof PoolError && error.status === 410) {
      throw new Error('this pool requires negotiated WebSocket mode, which is not supported yet');
    }
    if (error instanceof PoolError && error.status === 400) {
      throw new Error(`registration rejected: ${JSON.stringify(error.body)}`);
    }
    if (error instanceof PoolError && error.status === 426) {
      throw new Error('miner upgrade required by pool');
    }
    throw error;
  });

  let workerId = String((registration.body as { workerId?: string }).workerId ?? '');
  if (!workerId) throw new Error('pool /register did not return workerId');
  console.log(`[pool-miner] registered worker ${workerId}`);

  let epoch = 0;
  let activePoolJobId: string | null = null;
  let activeNativeJobId = 0;
  let lastRestartKey: string | null = null;
  let nativeJobCounter = 0;
  const nativeToPoolJob = new Map<number, { poolJobId: string; workerId: string; epoch: number }>();
  const submitted = new Set<string>();
  let hashes = 0;
  let reportedAt = Date.now();

  const nativeMiner = new NativeMiner(options.workers, onNativeEvent, options.nativeMinerPath);
  await nativeMiner.start();
  console.log('[pool-miner] native Sandglass worker pool ready');

  const stop = async (): Promise<void> => {
    stopping = true;
    nativeMiner.stop();
    await nativeMiner.close();
  };
  options.signal?.addEventListener('abort', () => {
    void stop();
  }, { once: true });

  function onNativeEvent(event: NativeMinerEvent): void {
    if (event.type === 'hashrate') {
      hashes += event.hashes;
      const elapsed = Date.now() - reportedAt;
      if (elapsed >= 2000) {
        const rate = Math.round((hashes * 1000) / elapsed);
        console.log(`[pool-miner] ${rate} H/s job=${activePoolJobId ?? '-'}`);
        hashes = 0;
        reportedAt = Date.now();
      }
      return;
    }
    if (event.type === 'error') {
      console.error(`[pool-miner] native error: ${event.message}`);
      return;
    }
    if (event.type === 'exhausted') {
      console.log(`[pool-miner] nonce slot exhausted for native job ${event.jobId}`);
      return;
    }
    if (event.type !== 'solved') return;
    const mapped = nativeToPoolJob.get(event.jobId);
    if (!mapped || mapped.epoch !== epoch || mapped.poolJobId !== activePoolJobId) return;
    const dedupeKey = `${mapped.workerId}|${mapped.poolJobId}|${event.nonce}`;
    if (submitted.has(dedupeKey)) return;
    submitted.add(dedupeKey);
    void submitShare(mapped.workerId, mapped.poolJobId, mapped.epoch, event.nonce);
  }

  async function submitShare(
    shareWorkerId: string,
    jobId: string,
    shareEpoch: number,
    nonce: number,
  ): Promise<void> {
    const deadline = Date.now() + 120_000;
    for (let attempt = 0; ; attempt++) {
      if (stopping || shareEpoch !== epoch || activePoolJobId !== jobId) return;
      if (Date.now() >= deadline) {
        console.warn('[pool-miner] share dropped after prolonged pool outage');
        return;
      }
      try {
        const response = await poolFetch(`${poolUrl}/share`, {
          method: 'POST',
          body: JSON.stringify({ workerId: shareWorkerId, jobId, nonce }),
          signal: options.signal,
        });
        const body = (response.body ?? {}) as { result?: string; block?: boolean };
        const outcome = shareOutcome(response.status, body);
        if (outcome === 'retry') {
          await sleep(Math.min(backoffDelay(attempt, {
            retryAfterMs: parseRetryAfterMs(response.headers),
          }), deadline - Date.now()));
          continue;
        }
        if (outcome === 'accepted') {
          console.log(`[pool-miner] share accepted: nonce=${nonce} job=${jobId}`);
        } else if (outcome === 'block-strike') {
          console.log(`\x1b[1;32m[pool-miner] BLOCK STRIKE: nonce=${nonce} job=${jobId}\x1b[0m`);
        } else if (body.result === 'stale') {
          console.log(`[pool-miner] share stale: job=${jobId}`);
          activePoolJobId = null;
          lastRestartKey = null;
          nativeMiner.stop();
        } else {
          console.log(`[pool-miner] share rejected: ${body.result ?? response.status}`);
        }
        return;
      } catch (error) {
        if ((error as Error)?.name === 'AbortError' || stopping) return;
        await sleep(Math.min(backoffDelay(attempt), deadline - Date.now()));
      }
    }
  }

  async function reregister(): Promise<void> {
    const again = await withPoolRetry(
      () => poolFetch(`${poolUrl}/register`, {
        method: 'POST',
        body: JSON.stringify({ payoutAddress: options.payoutAddress, minerVersion: MINER_VERSION }),
        signal: options.signal,
      }),
      { signal: options.signal },
    );
    workerId = String((again.body as { workerId?: string }).workerId ?? '');
    if (!workerId) throw new Error('pool re-register did not return workerId');
    epoch += 1;
    activePoolJobId = null;
    lastRestartKey = null;
    submitted.clear();
    nativeMiner.stop();
    console.log(`[pool-miner] re-registered worker ${workerId}`);
  }

  async function refresh(have: string | null, waitSValue: number): Promise<'ok' | 'retry' | 'reregister'> {
    const url = buildJobUrl(poolUrl, workerId, { waitS: waitSValue, have });
    const timeoutMs = waitSValue > 0 ? waitSValue * 1000 + 10_000 : 15_000;
    let response: { status: number; body: unknown };
    try {
      response = await poolFetch(url, { signal: options.signal }, timeoutMs);
    } catch (error) {
      if ((error as Error)?.name === 'AbortError') throw error;
      return 'retry';
    }
    if (response.status === 404) {
      await reregister();
      return 'reregister';
    }
    if (classify(response.status) === 'transient') return 'retry';
    if (response.status !== 200 || !isValidJob(response.body)) return 'retry';

    const job = response.body;
    const key = jobRestartKey(job);
    if (key === lastRestartKey) return 'ok';

    nativeJobCounter += 1;
    activeNativeJobId = nativeJobCounter;
    activePoolJobId = job.jobId;
    lastRestartKey = key;
    nativeToPoolJob.set(activeNativeJobId, { poolJobId: job.jobId, workerId, epoch });
    submitted.clear();

    // Pool owns the slot. Local workers stride through [nonceStart, nonceEnd).
    nativeMiner.startJob({
      jobId: activeNativeJobId,
      headerHex: job.headerHex,
      targetHex: job.shareTargetHex,
      nonceOffset: job.nonceStart,
      nonceStride: options.workers,
      nonceEnd: job.nonceEnd,
      continuous: true,
    });
    console.log(
      `[pool-miner] job=${job.jobId} slot=[${job.nonceStart},${job.nonceEnd}) workers=${options.workers}`,
    );
    return 'ok';
  }

  try {
    while (!stopping && !options.signal?.aborted) {
      const have: string | null = activePoolJobId;
      const usedWait = have ? waitS : 0;
      const result = await refresh(have, usedWait);
      if (stopping || options.signal?.aborted) break;
      if (result === 'reregister') continue;
      if (result === 'retry') await sleep(jobPollMs);
      // When long-poll returns the same job quickly, pause before the next poll.
      else if (have && activePoolJobId === have) await sleep(jobPollMs);
    }
  } finally {
    await stop();
    options.signal?.removeEventListener('abort', onAbort);
  }
}

function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value)) return min;
  return Math.min(max, Math.max(min, Math.floor(value)));
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, Math.max(0, ms)));
}
