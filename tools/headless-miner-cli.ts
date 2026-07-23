import { runMiner } from './headless-miner.js';

void runMiner().catch((error: unknown) => {
  console.error(`[miner] fatal: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 1;
});
