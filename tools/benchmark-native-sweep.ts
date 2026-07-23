import { execFile } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const run = promisify(execFile);
const workers = (process.argv[2] ?? '16,20,24,28,32').split(',').map((value) => positive(value, 'workers'));
const durationMs = positive(process.argv[3] ?? '30000', 'durationMs');
const repeats = positive(process.argv[4] ?? '3', 'repeats');
const tsx = fileURLToPath(new URL('../node_modules/.bin/tsx', import.meta.url));

for (const workerCount of workers) {
  const results: number[] = [];
  for (let runIndex = 0; runIndex < repeats; runIndex++) {
    const { stdout } = await run(tsx, ['tools/benchmark-native.ts', String(workerCount), String(durationMs)]);
    const line = stdout.trim().split('\n').at(-1);
    if (!line) throw new Error(`native benchmark produced no result for ${workerCount} workers`);
    const result = JSON.parse(line) as { hashesPerSecond?: number; workerErrors?: number };
    if (!Number.isFinite(result.hashesPerSecond) || result.workerErrors !== 0) {
      throw new Error(`native benchmark failed for ${workerCount} workers: ${line}`);
    }
    results.push(result.hashesPerSecond!);
  }
  const sorted = [...results].sort((left, right) => left - right);
  console.log(JSON.stringify({
    workers: workerCount,
    durationMs,
    repeats,
    hashesPerSecond: results,
    medianHashesPerSecond: sorted[Math.floor(sorted.length / 2)]!,
  }));
}

function positive(value: string, name: string): number {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 1) throw new Error(`${name} must be a positive integer`);
  return parsed;
}
