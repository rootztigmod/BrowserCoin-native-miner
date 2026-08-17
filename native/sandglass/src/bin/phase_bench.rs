//! Micro-timing for Sandglass LANES=2: fill-only, walk-only, and full hash_batch.
//!
//! Usage (from native/):
//!   SANDGLASS_HUGEPAGE=1 cargo run -p browsercoin-sandglass --release --bin sandglass-phase-bench -- 50

use browsercoin_sandglass::{SandglassBatch, HEADER_LEN};
use sha2::{Digest, Sha256};
use std::time::Instant;

fn main() {
    let iterations: u32 = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(30);

    let mut header = [0_u8; HEADER_LEN];
    header[2] = 0x8a;
    header[3] = 0xdf; // post-fork height marker used by other benches

    let mut batch = SandglassBatch::<2>::new();
    let mut headers = [[0_u8; HEADER_LEN]; 2];

    // Warmup
    for nonce in 0..4_u32 {
        set_nonces(&mut headers, &header, nonce);
        let _ = batch.hash_batch(&headers);
    }

    let mut fill_ns = 0_u128;
    let mut walk_ns = 0_u128;
    let mut full_ns = 0_u128;
    let mut sink = 0_u64;

    for nonce in 0..iterations {
        set_nonces(&mut headers, &header, nonce);
        let seeds = [
            Sha256::digest(headers[0]).into(),
            Sha256::digest(headers[1]).into(),
        ];

        let start = Instant::now();
        let h = batch.fill_only(&seeds);
        fill_ns += start.elapsed().as_nanos();

        let start = Instant::now();
        let states = batch.walk_only(h);
        walk_ns += start.elapsed().as_nanos();
        sink ^= u64::from(states[0][0]) ^ u64::from(states[1][1]);

        set_nonces(&mut headers, &header, nonce.wrapping_add(1_000_000));
        let start = Instant::now();
        let digests = batch.hash_batch(&headers);
        full_ns += start.elapsed().as_nanos();
        sink ^= u64::from(digests[0][0]) ^ u64::from(digests[1][0]);
    }

    let iters = u128::from(iterations);
    let fill_per = fill_ns / iters;
    let walk_per = walk_ns / iters;
    let full_per = full_ns / iters;
    // Each full hash_batch covers 2 lane-hashes.
    let ns_per_hash = full_per / 2;
    let hashes_per_sec = if ns_per_hash == 0 {
        0
    } else {
        1_000_000_000 / ns_per_hash
    };

    println!(
        "{}",
        serde_json::json!({
            "iterations": iterations,
            "lanes": 2,
            "fillNsPerBatch": fill_per,
            "walkNsPerBatch": walk_per,
            "fullNsPerBatch": full_per,
            "nsPerHash": ns_per_hash,
            "hashesPerSecondSingleThread": hashes_per_sec,
            "fillShare": share(fill_per, fill_per + walk_per),
            "walkShare": share(walk_per, fill_per + walk_per),
            "sink": sink,
        })
    );
}

fn set_nonces(headers: &mut [[u8; HEADER_LEN]; 2], base: &[u8; HEADER_LEN], nonce: u32) {
    headers[0] = *base;
    headers[1] = *base;
    headers[0][112..116].copy_from_slice(&nonce.to_be_bytes());
    headers[1][112..116].copy_from_slice(&(nonce.wrapping_add(1)).to_be_bytes());
}

fn share(part: u128, total: u128) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64) / (total as f64)
    }
}
