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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
    fn compares_targets_as_big_endian_uint256() {
        assert!(hash_meets_target(&[0; 32], &[1; 32]));
        assert!(!hash_meets_target(&[1; 32], &[1; 32]));
        assert!(!hash_meets_target(&[2; 32], &[1; 32]));
    }
}
