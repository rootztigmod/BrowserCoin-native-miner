import { describe, expect, it } from 'vitest';
import {
  buildJobUrl,
  classifyShare,
  isValidJob,
  isValidSlot,
  jobRestartKey,
  normalizePoolUrl,
  shareOutcome,
} from './pool-client.js';
import { partitionNonceSpace } from './pool-partition.js';

const okJob = {
  jobId: '19033-176',
  headerHex: 'a'.repeat(296),
  shareTargetHex: 'b'.repeat(64),
  nonceStart: 0,
  nonceEnd: 4_194_304,
};

describe('pool job validation', () => {
  it('accepts a well-formed job and rejects malformed fields', () => {
    expect(isValidJob(okJob)).toBe(true);
    expect(isValidJob({ ...okJob, jobId: '' })).toBe(false);
    expect(isValidJob({ ...okJob, headerHex: 'a'.repeat(294) })).toBe(false);
    expect(isValidJob({ ...okJob, shareTargetHex: '0004802c' })).toBe(false);
    expect(isValidJob({ ...okJob, nonceStart: 100, nonceEnd: 100 })).toBe(false);
    expect(isValidJob(null)).toBe(false);
  });

  it('validates nonce slots inside u32 space', () => {
    expect(isValidSlot(0, 1)).toBe(true);
    expect(isValidSlot(0, 0x1_0000_0000)).toBe(true);
    expect(isValidSlot(-1, 10)).toBe(false);
    expect(isValidSlot(10, 10)).toBe(false);
  });
});

describe('share outcomes', () => {
  it('classifies accepted, block-strike, and rejected shares', () => {
    expect(classifyShare({ result: 'accepted' })).toBe('accepted');
    expect(classifyShare({ result: 'accepted', block: true })).toBe('block-strike');
    expect(classifyShare({ result: 'stale' })).toBe('rejected');
  });

  it('retries transient and ambiguous statuses', () => {
    expect(shareOutcome(429, { result: 'rate-limited' })).toBe('retry');
    expect(shareOutcome(503, {})).toBe('retry');
    expect(shareOutcome(408, {})).toBe('retry');
    expect(shareOutcome(500, { result: 'invalid' })).toBe('retry');
    expect(shareOutcome(200, { result: 'stale' })).toBe('rejected');
    expect(shareOutcome(400, { result: 'invalid' })).toBe('rejected');
  });
});

describe('pool helpers', () => {
  it('builds job URLs and restart keys', () => {
    expect(buildJobUrl('https://pool.example', 'w1')).toBe('https://pool.example/job?workerId=w1');
    expect(buildJobUrl('https://pool.example', 'w1', { waitS: 25, have: 'j1' }))
      .toBe('https://pool.example/job?workerId=w1&wait=25&have=j1');
    expect(jobRestartKey(okJob)).toContain(okJob.jobId);
    expect(normalizePoolUrl('pool.fulgurpool.xyz')).toBe('https://pool.fulgurpool.xyz');
    expect(normalizePoolUrl('https://pool.fulgurpool.xyz/')).toBe('https://pool.fulgurpool.xyz');
  });

  it('partitions pool slots across local workers', () => {
    const ranges = partitionNonceSpace(4, 100, 200);
    expect(ranges).toEqual([
      { start: 100, end: 125 },
      { start: 125, end: 150 },
      { start: 150, end: 175 },
      { start: 175, end: 200 },
    ]);
  });
});
