//! Locality-sensitive hashing for dedup candidate pair generation.

use super::*;

// -------------------------------------------------------------------------
// LSH-based candidate pair generation for deduplication
// -------------------------------------------------------------------------

/// Number of hyperplanes (bits) in each SimHash signature.
const LSH_BITS: usize = 16;
/// Number of bands the signature is split into (LSH_BITS = LSH_BANDS *
/// LSH_ROWS).
const LSH_BANDS: usize = 4;
/// Bits per band.
const LSH_ROWS: usize = LSH_BITS / LSH_BANDS;
/// Deterministic seed so signatures are stable across runs.
const LSH_SEED: u64 = 0x0DEA_D0DE_C0FF_EE42;

/// Fixed set of random hyperplanes for a given embedding dimension.
///
/// Uses `StdRng::seed_from_u64(LSH_SEED)` so successive runs produce identical
/// signatures for identical inputs (important for reproducible dedup).
fn hyperplanes(dim: usize) -> Vec<Vec<f32>> {
    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};
    let mut rng = StdRng::seed_from_u64(LSH_SEED);
    let mut planes = Vec::with_capacity(LSH_BITS);
    for _ in 0..LSH_BITS {
        let mut h = Vec::with_capacity(dim);
        for _ in 0..dim {
            // Gaussian ~ N(0,1) via Box-Muller. Clamping u1 avoids log(0).
            let u1: f32 = rng.random::<f32>().max(1e-12);
            let u2: f32 = rng.random::<f32>();
            let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos();
            h.push(z);
        }
        planes.push(h);
    }
    planes
}

/// Compute a `LSH_BITS`-bit SimHash signature for an embedding.
fn simhash_signature(planes: &[Vec<f32>], emb: &[f32]) -> u32 {
    let mut sig: u32 = 0;
    for (i, plane) in planes.iter().enumerate().take(LSH_BITS) {
        if plane.len() != emb.len() {
            continue;
        }
        let dot: f32 = plane.iter().zip(emb.iter()).map(|(a, b)| a * b).sum();
        if dot >= 0.0 {
            sig |= 1 << i;
        }
    }
    sig
}

/// Generate candidate `(i, j)` index pairs (i < j) that should be checked
/// pairwise for near-duplicate detection.
///
/// - Memories with an embedding of matching dimension are bucketed via LSH
///   banding: pairs colliding in **any** band become candidates.
/// - Memories without an embedding fall back to grouping by the lowercased
///   50-character prefix (matches the pre-existing textual fallback).
/// - Cross-group pairs are never emitted (mirrors the original loop's `(Some,
///   None)` short-circuit).
pub(super) fn build_dedup_candidate_pairs(memories: &[Memory]) -> Vec<(usize, usize)> {
    let mut pairs: HashSet<(usize, usize)> = HashSet::new();

    // Bucket 1: embedding-based LSH banding.
    // Pick a dimension from the first embedding we see; skip any embedding whose
    // dimension differs (mixed-model corpora are rare, so this trades a bit of
    // recall for simplicity — such entries just don't participate in LSH).
    let dim = memories
        .iter()
        .find_map(|m| m.embedding.as_ref().map(|e| e.len()))
        .filter(|d| *d > 0);

    if let Some(dim) = dim {
        let planes = hyperplanes(dim);
        // `buckets[band][band_value]` -> list of memory indices.
        let mut buckets: Vec<HashMap<u32, Vec<usize>>> =
            (0..LSH_BANDS).map(|_| HashMap::new()).collect();
        let band_mask: u32 = (1u32 << LSH_ROWS) - 1;

        for (idx, mem) in memories.iter().enumerate() {
            let Some(emb) = mem.embedding.as_ref() else {
                continue;
            };
            if emb.len() != dim {
                continue;
            }
            let sig = simhash_signature(&planes, emb);
            for (b, bucket) in buckets.iter_mut().enumerate() {
                let key = (sig >> (b * LSH_ROWS)) & band_mask;
                bucket.entry(key).or_default().push(idx);
            }
        }

        for band in &buckets {
            for members in band.values() {
                if members.len() < 2 {
                    continue;
                }
                for a in 0..members.len() {
                    for b in (a + 1)..members.len() {
                        let (i, j) = (members[a], members[b]);
                        if i < j {
                            pairs.insert((i, j));
                        } else {
                            pairs.insert((j, i));
                        }
                    }
                }
            }
        }
    }

    // Bucket 2: prefix-hash for memories without an embedding.
    let mut prefix_buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, mem) in memories.iter().enumerate() {
        if mem.embedding.is_some() {
            continue;
        }
        let key: String = mem.content.to_lowercase().chars().take(50).collect();
        prefix_buckets.entry(key).or_default().push(idx);
    }
    for members in prefix_buckets.values() {
        if members.len() < 2 {
            continue;
        }
        for a in 0..members.len() {
            for b in (a + 1)..members.len() {
                let (i, j) = (members[a], members[b]);
                if i < j {
                    pairs.insert((i, j));
                } else {
                    pairs.insert((j, i));
                }
            }
        }
    }

    let mut out: Vec<(usize, usize)> = pairs.into_iter().collect();
    // Sort so iteration order is deterministic; earlier pairs win when
    // importance ties break in favour of index-smaller memories.
    out.sort_unstable();
    out
}
