import { createHash } from 'node:crypto';
import { chmod, mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { dirname, join } from 'node:path';

export interface EmbeddedMinerAssets {
  nativeMiner: string;
  jsWorker: string;
}

export interface MaterializedMinerAssets {
  nativeMinerPath: string;
  jsWorkerPath: string;
}

/**
 * Bun exposes embedded assets through a virtual read-only filesystem. The
 * native miner must be a real executable for spawn(), so materialize it in an
 * owner-only cache path whose filename is derived from its content hash.
 */
export async function materializeMinerAssets(assets: EmbeddedMinerAssets): Promise<MaterializedMinerAssets> {
  const cacheHome = process.env.XDG_CACHE_HOME || join(homedir(), '.cache');
  const cacheDir = join(cacheHome, 'browsercoin-miner');
  await mkdir(cacheDir, { recursive: true, mode: 0o700 });
  await chmod(cacheDir, 0o700);

  return {
    nativeMinerPath: await materializeAsset(cacheDir, 'browsercoin-native-miner', assets.nativeMiner, 0o700),
    jsWorkerPath: await materializeAsset(cacheDir, 'headless-miner.worker.mjs', assets.jsWorker, 0o600),
  };
}

async function materializeAsset(cacheDir: string, name: string, assetPath: string, mode: number): Promise<string> {
  const contents = await readFile(assetPath);
  const digest = createHash('sha256').update(contents).digest('hex');
  const destination = join(cacheDir, `${name}-${digest}`);

  try {
    const existing = await readFile(destination);
    if (createHash('sha256').update(existing).digest('hex') === digest) {
      await chmod(destination, mode);
      return destination;
    }
  } catch {
    // The content-addressed file has not been created yet.
  }

  const temporary = join(dirname(destination), `.${name}-${process.pid}-${Date.now()}.tmp`);
  await writeFile(temporary, contents, { mode, flag: 'wx' });
  await chmod(temporary, mode);
  try {
    await rename(temporary, destination);
  } catch (error: unknown) {
    // Another invocation may have completed the identical content-addressed
    // write first. Only accept it after checking its digest.
    const existing = await readFile(destination);
    if (createHash('sha256').update(existing).digest('hex') !== digest) throw error;
  }
  return destination;
}
