//! Embedded deterministic PRNG (SplitMix64) — no `rand` dependency, so the
//! output stream can never shift under a dependency upgrade.

pub const GLOBAL_SEED: u64 = 0x5EED_5A17_1E5A_3B1E;

pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n` (modulo bias is irrelevant for sample data).
    pub fn range(&mut self, n: u64) -> u64 {
        assert!(n > 0, "range(0) is meaningless");
        self.next_u64() % n
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len() as u64) as usize]
    }

    /// `k` distinct items in first-seen order (partial Fisher–Yates over indices).
    pub fn pick_k<'a, T>(&mut self, items: &'a [T], k: usize) -> Vec<&'a T> {
        let k = k.min(items.len());
        let mut idx: Vec<usize> = (0..items.len()).collect();
        for i in 0..k {
            let j = i + self.range((idx.len() - i) as u64) as usize;
            idx.swap(i, j);
        }
        idx[..k].iter().map(|&i| &items[i]).collect()
    }

    /// True `pct`% of the time.
    pub fn chance(&mut self, pct: u64) -> bool {
        self.range(100) < pct
    }
}

pub fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Content stream for one file, keyed off its tree-relative path.
pub fn file_rng(rel_path: &str) -> SplitMix64 {
    SplitMix64::new(GLOBAL_SEED ^ fnv1a(rel_path))
}

/// Naming stream for the i-th file of a theme (paths need names before they exist).
pub fn stream_rng(tag: &str, i: u64) -> SplitMix64 {
    SplitMix64::new(GLOBAL_SEED ^ fnv1a(tag) ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known SplitMix64 test vectors for seed 0 — pins the algorithm itself,
    /// which is what makes regeneration stable across builds.
    #[test]
    fn splitmix64_known_vectors() {
        let mut r = SplitMix64::new(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(r.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn range_is_bounded_and_deterministic() {
        let mut r = SplitMix64::new(42);
        let vals: Vec<u64> = (0..100).map(|_| r.range(7)).collect();
        assert!(vals.iter().all(|v| *v < 7));
        let mut r2 = SplitMix64::new(42);
        let vals2: Vec<u64> = (0..100).map(|_| r2.range(7)).collect();
        assert_eq!(vals, vals2);
    }

    #[test]
    fn pick_k_returns_distinct_items() {
        let items = ["a", "b", "c", "d", "e"];
        let mut r = SplitMix64::new(7);
        let picked = r.pick_k(&items, 3);
        assert_eq!(picked.len(), 3);
        let mut sorted: Vec<_> = picked.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "picks must be distinct");
    }

    #[test]
    fn pick_k_clamps_to_len() {
        let items = ["a", "b"];
        let mut r = SplitMix64::new(7);
        assert_eq!(r.pick_k(&items, 10).len(), 2);
    }

    #[test]
    fn fnv1a_known_values() {
        // FNV-1a 64-bit: empty string hashes to the offset basis.
        assert_eq!(fnv1a(""), 0xCBF2_9CE4_8422_2325);
        assert_ne!(fnv1a("a"), fnv1a("b"));
    }

    #[test]
    fn file_rng_depends_only_on_path() {
        let a1: Vec<u64> = {
            let mut r = file_rng("work/plans/DCP-100-x.md");
            (0..5).map(|_| r.next_u64()).collect()
        };
        let a2: Vec<u64> = {
            let mut r = file_rng("work/plans/DCP-100-x.md");
            (0..5).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = file_rng("work/plans/DCP-101-x.md");
            (0..5).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }
}
