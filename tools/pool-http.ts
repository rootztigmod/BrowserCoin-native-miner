import { MINER_VERSION } from './miner-version.js';

export const MINER_UA = `browsercoin-miner/${MINER_VERSION}`;
const POOL_FETCH_TIMEOUT_MS = 15_000;

export type RespClass = 'ok' | 'transient' | 'fatal';

export function classify(status: number): RespClass {
  if (status === 200) return 'ok';
  if (status === 429 || status === 503) return 'transient';
  return 'fatal';
}

export function backoffDelay(
  attempt: number,
  opts: { retryAfterMs?: number; rnd?: () => number } = {},
): number {
  if (opts.retryAfterMs && opts.retryAfterMs > 0) return opts.retryAfterMs;
  const rnd = opts.rnd ?? Math.random;
  const base = Math.min(30_000, 1_000 * 2 ** attempt);
  const jitter = base * 0.2 * (rnd() * 2 - 1);
  return Math.max(0, Math.round(base + jitter));
}

export function parseRetryAfterMs(headers: Headers): number | undefined {
  const ra = headers.get('retry-after') ?? headers.get('ratelimit-reset');
  if (!ra) return undefined;
  const secs = Number(ra);
  return Number.isFinite(secs) && secs >= 0 ? Math.round(secs * 1000) : undefined;
}

export class PoolError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: unknown,
  ) {
    super(`pool request failed: ${status}`);
    this.name = 'PoolError';
  }
}

export async function poolFetch(
  url: string,
  init: RequestInit = {},
  timeoutMs: number = POOL_FETCH_TIMEOUT_MS,
  doFetch: typeof fetch = fetch,
): Promise<{ status: number; body: unknown; headers: Headers }> {
  const headers = new Headers(init.headers);
  headers.set('user-agent', MINER_UA);
  if (init.body && !headers.has('content-type')) headers.set('content-type', 'application/json');

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  const onAbort = (): void => controller.abort();
  if (init.signal) {
    if (init.signal.aborted) controller.abort();
    else init.signal.addEventListener('abort', onAbort, { once: true });
  }
  try {
    const response = await doFetch(url, { ...init, headers, signal: controller.signal });
    const text = await response.text();
    let body: unknown = null;
    if (text) {
      try {
        body = JSON.parse(text);
      } catch {
        body = text;
      }
    }
    return { status: response.status, body, headers: response.headers };
  } catch (error) {
    if ((error as Error)?.name === 'AbortError' && init.signal?.aborted) throw error;
    const timeout = new Error(`pool request timed out after ${timeoutMs}ms`);
    timeout.name = 'TimeoutError';
    throw timeout;
  } finally {
    clearTimeout(timer);
    init.signal?.removeEventListener('abort', onAbort);
  }
}

const defaultSleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

export async function withPoolRetry(
  attempt: () => Promise<{ status: number; body: unknown; headers: Headers }>,
  opts: {
    signal?: AbortSignal;
    onWait?: (attempt: number, delayMs: number) => void;
    sleep?: (ms: number) => Promise<void>;
  } = {},
): Promise<{ status: number; body: unknown }> {
  const sleep = opts.sleep ?? defaultSleep;
  for (let i = 0; ; i++) {
    if (opts.signal?.aborted) throw new DOMException('aborted', 'AbortError');
    let response: { status: number; body: unknown; headers: Headers };
    try {
      response = await attempt();
    } catch (error) {
      if ((error as Error)?.name === 'AbortError') throw error;
      const delay = backoffDelay(i);
      opts.onWait?.(i, delay);
      await sleep(delay);
      continue;
    }
    const cls = classify(response.status);
    if (cls === 'ok') return { status: response.status, body: response.body };
    if (cls === 'fatal') throw new PoolError(response.status, response.body);
    const delay = backoffDelay(i, { retryAfterMs: parseRetryAfterMs(response.headers) });
    opts.onWait?.(i, delay);
    await sleep(delay);
  }
}
