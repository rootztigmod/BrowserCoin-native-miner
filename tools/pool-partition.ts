export const NONCE_SPACE = 0x1_0000_0000;

export interface NonceRange {
  start: number;
  end: number;
}

/** Split [spaceStart, spaceEnd) into `workers` disjoint contiguous ranges. */
export function partitionNonceSpace(
  workers: number,
  spaceStart = 0,
  spaceEnd = NONCE_SPACE,
): NonceRange[] {
  const n = Math.max(1, Math.floor(workers));
  const lo = Math.max(0, Math.min(Math.floor(spaceStart), NONCE_SPACE));
  const hi = Math.max(lo, Math.min(Math.floor(spaceEnd), NONCE_SPACE));
  const step = Math.floor((hi - lo) / n);
  const ranges: NonceRange[] = [];
  for (let i = 0; i < n; i++) {
    const start = lo + i * step;
    const end = i === n - 1 ? hi : lo + (i + 1) * step;
    ranges.push({ start, end });
  }
  return ranges;
}
