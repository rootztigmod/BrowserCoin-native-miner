import nativeMiner from './standalone-assets/browsercoin-native-miner.asset' with { type: 'file' };
import jsWorker from './standalone-assets/headless-miner.worker.asset' with { type: 'file' };
import { runMiner } from './headless-miner.js';
import { materializeMinerAssets } from './standalone-runtime.js';

void (async () => {
  const args = process.argv.slice(2);
  if (args.length === 1 && (args[0] === '--help' || args[0] === '--version')) {
    await runMiner(args, { standalone: true });
    return;
  }
  const assets = await materializeMinerAssets({
    nativeMiner,
    jsWorker,
  });
  await runMiner(args, {
    ...assets,
    standalone: true,
  });
})().catch((error: unknown) => {
  console.error(`[miner] fatal: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
});
