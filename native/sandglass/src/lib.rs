use sha2::{Digest, Sha256};

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub const HEADER_LEN: usize = 148;
const WORDS: usize = 1 << 17;
const HUGE_PAGE_BYTES: usize = 2 << 20;
const MASK: u32 = (WORDS - 1) as u32;
const PER_CHAIN: u32 = (1 << 21) / 4;
const GOLDEN: u32 = 0x9e37_79b9;

pub struct Sandglass {
    scratch: Scratch,
    #[cfg(target_arch = "x86_64")]
    use_avx2: bool,
    #[cfg(target_arch = "x86_64")]
    prefetch: bool,
}

enum Scratch {
    Heap(Box<[u32; WORDS]>),
    Huge(*mut u32),
}

// A scratch allocation is owned by exactly one `Sandglass`, which in turn is
// owned by one mining thread. The raw mapping is never shared across threads.
unsafe impl Send for Scratch {}

impl Scratch {
    fn new() -> Self {
        if std::env::var("SANDGLASS_HUGEPAGE").is_ok_and(|value| value == "1") {
            // SAFETY: mmap returns an owned anonymous mapping which is released
            // in Drop. The entire mapping is faulted once so THP can collapse it.
            let memory = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    HUGE_PAGE_BYTES,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if memory != libc::MAP_FAILED {
                unsafe {
                    libc::madvise(memory, HUGE_PAGE_BYTES, libc::MADV_HUGEPAGE);
                    std::ptr::write_bytes(memory, 0, HUGE_PAGE_BYTES);
                }
                return Self::Huge(memory.cast());
            }
        }
        Self::Heap(Box::new([0; WORDS]))
    }

    fn as_mut_ptr(&mut self) -> *mut u32 {
        match self {
            Self::Heap(words) => words.as_mut_ptr(),
            Self::Huge(memory) => *memory,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Self::Huge(memory) = self {
            // SAFETY: the mapping was allocated by Scratch::new and is released
            // exactly once when its owning Sandglass is dropped.
            unsafe { libc::munmap((*memory).cast(), HUGE_PAGE_BYTES) };
        }
    }
}

impl Default for Sandglass {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandglass {
    pub fn new() -> Self {
        Self {
            scratch: Scratch::new(),
            #[cfg(target_arch = "x86_64")]
            use_avx2: std::env::var("SANDGLASS_MODE").is_ok_and(|mode| mode == "avx2")
                && is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            prefetch: std::env::var("SANDGLASS_PREFETCH").is_ok_and(|value| value == "1"),
        }
    }

    pub fn hash(&mut self, header: &[u8; HEADER_LEN]) -> [u8; 32] {
        let seed: [u8; 32] = Sha256::digest(header).into();
        let [h, a0, a1, a2, a3] = self.fill_and_walk(&seed);
        let mut final_input = [0_u8; 52];
        final_input[..32].copy_from_slice(&seed);
        final_input[32..36].copy_from_slice(&h.to_be_bytes());
        final_input[36..40].copy_from_slice(&a0.to_be_bytes());
        final_input[40..44].copy_from_slice(&a1.to_be_bytes());
        final_input[44..48].copy_from_slice(&a2.to_be_bytes());
        final_input[48..52].copy_from_slice(&a3.to_be_bytes());
        Sha256::digest(final_input).into()
    }

    fn fill_and_walk(&mut self, seed: &[u8; 32]) -> [u32; 5] {
        let mut seed_words = [0_u32; 8];
        for (index, word) in seed_words.iter_mut().enumerate() {
            *word = u32::from_be_bytes(seed[index * 4..index * 4 + 4].try_into().unwrap());
        }

        let mut h = mix(seed_words[0] ^ GOLDEN);
        for index in 0..WORDS {
            h = mix(h.wrapping_add(GOLDEN).wrapping_add(seed_words[index & 7]));
            // SAFETY: fill indexes are bounded by WORDS.
            unsafe { *self.scratch.as_mut_ptr().add(index) = h };
        }

        let mut x = h;
        x = mix(x ^ 1);
        let a0 = mix(x ^ GOLDEN);
        let i0 = x & MASK;
        x = mix(x ^ 2);
        let a1 = mix(x ^ GOLDEN);
        let i1 = x & MASK;
        x = mix(x ^ 3);
        let a2 = mix(x ^ GOLDEN);
        let i2 = x & MASK;
        x = mix(x ^ 4);
        let a3 = mix(x ^ GOLDEN);
        let i3 = x & MASK;

        #[cfg(target_arch = "x86_64")]
        if self.use_avx2 {
            // SAFETY: `use_avx2` is set only after runtime feature detection.
            return unsafe { self.walk_avx2(h, a0, i0, a1, i1, a2, i2, a3, i3) };
        }
        #[cfg(target_arch = "x86_64")]
        if self.prefetch {
            return self.walk_scalar::<true>(h, a0, i0, a1, i1, a2, i2, a3, i3);
        }
        self.walk_scalar::<false>(h, a0, i0, a1, i1, a2, i2, a3, i3)
    }

    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn walk_scalar<const PREFETCH: bool>(
        &mut self,
        h: u32,
        mut a0: u32,
        mut i0: u32,
        mut a1: u32,
        mut i1: u32,
        mut a2: u32,
        mut i2: u32,
        mut a3: u32,
        mut i3: u32,
    ) -> [u32; 5] {
        let scratch = self.scratch.as_mut_ptr();
        for step in 0..PER_CHAIN {
            // SAFETY: every index is masked with MASK (WORDS - 1) after it
            // is derived, so all accesses remain inside the scratch array.
            a0 = mix(a0 ^ unsafe { *scratch.add(i0 as usize) });
            unsafe { *scratch.add(i0 as usize) = a0.wrapping_add(step) };
            i0 = a0 & MASK;
            if PREFETCH {
                prefetch(unsafe { scratch.add(i0 as usize) });
            }
            a1 = mix(a1 ^ unsafe { *scratch.add(i1 as usize) });
            unsafe { *scratch.add(i1 as usize) = a1.wrapping_add(step) };
            i1 = a1 & MASK;
            if PREFETCH {
                prefetch(unsafe { scratch.add(i1 as usize) });
            }
            a2 = mix(a2 ^ unsafe { *scratch.add(i2 as usize) });
            unsafe { *scratch.add(i2 as usize) = a2.wrapping_add(step) };
            i2 = a2 & MASK;
            if PREFETCH {
                prefetch(unsafe { scratch.add(i2 as usize) });
            }
            a3 = mix(a3 ^ unsafe { *scratch.add(i3 as usize) });
            unsafe { *scratch.add(i3 as usize) = a3.wrapping_add(step) };
            i3 = a3 & MASK;
            if PREFETCH {
                prefetch(unsafe { scratch.add(i3 as usize) });
            }
        }
        [h, a0, a1, a2, a3]
    }

    #[cfg(target_arch = "x86_64")]
    #[allow(clippy::too_many_arguments)]
    #[target_feature(enable = "avx2")]
    unsafe fn walk_avx2(
        &mut self,
        h: u32,
        mut a0: u32,
        mut i0: u32,
        mut a1: u32,
        mut i1: u32,
        mut a2: u32,
        mut i2: u32,
        mut a3: u32,
        mut i3: u32,
    ) -> [u32; 5] {
        let scratch = self.scratch.as_mut_ptr();
        for step in 0..PER_CHAIN {
            // The reference walk performs chains in order. A gather is only
            // equivalent when all four current addresses differ; collisions
            // are rare but must fall back to the exact scalar ordering.
            if has_collision(i0, i1, i2, i3) {
                a0 = mix(a0 ^ unsafe { *scratch.add(i0 as usize) });
                unsafe { *scratch.add(i0 as usize) = a0.wrapping_add(step) };
                i0 = a0 & MASK;
                a1 = mix(a1 ^ unsafe { *scratch.add(i1 as usize) });
                unsafe { *scratch.add(i1 as usize) = a1.wrapping_add(step) };
                i1 = a1 & MASK;
                a2 = mix(a2 ^ unsafe { *scratch.add(i2 as usize) });
                unsafe { *scratch.add(i2 as usize) = a2.wrapping_add(step) };
                i2 = a2 & MASK;
                a3 = mix(a3 ^ unsafe { *scratch.add(i3 as usize) });
                unsafe { *scratch.add(i3 as usize) = a3.wrapping_add(step) };
                i3 = a3 & MASK;
                continue;
            }

            let indices = _mm_setr_epi32(i0 as i32, i1 as i32, i2 as i32, i3 as i32);
            // SAFETY: indices are masked to the scratch array bounds.
            let values = unsafe { _mm_i32gather_epi32(scratch as *const i32, indices, 4) };
            let states = _mm_setr_epi32(a0 as i32, a1 as i32, a2 as i32, a3 as i32);
            // SAFETY: this function is AVX2-gated by the caller.
            let next = unsafe { mix_avx2(_mm_xor_si128(states, values)) };
            let writes = _mm_add_epi32(next, _mm_set1_epi32(step as i32));

            unsafe { *scratch.add(i0 as usize) = _mm_extract_epi32(writes, 0) as u32 };
            unsafe { *scratch.add(i1 as usize) = _mm_extract_epi32(writes, 1) as u32 };
            unsafe { *scratch.add(i2 as usize) = _mm_extract_epi32(writes, 2) as u32 };
            unsafe { *scratch.add(i3 as usize) = _mm_extract_epi32(writes, 3) as u32 };
            a0 = _mm_extract_epi32(next, 0) as u32;
            a1 = _mm_extract_epi32(next, 1) as u32;
            a2 = _mm_extract_epi32(next, 2) as u32;
            a3 = _mm_extract_epi32(next, 3) as u32;
            i0 = a0 & MASK;
            i1 = a1 & MASK;
            i2 = a2 & MASK;
            i3 = a3 & MASK;
        }
        [h, a0, a1, a2, a3]
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mix_avx2(mut value: __m128i) -> __m128i {
    value = _mm_xor_si128(value, _mm_srli_epi32(value, 16));
    value = _mm_mullo_epi32(value, _mm_set1_epi32(0x7feb_352d));
    value = _mm_xor_si128(value, _mm_srli_epi32(value, 15));
    value = _mm_mullo_epi32(value, _mm_set1_epi32(0x846c_a68b_u32 as i32));
    _mm_xor_si128(value, _mm_srli_epi32(value, 16))
}

#[inline]
fn has_collision(i0: u32, i1: u32, i2: u32, i3: u32) -> bool {
    i0 == i1 || i0 == i2 || i0 == i3 || i1 == i2 || i1 == i3 || i2 == i3
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn prefetch(address: *const u32) {
    // SAFETY: prefetch does not dereference the address; all callers derive it
    // from a masked index inside the scratch allocation.
    unsafe { _mm_prefetch(address.cast(), _MM_HINT_T0) };
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
fn prefetch(_: *const u32) {}

#[inline(always)]
fn mix(mut value: u32) -> u32 {
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^ (value >> 16)
}

pub fn hash_meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    hash < target
}

/// Independent Sandglass lanes interleaved for memory-level parallelism.
/// Each lane owns its own 512 KiB scratch and computes an exact consensus digest.
pub struct SandglassBatch<const LANES: usize> {
    scratches: [Scratch; LANES],
}

impl<const LANES: usize> Default for SandglassBatch<LANES> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const LANES: usize> SandglassBatch<LANES> {
    pub fn new() -> Self {
        assert!(LANES > 0, "SandglassBatch requires at least one lane");
        Self {
            scratches: std::array::from_fn(|_| Scratch::new()),
        }
    }

    pub fn hash_batch(&mut self, headers: &[[u8; HEADER_LEN]; LANES]) -> [[u8; 32]; LANES] {
        let mut seeds = [[0_u8; 32]; LANES];
        for lane in 0..LANES {
            seeds[lane] = Sha256::digest(headers[lane]).into();
        }
        let states = self.fill_and_walk_batch(&seeds);
        let mut digests = [[0_u8; 32]; LANES];
        for lane in 0..LANES {
            let [h, a0, a1, a2, a3] = states[lane];
            let mut final_input = [0_u8; 52];
            final_input[..32].copy_from_slice(&seeds[lane]);
            final_input[32..36].copy_from_slice(&h.to_be_bytes());
            final_input[36..40].copy_from_slice(&a0.to_be_bytes());
            final_input[40..44].copy_from_slice(&a1.to_be_bytes());
            final_input[44..48].copy_from_slice(&a2.to_be_bytes());
            final_input[48..52].copy_from_slice(&a3.to_be_bytes());
            digests[lane] = Sha256::digest(final_input).into();
        }
        digests
    }

    fn fill_and_walk_batch(&mut self, seeds: &[[u8; 32]; LANES]) -> [[u32; 5]; LANES] {
        // Specialized LANES=2 kernel (unrolled). Opt-in via SANDGLASS_L2_KERNEL=1;
        // default remains the generic interleaved batch (faster in aggregate benches).
        if LANES == 2 && l2_kernel_enabled() {
            let seeds2 = [
                seeds[0],
                seeds.get(1).copied().unwrap_or([0_u8; 32]),
            ];
            // SAFETY: LANES == 2, so scratches has exactly two elements.
            let scratches: &mut [Scratch; 2] =
                unsafe { &mut *(self.scratches.as_mut_ptr() as *mut [Scratch; 2]) };
            let states2 = fill_and_walk_batch_l2(scratches, &seeds2);
            let mut states = [[0_u32; 5]; LANES];
            for i in 0..LANES.min(2) {
                states[i] = states2[i];
            }
            return states;
        }
        self.fill_and_walk_batch_generic(seeds)
    }

    fn fill_and_walk_batch_generic(&mut self, seeds: &[[u8; 32]; LANES]) -> [[u32; 5]; LANES] {
        let mut seed_words = [[0_u32; 8]; LANES];
        let mut h = [0_u32; LANES];
        let mut ptrs = [std::ptr::null_mut::<u32>(); LANES];
        for lane in 0..LANES {
            for (index, word) in seed_words[lane].iter_mut().enumerate() {
                *word = u32::from_be_bytes(seeds[lane][index * 4..index * 4 + 4].try_into().unwrap());
            }
            h[lane] = mix(seed_words[lane][0] ^ GOLDEN);
            ptrs[lane] = self.scratches[lane].as_mut_ptr();
        }

        // Interleave fill writes across lanes so multiple 512 KiB streams stay in flight.
        for index in 0..WORDS {
            for lane in 0..LANES {
                h[lane] = mix(
                    h[lane]
                        .wrapping_add(GOLDEN)
                        .wrapping_add(seed_words[lane][index & 7]),
                );
                // SAFETY: fill indexes are bounded by WORDS; each lane has its own scratch.
                unsafe { *ptrs[lane].add(index) = h[lane] };
            }
        }

        let mut a0 = [0_u32; LANES];
        let mut a1 = [0_u32; LANES];
        let mut a2 = [0_u32; LANES];
        let mut a3 = [0_u32; LANES];
        let mut i0 = [0_u32; LANES];
        let mut i1 = [0_u32; LANES];
        let mut i2 = [0_u32; LANES];
        let mut i3 = [0_u32; LANES];
        for lane in 0..LANES {
            let mut x = h[lane];
            x = mix(x ^ 1);
            a0[lane] = mix(x ^ GOLDEN);
            i0[lane] = x & MASK;
            x = mix(x ^ 2);
            a1[lane] = mix(x ^ GOLDEN);
            i1[lane] = x & MASK;
            x = mix(x ^ 3);
            a2[lane] = mix(x ^ GOLDEN);
            i2[lane] = x & MASK;
            x = mix(x ^ 4);
            a3[lane] = mix(x ^ GOLDEN);
            i3[lane] = x & MASK;
        }

        // Interleave walk RMWs by chain across lanes. Within each lane the
        // chain order remains 0→1→2→3 for every step, matching the scalar path.
        // With SANDGLASS_PREFETCH=1, prefetch the next chain's current indices
        // (and the just-updated index) so the RMW stream stays in flight without
        // changing the access order.
        let do_prefetch = prefetch_enabled();
        for step in 0..PER_CHAIN {
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                for lane in 0..LANES {
                    prefetch(unsafe { ptrs[lane].add(i0[lane] as usize) });
                }
            }
            for lane in 0..LANES {
                // SAFETY: every index is masked with MASK after it is derived.
                a0[lane] = mix(a0[lane] ^ unsafe { *ptrs[lane].add(i0[lane] as usize) });
                unsafe { *ptrs[lane].add(i0[lane] as usize) = a0[lane].wrapping_add(step) };
                i0[lane] = a0[lane] & MASK;
            }
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                for lane in 0..LANES {
                    prefetch(unsafe { ptrs[lane].add(i1[lane] as usize) });
                }
            }
            for lane in 0..LANES {
                a1[lane] = mix(a1[lane] ^ unsafe { *ptrs[lane].add(i1[lane] as usize) });
                unsafe { *ptrs[lane].add(i1[lane] as usize) = a1[lane].wrapping_add(step) };
                i1[lane] = a1[lane] & MASK;
            }
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                for lane in 0..LANES {
                    prefetch(unsafe { ptrs[lane].add(i2[lane] as usize) });
                }
            }
            for lane in 0..LANES {
                a2[lane] = mix(a2[lane] ^ unsafe { *ptrs[lane].add(i2[lane] as usize) });
                unsafe { *ptrs[lane].add(i2[lane] as usize) = a2[lane].wrapping_add(step) };
                i2[lane] = a2[lane] & MASK;
            }
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                for lane in 0..LANES {
                    prefetch(unsafe { ptrs[lane].add(i3[lane] as usize) });
                }
            }
            for lane in 0..LANES {
                a3[lane] = mix(a3[lane] ^ unsafe { *ptrs[lane].add(i3[lane] as usize) });
                unsafe { *ptrs[lane].add(i3[lane] as usize) = a3[lane].wrapping_add(step) };
                i3[lane] = a3[lane] & MASK;
            }
        }

        let mut states = [[0_u32; 5]; LANES];
        for lane in 0..LANES {
            states[lane] = [h[lane], a0[lane], a1[lane], a2[lane], a3[lane]];
        }
        states
    }
}

impl SandglassBatch<2> {
    /// Fill both 512 KiB scratches only (for phase micro-benchmarks).
    pub fn fill_only(&mut self, seeds: &[[u8; 32]; 2]) -> [u32; 2] {
        fill_batch_l2(&mut self.scratches, seeds)
    }

    /// Walk both scratches only; buffers must already be filled for `h`.
    pub fn walk_only(&mut self, h: [u32; 2]) -> [[u32; 5]; 2] {
        walk_batch_l2(&mut self.scratches, h)
    }
}

/// Specialized LANES=2 fill+walk: fully unrolled lanes + software prefetch on
/// the next RMW index. Digests match the generic/scalar path.
fn fill_and_walk_batch_l2(scratches: &mut [Scratch; 2], seeds: &[[u8; 32]; 2]) -> [[u32; 5]; 2] {
    let h = fill_batch_l2(scratches, seeds);
    walk_batch_l2(scratches, h)
}

fn fill_batch_l2(scratches: &mut [Scratch; 2], seeds: &[[u8; 32]; 2]) -> [u32; 2] {
    let mut seed_words0 = [0_u32; 8];
    let mut seed_words1 = [0_u32; 8];
    for index in 0..8 {
        seed_words0[index] =
            u32::from_be_bytes(seeds[0][index * 4..index * 4 + 4].try_into().unwrap());
        seed_words1[index] =
            u32::from_be_bytes(seeds[1][index * 4..index * 4 + 4].try_into().unwrap());
    }

    let mut h0 = mix(seed_words0[0] ^ GOLDEN);
    let mut h1 = mix(seed_words1[0] ^ GOLDEN);
    let ptr0 = scratches[0].as_mut_ptr();
    let ptr1 = scratches[1].as_mut_ptr();

    // Unrolled 2-lane fill: keep both write streams in flight.
    for index in 0..WORDS {
        h0 = mix(h0.wrapping_add(GOLDEN).wrapping_add(seed_words0[index & 7]));
        h1 = mix(h1.wrapping_add(GOLDEN).wrapping_add(seed_words1[index & 7]));
        // SAFETY: fill indexes are bounded by WORDS.
        unsafe {
            *ptr0.add(index) = h0;
            *ptr1.add(index) = h1;
        }
    }
    [h0, h1]
}

fn walk_batch_l2(scratches: &mut [Scratch; 2], h: [u32; 2]) -> [[u32; 5]; 2] {
    let ptr0 = scratches[0].as_mut_ptr();
    let ptr1 = scratches[1].as_mut_ptr();

    let (mut a0_0, mut i0_0, mut a1_0, mut i1_0, mut a2_0, mut i2_0, mut a3_0, mut i3_0) =
        init_walk_chains(h[0]);
    let (mut a0_1, mut i0_1, mut a1_1, mut i1_1, mut a2_1, mut i2_1, mut a3_1, mut i3_1) =
        init_walk_chains(h[1]);

    // Prefetch next RMW targets when SANDGLASS_PREFETCH=1 (same opt-in as scalar).
    // Aggressive always-on prefetch regressed aggregate H/s on L2-fit LANES=2 hosts.
    let do_prefetch = prefetch_enabled();

    for step in 0..PER_CHAIN {
        // SAFETY: every index is masked with MASK after it is derived.
        unsafe {
            a0_0 = mix(a0_0 ^ *ptr0.add(i0_0 as usize));
            *ptr0.add(i0_0 as usize) = a0_0.wrapping_add(step);
            i0_0 = a0_0 & MASK;
            a0_1 = mix(a0_1 ^ *ptr1.add(i0_1 as usize));
            *ptr1.add(i0_1 as usize) = a0_1.wrapping_add(step);
            i0_1 = a0_1 & MASK;
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                {
                    prefetch(ptr0.add(i0_0 as usize));
                    prefetch(ptr1.add(i0_1 as usize));
                }
            }

            a1_0 = mix(a1_0 ^ *ptr0.add(i1_0 as usize));
            *ptr0.add(i1_0 as usize) = a1_0.wrapping_add(step);
            i1_0 = a1_0 & MASK;
            a1_1 = mix(a1_1 ^ *ptr1.add(i1_1 as usize));
            *ptr1.add(i1_1 as usize) = a1_1.wrapping_add(step);
            i1_1 = a1_1 & MASK;
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                {
                    prefetch(ptr0.add(i1_0 as usize));
                    prefetch(ptr1.add(i1_1 as usize));
                }
            }

            a2_0 = mix(a2_0 ^ *ptr0.add(i2_0 as usize));
            *ptr0.add(i2_0 as usize) = a2_0.wrapping_add(step);
            i2_0 = a2_0 & MASK;
            a2_1 = mix(a2_1 ^ *ptr1.add(i2_1 as usize));
            *ptr1.add(i2_1 as usize) = a2_1.wrapping_add(step);
            i2_1 = a2_1 & MASK;
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                {
                    prefetch(ptr0.add(i2_0 as usize));
                    prefetch(ptr1.add(i2_1 as usize));
                }
            }

            a3_0 = mix(a3_0 ^ *ptr0.add(i3_0 as usize));
            *ptr0.add(i3_0 as usize) = a3_0.wrapping_add(step);
            i3_0 = a3_0 & MASK;
            a3_1 = mix(a3_1 ^ *ptr1.add(i3_1 as usize));
            *ptr1.add(i3_1 as usize) = a3_1.wrapping_add(step);
            i3_1 = a3_1 & MASK;
            if do_prefetch {
                #[cfg(target_arch = "x86_64")]
                {
                    prefetch(ptr0.add(i3_0 as usize));
                    prefetch(ptr1.add(i3_1 as usize));
                }
            }
        }
    }

    [
        [h[0], a0_0, a1_0, a2_0, a3_0],
        [h[1], a0_1, a1_1, a2_1, a3_1],
    ]
}

#[inline(always)]
fn init_walk_chains(h: u32) -> (u32, u32, u32, u32, u32, u32, u32, u32) {
    let mut x = h;
    x = mix(x ^ 1);
    let a0 = mix(x ^ GOLDEN);
    let i0 = x & MASK;
    x = mix(x ^ 2);
    let a1 = mix(x ^ GOLDEN);
    let i1 = x & MASK;
    x = mix(x ^ 3);
    let a2 = mix(x ^ GOLDEN);
    let i2 = x & MASK;
    x = mix(x ^ 4);
    let a3 = mix(x ^ GOLDEN);
    let i3 = x & MASK;
    (a0, i0, a1, i1, a2, i2, a3, i3)
}

fn l2_kernel_enabled() -> bool {
    // Default OFF: on L2-fit hosts the generic interleaved batch path beat the
    // hand-unrolled LANES=2 kernel in aggregate H/s. Opt in with SANDGLASS_L2_KERNEL=1.
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("SANDGLASS_L2_KERNEL").as_deref() == Ok("1"))
}

fn prefetch_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("SANDGLASS_PREFETCH").is_ok_and(|value| value == "1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use sha2::{Digest, Sha256};

    #[derive(Deserialize)]
    struct Vector {
        #[serde(rename = "headerHex")]
        header_hex: String,
        #[serde(rename = "digestHex")]
        digest_hex: String,
    }

    fn decode<const N: usize>(hex: &str) -> [u8; N] {
        let mut bytes = [0_u8; N];
        for (index, output) in bytes.iter_mut().enumerate() {
            *output = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap();
        }
        bytes
    }

    fn header_with_nonce(base: &[u8; HEADER_LEN], nonce: u32) -> [u8; HEADER_LEN] {
        let mut header = *base;
        header[112..116].copy_from_slice(&nonce.to_be_bytes());
        header
    }

    #[test]
    fn matches_browsercoin_frozen_vectors() {
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../../src/crypto/sandglass.vectors.json"))
                .unwrap();
        let mut hasher = Sandglass::new();
        for vector in vectors {
            assert_eq!(
                hasher.hash(&decode::<HEADER_LEN>(&vector.header_hex)),
                decode::<32>(&vector.digest_hex)
            );
        }
    }

    #[test]
    fn batch_lanes_match_scalar() {
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../../src/crypto/sandglass.vectors.json"))
                .unwrap();
        let base = decode::<HEADER_LEN>(&vectors[0].header_hex);
        let mut scalar = Sandglass::new();
        let mut batch2 = SandglassBatch::<2>::new();
        let mut batch4 = SandglassBatch::<4>::new();

        let headers2 = [
            header_with_nonce(&base, 0),
            header_with_nonce(&base, 1),
        ];
        let out2 = batch2.hash_batch(&headers2);
        assert_eq!(out2[0], scalar.hash(&headers2[0]));
        assert_eq!(out2[1], scalar.hash(&headers2[1]));

        let headers4 = [
            header_with_nonce(&base, 10),
            header_with_nonce(&base, 11),
            header_with_nonce(&base, 12),
            header_with_nonce(&base, 13),
        ];
        let out4 = batch4.hash_batch(&headers4);
        for lane in 0..4 {
            assert_eq!(out4[lane], scalar.hash(&headers4[lane]));
        }

        // Exercise many nonces so walk index collisions are likely somewhere.
        for nonce in (0_u32..256).step_by(4) {
            let headers = [
                header_with_nonce(&base, nonce),
                header_with_nonce(&base, nonce.wrapping_add(1)),
                header_with_nonce(&base, nonce.wrapping_add(2)),
                header_with_nonce(&base, nonce.wrapping_add(3)),
            ];
            let batched = batch4.hash_batch(&headers);
            for lane in 0..4 {
                assert_eq!(batched[lane], scalar.hash(&headers[lane]));
            }
        }
    }

    #[test]
    fn lanes2_fill_walk_split_matches_hash_batch() {
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../../../src/crypto/sandglass.vectors.json"))
                .unwrap();
        let base = decode::<HEADER_LEN>(&vectors[0].header_hex);
        let mut batch = SandglassBatch::<2>::new();
        let mut scalar = Sandglass::new();

        for nonce in 0_u32..64 {
            let headers = [
                header_with_nonce(&base, nonce),
                header_with_nonce(&base, nonce.wrapping_add(1)),
            ];
            let seeds = [
                Sha256::digest(headers[0]).into(),
                Sha256::digest(headers[1]).into(),
            ];
            let h = batch.fill_only(&seeds);
            let states = batch.walk_only(h);
            let digests = batch.hash_batch(&headers);
            for lane in 0..2 {
                let [hh, a0, a1, a2, a3] = states[lane];
                let mut final_input = [0_u8; 52];
                final_input[..32].copy_from_slice(&seeds[lane]);
                final_input[32..36].copy_from_slice(&hh.to_be_bytes());
                final_input[36..40].copy_from_slice(&a0.to_be_bytes());
                final_input[40..44].copy_from_slice(&a1.to_be_bytes());
                final_input[44..48].copy_from_slice(&a2.to_be_bytes());
                final_input[48..52].copy_from_slice(&a3.to_be_bytes());
                let from_split: [u8; 32] = Sha256::digest(final_input).into();
                assert_eq!(from_split, digests[lane]);
                assert_eq!(digests[lane], scalar.hash(&headers[lane]));
            }
        }
    }

    #[test]
    fn compares_targets_as_big_endian_uint256() {
        assert!(hash_meets_target(&[0; 32], &[1; 32]));
        assert!(!hash_meets_target(&[1; 32], &[1; 32]));
        assert!(!hash_meets_target(&[2; 32], &[1; 32]));
    }
}
