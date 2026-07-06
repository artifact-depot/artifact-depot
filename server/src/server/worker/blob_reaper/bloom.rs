// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

// ---------------------------------------------------------------------------
// Bloom filter helpers (using `bloomfilter` crate)
// ---------------------------------------------------------------------------
//
// The GC pass builds per-shard filters in parallel and merges them by
// bitwise-OR, which is only sound when every shard filter hashes identically
// (same seed, same k, same size). bloomfilter 3.x serializes a filter as a
// fixed header (version, length, k, seed) followed by the raw bitmap, so
// "same parameters" is exactly "same header bytes": every merge asserts
// header equality before touching a bit, making a parameter mismatch — the
// failure mode that would turn live blobs into orphans — a loud failure
// instead of silent corruption.

use std::sync::atomic::{AtomicU8, Ordering};

use bloomfilter::Bloom;
use depot_core::error::DepotError;

fn bloom_err(context: &str, e: &str) -> DepotError {
    DepotError::Internal(format!("bloom filter {context}: {e}"))
}

/// Length of the serialized header preceding the bitmap payload. Derived,
/// not hardcoded: total serialized length minus the bitmap length implied by
/// the filter's bit count.
fn header_len(filter: &Bloom<[u8]>) -> usize {
    let total = filter.as_slice().len();
    let bitmap_bytes = (filter.len() / 8) as usize;
    total.saturating_sub(bitmap_bytes)
}

/// Create an empty bloom filter with the same dimensions and hash seed as
/// `template`, for parallel construction followed by merging into a
/// `BloomAccumulator` or via `bloom_union`.
pub(crate) fn bloom_empty_like(template: &Bloom<[u8]>) -> depot_core::error::Result<Bloom<[u8]>> {
    // Round-trip through the serialized form (`Bloom<[u8]>` has no `Clone`:
    // the derive requires `[u8]: Clone`), then drop the copied bits. The
    // round-trip cannot fail on a well-formed input; the error path exists
    // to satisfy the crate's fallible API without panicking.
    let mut b = Bloom::from_bytes(template.to_bytes()).map_err(|e| bloom_err("clone", e))?;
    b.clear();
    Ok(b)
}

/// Merge `source` into `target` via bitwise OR. Both filters must have
/// identical parameters (created via `bloom_empty_like`); a mismatch is an
/// error rather than a filter that hashes differently than its inputs —
/// merging mismatched filters would make live blobs read as orphans.
///
/// Allocates a fresh buffer per call. For tight merge loops in hot paths,
/// prefer `BloomAccumulator::or_from` which ORs atomically in place.
pub(crate) fn bloom_union(
    target: &mut Bloom<[u8]>,
    source: &Bloom<[u8]>,
) -> depot_core::error::Result<()> {
    let h = header_len(target);
    let mut merged = target.to_bytes();
    let src = source.as_slice();
    if merged.get(..h) != src.get(..h) || merged.len() != src.len() {
        return Err(bloom_err("union", "filter parameter mismatch"));
    }
    for (t, s) in merged.iter_mut().skip(h).zip(src.iter().skip(h)) {
        *t |= s;
    }
    *target = Bloom::from_bytes(merged).map_err(|e| bloom_err("union", e))?;
    Ok(())
}

/// Lock-free accumulator that folds shard-local bloom filters into one
/// combined filter via atomic bitwise-OR. Designed so each shard task can
/// `or_from` its local filter (behind `Arc<Self>`, `&self`) and drop that
/// local filter immediately, keeping peak memory bounded by the number of
/// concurrent scan tasks rather than the total number of shards.
///
/// The `bloomfilter` crate has no atomic view of its bitmap, so the
/// accumulator keeps the template's serialized header plus its own
/// `Vec<AtomicU8>` payload, and materialises a plain `Bloom<[u8]>` once via
/// `finalize`.
pub(crate) struct BloomAccumulator {
    header: Vec<u8>,
    bits: Vec<AtomicU8>,
}

impl BloomAccumulator {
    /// Create an all-zero accumulator matching `template`'s parameters.
    pub(crate) fn empty_like(template: &Bloom<[u8]>) -> Self {
        let h = header_len(template);
        let serialized = template.as_slice();
        let header = serialized.get(..h).unwrap_or_default().to_vec();
        let payload_len = serialized.len().saturating_sub(h);
        let mut bits = Vec::with_capacity(payload_len);
        bits.resize_with(payload_len, || AtomicU8::new(0));
        Self { header, bits }
    }

    /// OR `source`'s bitmap into this accumulator via per-byte atomic OR.
    ///
    /// Safe to call concurrently from many tasks with `&self`; each byte
    /// is updated independently with `Relaxed` ordering (sufficient because
    /// OR is commutative and idempotent, and the final read happens after
    /// every contributor has observed its `or_from` return).
    ///
    /// `source` must have been built from a template with identical
    /// parameters (typically via `bloom_empty_like`). A mismatch would OR
    /// bits hashed under a different scheme into the live-blob filter and
    /// cause real blobs to be treated as orphans, so it is checked, not
    /// assumed.
    pub(crate) fn or_from(&self, source: &Bloom<[u8]>) -> depot_core::error::Result<()> {
        let src = source.as_slice();
        if src.get(..self.header.len()) != Some(self.header.as_slice())
            || src.len() != self.header.len() + self.bits.len()
        {
            return Err(bloom_err("accumulate", "filter parameter mismatch"));
        }
        for (dst, s) in self.bits.iter().zip(src.iter().skip(self.header.len())) {
            // Avoid the atomic RMW for bytes whose src contribution is 0 —
            // the common case for sparse shard filters. Saves bus traffic
            // on the dense final fold.
            if *s != 0 {
                dst.fetch_or(*s, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    /// Materialise the accumulated bits as a `Bloom<[u8]>` ready for `check`.
    /// Consumes `self` so the temporary atomic storage is freed.
    pub(crate) fn finalize(self) -> depot_core::error::Result<Bloom<[u8]>> {
        let mut bytes = self.header;
        bytes.reserve(self.bits.len());
        bytes.extend(self.bits.into_iter().map(|b| b.into_inner()));
        Bloom::from_bytes(bytes).map_err(|e| bloom_err("finalize", e))
    }
}
