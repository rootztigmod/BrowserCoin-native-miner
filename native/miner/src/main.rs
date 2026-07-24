use browsercoin_sandglass::{HEADER_LEN, Sandglass, SandglassBatch, hash_meets_target};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, BufRead, Write};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU64, Ordering},
    mpsc,
};
use std::thread;
use std::time::Instant;

const NONCE_OFFSET: usize = 112;
const CHECK_INTERVAL: usize = 64;
const REPORT_INTERVAL_MS: u128 = 2_000;

#[derive(Clone)]
struct Job {
    id: u64,
    header: [u8; HEADER_LEN],
    target: [u8; 32],
    nonce_offset: u32,
    nonce_stride: u32,
}

struct JobState {
    job: Option<Job>,
    shutdown: bool,
}

type SharedState = Arc<(Mutex<JobState>, Condvar, AtomicU64)>;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Command {
    Start {
        #[serde(rename = "jobId")]
        job_id: u64,
        #[serde(rename = "headerHex")]
        header_hex: String,
        #[serde(rename = "targetHex")]
        target_hex: String,
        #[serde(rename = "nonceOffset")]
        nonce_offset: u32,
        #[serde(rename = "nonceStride")]
        nonce_stride: u32,
    },
    Stop,
    Shutdown,
}

enum Event {
    Hashrate {
        worker: usize,
        job_id: u64,
        hashes: u64,
        elapsed_ms: u128,
    },
    Solved {
        worker: usize,
        job_id: u64,
        nonce: u32,
        hash: [u8; 32],
    },
    Exhausted {
        worker: usize,
        job_id: u64,
    },
}

enum LaneHasher {
    L1(Sandglass),
    L2(SandglassBatch<2>),
    L4(SandglassBatch<4>),
}

impl LaneHasher {
    fn new(lanes: usize) -> Self {
        match lanes {
            1 => Self::L1(Sandglass::new()),
            2 => Self::L2(SandglassBatch::<2>::new()),
            4 => Self::L4(SandglassBatch::<4>::new()),
            _ => unreachable!("parse_lanes only returns 1, 2, or 4"),
        }
    }

    fn lanes(&self) -> usize {
        match self {
            Self::L1(_) => 1,
            Self::L2(_) => 2,
            Self::L4(_) => 4,
        }
    }
}

fn main() {
    let workers = parse_workers();
    let lanes = parse_lanes();
    let output = Arc::new(Mutex::new(io::stdout()));
    let state = Arc::new((
        Mutex::new(JobState {
            job: None,
            shutdown: false,
        }),
        Condvar::new(),
        AtomicU64::new(0),
    ));
    let (events_tx, events_rx) = mpsc::channel();
    let reporter_output = Arc::clone(&output);
    let reporter = thread::spawn(move || {
        for event in events_rx {
            match event {
                Event::Hashrate {
                    worker,
                    job_id,
                    hashes,
                    elapsed_ms,
                } => write_json(
                    &reporter_output,
                    json!({
                        "type": "hashrate", "worker": worker, "jobId": job_id, "hashes": hashes, "elapsedMs": elapsed_ms,
                    }),
                ),
                Event::Solved {
                    worker,
                    job_id,
                    nonce,
                    hash,
                } => write_json(
                    &reporter_output,
                    json!({
                        "type": "solved", "worker": worker, "jobId": job_id, "nonce": nonce, "hash": hex(&hash),
                    }),
                ),
                Event::Exhausted { worker, job_id } => write_json(
                    &reporter_output,
                    json!({
                        "type": "exhausted", "worker": worker, "jobId": job_id,
                    }),
                ),
            }
        }
    });

    let worker_threads: Vec<_> = (0..workers)
        .map(|worker| {
            let worker_state = Arc::clone(&state);
            let worker_events = events_tx.clone();
            thread::spawn(move || worker_loop(worker, lanes, worker_state, worker_events))
        })
        .collect();
    drop(events_tx);
    write_json(
        &output,
        json!({ "type": "ready", "workers": workers, "lanes": lanes }),
    );

    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<Command>(&line) {
            Ok(Command::Start {
                job_id,
                header_hex,
                target_hex,
                nonce_offset,
                nonce_stride,
            }) => {
                let result =
                    parse_job(job_id, &header_hex, &target_hex, nonce_offset, nonce_stride);
                match result {
                    Ok(job) => replace_job(&state, Some(job)),
                    Err(error) => write_json(&output, json!({ "type": "error", "message": error })),
                }
            }
            Ok(Command::Stop) => replace_job(&state, None),
            Ok(Command::Shutdown) => break,
            Err(error) => write_json(
                &output,
                json!({ "type": "error", "message": format!("invalid command: {error}") }),
            ),
        }
    }

    {
        let (lock, wake, generation) = &*state;
        let mut guard = lock.lock().unwrap();
        guard.shutdown = true;
        generation.fetch_add(1, Ordering::Release);
        wake.notify_all();
    }
    for worker in worker_threads {
        let _ = worker.join();
    }
    let _ = reporter.join();
}

fn worker_loop(worker: usize, lanes: usize, state: SharedState, events: mpsc::Sender<Event>) {
    let mut hasher = LaneHasher::new(lanes);
    let mut seen_generation = 0;
    loop {
        let job = {
            let (lock, wake, generation) = &*state;
            let mut guard = lock.lock().unwrap();
            while !guard.shutdown
                && (guard.job.is_none() || generation.load(Ordering::Acquire) == seen_generation)
            {
                guard = wake.wait(guard).unwrap();
            }
            if guard.shutdown {
                return;
            }
            seen_generation = generation.load(Ordering::Acquire);
            guard.job.clone().unwrap()
        };
        grind(worker, &job, seen_generation, &mut hasher, &state, &events);
    }
}

fn grind(
    worker: usize,
    job: &Job,
    generation: u64,
    hasher: &mut LaneHasher,
    state: &SharedState,
    events: &mpsc::Sender<Event>,
) {
    let start_nonce = job.nonce_offset.wrapping_add(worker as u32);
    let mut nonce = start_nonce;
    let mut hashes = 0_u64;
    let mut reported_at = Instant::now();
    let lanes = hasher.lanes();
    let batch_stride = job.nonce_stride.wrapping_mul(lanes as u32);

    loop {
        for _ in 0..CHECK_INTERVAL {
            if generation_changed(state, generation) {
                return;
            }
            if let Some((solved_nonce, hash)) =
                grind_batch(hasher, job, nonce, start_nonce, &mut hashes)
            {
                clear_current_job(state, generation);
                if let Some(hash) = hash {
                    let _ = events.send(Event::Solved {
                        worker,
                        job_id: job.id,
                        nonce: solved_nonce,
                        hash,
                    });
                } else {
                    let _ = events.send(Event::Exhausted {
                        worker,
                        job_id: job.id,
                    });
                }
                return;
            }
            nonce = nonce.wrapping_add(batch_stride);
            if nonce == start_nonce {
                clear_current_job(state, generation);
                let _ = events.send(Event::Exhausted {
                    worker,
                    job_id: job.id,
                });
                return;
            }
        }
        let elapsed = reported_at.elapsed();
        if elapsed.as_millis() >= REPORT_INTERVAL_MS {
            let _ = events.send(Event::Hashrate {
                worker,
                job_id: job.id,
                hashes,
                elapsed_ms: elapsed.as_millis(),
            });
            hashes = 0;
            reported_at = Instant::now();
        }
    }
}

/// Returns Some((nonce, Some(hash))) on solve, Some((nonce, None)) on exhaust mid-batch,
/// or None to continue.
fn grind_batch(
    hasher: &mut LaneHasher,
    job: &Job,
    nonce: u32,
    start_nonce: u32,
    hashes: &mut u64,
) -> Option<(u32, Option<[u8; 32]>)> {
    match hasher {
        LaneHasher::L1(hasher) => {
            let mut header = job.header;
            header[NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&nonce.to_be_bytes());
            let hash = hasher.hash(&header);
            *hashes += 1;
            if hash_meets_target(&hash, &job.target) {
                return Some((nonce, Some(hash)));
            }
            None
        }
        LaneHasher::L2(hasher) => grind_lanes::<2, _>(
            |headers| hasher.hash_batch(headers),
            job,
            nonce,
            start_nonce,
            hashes,
        ),
        LaneHasher::L4(hasher) => grind_lanes::<4, _>(
            |headers| hasher.hash_batch(headers),
            job,
            nonce,
            start_nonce,
            hashes,
        ),
    }
}

fn grind_lanes<const LANES: usize, F>(
    mut hash_batch: F,
    job: &Job,
    nonce: u32,
    start_nonce: u32,
    hashes: &mut u64,
) -> Option<(u32, Option<[u8; 32]>)>
where
    F: FnMut(&[[u8; HEADER_LEN]; LANES]) -> [[u8; 32]; LANES],
{
    let mut headers = [[0_u8; HEADER_LEN]; LANES];
    let mut lane_nonces = [0_u32; LANES];
    let mut active = LANES;
    for lane in 0..LANES {
        let lane_nonce = nonce.wrapping_add(job.nonce_stride.wrapping_mul(lane as u32));
        if lane > 0 && lane_nonce == start_nonce {
            active = lane;
            break;
        }
        lane_nonces[lane] = lane_nonce;
        headers[lane] = job.header;
        headers[lane][NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&lane_nonce.to_be_bytes());
    }
    if active == 0 {
        return Some((nonce, None));
    }
    if active == LANES {
        let digests = hash_batch(&headers);
        *hashes += LANES as u64;
        for lane in 0..LANES {
            if hash_meets_target(&digests[lane], &job.target) {
                return Some((lane_nonces[lane], Some(digests[lane])));
            }
        }
        return None;
    }

    // Partial final batch near nonce-space wrap: fall back to scalar-equivalent one-by-one
    // through a temporary single-lane batch path by hashing only filled headers via full batch
    // would mix unused lanes. Use single-hash Sandglass for the remainder instead.
    let mut scalar = Sandglass::new();
    for lane in 0..active {
        let hash = scalar.hash(&headers[lane]);
        *hashes += 1;
        if hash_meets_target(&hash, &job.target) {
            return Some((lane_nonces[lane], Some(hash)));
        }
    }
    Some((nonce, None))
}

fn generation_changed(state: &SharedState, generation: u64) -> bool {
    state.2.load(Ordering::Relaxed) != generation
}

fn clear_current_job(state: &SharedState, generation: u64) {
    let (lock, wake, current_generation) = &**state;
    let mut guard = lock.lock().unwrap();
    if current_generation.load(Ordering::Acquire) == generation {
        guard.job = None;
        current_generation.fetch_add(1, Ordering::Release);
        wake.notify_all();
    }
}

fn replace_job(state: &SharedState, job: Option<Job>) {
    let (lock, wake, generation) = &**state;
    let mut guard = lock.lock().unwrap();
    guard.job = job;
    generation.fetch_add(1, Ordering::Release);
    wake.notify_all();
}

fn parse_job(
    id: u64,
    header: &str,
    target: &str,
    nonce_offset: u32,
    nonce_stride: u32,
) -> Result<Job, String> {
    if nonce_stride == 0 {
        return Err("nonceStride must be positive".into());
    }
    Ok(Job {
        id,
        header: decode_hex(header).ok_or("headerHex must contain exactly 148 bytes")?,
        target: decode_hex(target).ok_or("targetHex must contain exactly 32 bytes")?,
        nonce_offset,
        nonce_stride,
    })
}

fn decode_hex<const N: usize>(input: &str) -> Option<[u8; N]> {
    if input.len() != N * 2 {
        return None;
    }
    let mut output = [0_u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&input[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(output)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_json(output: &Arc<Mutex<io::Stdout>>, value: serde_json::Value) {
    let mut writer = output.lock().unwrap();
    let _ = writeln!(writer, "{value}");
    let _ = writer.flush();
}

fn parse_workers() -> usize {
    let args: Vec<_> = std::env::args().collect();
    args.windows(2)
        .find(|pair| pair[0] == "--workers")
        .and_then(|pair| pair[1].parse().ok())
        .filter(|workers: &usize| *workers > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
        })
}

fn parse_lanes() -> usize {
    // Default to 2: local Core Ultra 9 measurements showed ~25% higher aggregate
    // H/s than single-lane, while 4 lanes regressed from memory pressure.
    match std::env::var("SANDGLASS_LANES").as_deref() {
        Ok("1") => 1,
        Ok("2") | Err(_) => 2,
        Ok("4") => 4,
        Ok(other) => {
            eprintln!("sandglass-native-miner: invalid SANDGLASS_LANES={other}; using 2");
            2
        }
    }
}
