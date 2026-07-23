use browsercoin_sandglass::{HEADER_LEN, Sandglass, hash_meets_target};
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

fn main() {
    let workers = parse_workers();
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
            thread::spawn(move || worker_loop(worker, worker_state, worker_events))
        })
        .collect();
    drop(events_tx);
    write_json(&output, json!({ "type": "ready", "workers": workers }));

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

fn worker_loop(worker: usize, state: SharedState, events: mpsc::Sender<Event>) {
    let mut hasher = Sandglass::new();
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
    hasher: &mut Sandglass,
    state: &SharedState,
    events: &mpsc::Sender<Event>,
) {
    let start_nonce = job.nonce_offset.wrapping_add(worker as u32);
    let mut nonce = start_nonce;
    let mut header = job.header;
    let mut hashes = 0_u64;
    let mut reported_at = Instant::now();

    loop {
        for _ in 0..CHECK_INTERVAL {
            if generation_changed(state, generation) {
                return;
            }
            header[NONCE_OFFSET..NONCE_OFFSET + 4].copy_from_slice(&nonce.to_be_bytes());
            let hash = hasher.hash(&header);
            hashes += 1;
            if hash_meets_target(&hash, &job.target) {
                clear_current_job(state, generation);
                let _ = events.send(Event::Solved {
                    worker,
                    job_id: job.id,
                    nonce,
                    hash,
                });
                return;
            }
            nonce = nonce.wrapping_add(job.nonce_stride);
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
