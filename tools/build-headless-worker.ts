import { build } from 'esbuild';

await build({
  bundle: true,
  entryPoints: ['tools/headless-miner.worker.ts'],
  format: 'esm',
  outfile: 'tools/dist/headless-miner.worker.mjs',
  platform: 'node',
  target: 'node22',
});
