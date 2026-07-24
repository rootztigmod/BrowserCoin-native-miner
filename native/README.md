# Native Sandglass Miner

The native process replaces only the post-fork Sandglass nonce-grinding loop.
`tools/headless-miner.ts` remains responsible for chain synchronization, Fork-3
difficulty calculation, template construction, local validation, and block
submission.

Build it on each Linux mining host:

```bash
npm run build:native-miner
```

This uses `RUSTFLAGS='-C target-cpu=native'`; do not copy the resulting binary
to a CPU with a different instruction-set profile. The normal headless command
uses the native engine by default:

```bash
npm run mine:headless -- --key-file /etc/browsercoin/miner.key --workers 32
```

`SANDGLASS_LANES` controls how many independent Sandglass hashes each worker
interleaves (memory-level parallelism). Supported values: `1`, `2` (default),
or `4`. On a Core Ultra 9 275HX, `2` was ~25% faster than `1`, while `4`
regressed. Override only after benchmarking the target host.

On Linux, set `SANDGLASS_HUGEPAGE=1` to allocate each worker's scratch mapping
with transparent-huge-page advice. This is vector-equivalent and should be
enabled only after benchmarking the target host. `SANDGLASS_PREFETCH=1` and
`SANDGLASS_MODE=avx2` are experimental A/B modes; they are disabled by default
because they may reduce throughput on a given CPU.

Use the older worker-thread implementation only explicitly:

```bash
npm run mine:headless -- --engine js --key-file /etc/browsercoin/miner.key --workers 32
```

Validation and measurement:

```bash
npm run test:native-miner
npm run benchmark:native -- 32 30000
npm run benchmark:headless -- 32 30000
```

The native process accepts JSON-lines commands from the TypeScript controller
and keeps one 512 KiB Sandglass buffer per persistent CPU thread. It never
constructs or submits blocks itself.
