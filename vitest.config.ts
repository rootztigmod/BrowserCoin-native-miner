import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['src/**/*.test.ts', 'tools/**/*.test.ts'],
    environment: 'node',
    // Argon2id at the v5 6-bit floor takes ~80–120ms per attempt × ~128
    // expected attempts ≈ 10–15 s per mined block in tests. The
    // sync/fork-choice tests build chains of ~10 blocks, so 5s default
    // would always timeout. 180s leaves comfortable headroom.
    testTimeout: 180_000,
    // Run test files sequentially. Parallel execution causes vitest's
    // worker-IPC heartbeat (`onTaskUpdate`) to time out at 60s when one
    // file is doing CPU-bound Argon2id mining and starves another worker
    // from reporting in. Tests still finish quickly in CI because the
    // sequential cost is bounded — the slow tests are bounded by their
    // own testTimeout.
    fileParallelism: false,
  },
});
