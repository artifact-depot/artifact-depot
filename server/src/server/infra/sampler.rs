// SPDX-FileCopyrightText: 2026 Artifact Depot Contributors
//
// SPDX-License-Identifier: Apache-2.0

//! Adaptive rate-limiting sampler for OpenTelemetry traces.
//!
//! At low throughput every trace is sampled. Once the rate exceeds
//! `max_traces_per_sec` the sampler switches to probabilistic sampling
//! so that the export volume stays bounded.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use opentelemetry::trace::{Link, SpanKind, TraceId, TraceState};
use opentelemetry::{Context, KeyValue};
use opentelemetry_sdk::trace::{SamplingDecision, SamplingResult, ShouldSample};

/// A sampler that allows up to `max_traces_per_sec` root traces per second.
///
/// Within each one-second window every trace is accepted until the budget is
/// exhausted; further root traces in the same window are dropped.  Child spans
/// whose parent was sampled are always accepted (parent-based semantics).
#[derive(Debug, Clone)]
pub struct RateLimitingSampler {
    max_per_sec: u64,
    /// Atomic counter packed as `(window_epoch_secs << 32) | count`.
    /// Using a single atomic avoids any locking.
    state: std::sync::Arc<AtomicU64>,
    start: Instant,
}

impl RateLimitingSampler {
    pub fn new(max_traces_per_sec: u64) -> Self {
        Self {
            max_per_sec: max_traces_per_sec,
            state: std::sync::Arc::new(AtomicU64::new(0)),
            start: Instant::now(),
        }
    }

    /// Returns the current second (monotonic) and tries to increment the
    /// counter.  Returns `true` if within budget.
    fn try_acquire(&self) -> bool {
        let now_sec = self.start.elapsed().as_secs() as u32;
        loop {
            let current = self.state.load(Ordering::Relaxed);
            let window = (current >> 32) as u32;
            let count = current as u32;

            if window == now_sec {
                if (count as u64) >= self.max_per_sec {
                    return false;
                }
                let new = ((now_sec as u64) << 32) | ((count + 1) as u64);
                if self
                    .state
                    .compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
                // CAS failed — retry.
            } else {
                // New window — reset counter to 1.
                let new = ((now_sec as u64) << 32) | 1;
                if self
                    .state
                    .compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    return true;
                }
            }
        }
    }
}

impl ShouldSample for RateLimitingSampler {
    fn should_sample(
        &self,
        parent_context: Option<&Context>,
        _trace_id: TraceId,
        _name: &str,
        _span_kind: &SpanKind,
        _attributes: &[KeyValue],
        _links: &[Link],
    ) -> SamplingResult {
        // Parent-based: if the parent was sampled, always sample the child.
        if let Some(cx) = parent_context {
            use opentelemetry::trace::TraceContextExt;
            let span = cx.span();
            let sc = span.span_context();
            if sc.is_valid() && sc.is_sampled() {
                return SamplingResult {
                    decision: SamplingDecision::RecordAndSample,
                    attributes: Vec::new(),
                    trace_state: TraceState::default(),
                };
            }
        }

        // Root span — apply rate limit.
        if self.try_acquire() {
            SamplingResult {
                decision: SamplingDecision::RecordAndSample,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            }
        } else {
            SamplingResult {
                decision: SamplingDecision::Drop,
                attributes: Vec::new(),
                trace_state: TraceState::default(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_budget() {
        let s = RateLimitingSampler::new(5);
        for _ in 0..5 {
            assert!(s.try_acquire());
        }
        assert!(!s.try_acquire());
    }

    #[test]
    fn resets_on_new_window() {
        // Use a budget of 1 to make it easy to exhaust.
        let s = RateLimitingSampler::new(1);
        assert!(s.try_acquire());
        assert!(!s.try_acquire());
        // Simulate advancing to the next second by poking the state.
        let current = s.state.load(Ordering::Relaxed);
        let next_window = ((current >> 32) + 1) << 32;
        s.state.store(next_window, Ordering::Relaxed);
        assert!(s.try_acquire());
    }
}
