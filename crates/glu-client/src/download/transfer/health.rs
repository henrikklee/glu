use super::{AttemptKind, ByteRange, TransferPolicy};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HedgeDecision {
    None,
    Localized,
    Ambiguous,
}

impl HedgeDecision {
    pub(super) fn attempt_kind(self) -> Option<AttemptKind> {
        match self {
            Self::None => None,
            Self::Localized => Some(AttemptKind::Hedge),
            Self::Ambiguous => Some(AttemptKind::Diagnostic),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct AttemptHealthRegistry {
    inner: Arc<Mutex<BTreeMap<u64, AttemptHealth>>>,
}

impl AttemptHealthRegistry {
    pub(super) fn register(
        &self,
        id: u64,
        kind: AttemptKind,
        range: ByteRange,
        rolling_window: Duration,
    ) -> AttemptHealthGuard {
        let now = Instant::now();
        let mut states = self.inner.lock().expect("attempt health lock poisoned");
        states.retain(|_, state| {
            state.active
                || state
                    .ended_at
                    .is_some_and(|ended| now.duration_since(ended) < rolling_window)
        });
        states.insert(
            id,
            AttemptHealth {
                kind,
                range,
                started: now,
                last_progress: now,
                received: 0,
                rolling_window,
                samples: VecDeque::from([(now, 0)]),
                active: true,
                ended_at: None,
                succeeded: false,
            },
        );
        AttemptHealthGuard {
            id,
            registry: self.clone(),
            completed: false,
        }
    }

    fn progress(&self, id: u64, bytes: u64) {
        let now = Instant::now();
        let mut states = self.inner.lock().expect("attempt health lock poisoned");
        let Some(state) = states.get_mut(&id) else {
            return;
        };
        state.received += bytes;
        state.last_progress = now;
        state.samples.push_back((now, state.received));
        while state.samples.len() > 1
            && now.duration_since(state.samples.front().expect("sample exists").0)
                > state.rolling_window
        {
            state.samples.pop_front();
        }
    }

    fn finish(&self, id: u64, succeeded: bool) {
        let now = Instant::now();
        if let Some(state) = self
            .inner
            .lock()
            .expect("attempt health lock poisoned")
            .get_mut(&id)
        {
            state.active = false;
            state.ended_at = Some(now);
            state.succeeded = succeeded;
        }
    }

    #[cfg(test)]
    pub(super) fn active_count(&self) -> usize {
        self.inner
            .lock()
            .expect("attempt health lock poisoned")
            .values()
            .filter(|state| state.active)
            .count()
    }

    pub(super) fn hedge_decision(&self, target_id: u64, policy: &TransferPolicy) -> HedgeDecision {
        let now = Instant::now();
        let mut states = self.inner.lock().expect("attempt health lock poisoned");
        states.retain(|_, state| {
            state.active
                || state
                    .ended_at
                    .is_some_and(|ended| now.duration_since(ended) < policy.stalled_for)
        });
        let Some(target) = states.get(&target_id) else {
            return HedgeDecision::None;
        };
        if !target.active || now.duration_since(target.started) < policy.hedge_warmup {
            return HedgeDecision::None;
        }

        let target_rate = target.rate_at(now);
        let remaining = target.range.len().saturating_sub(target.received);
        let predicted = if target_rate > 0.0 {
            Duration::from_secs_f64(remaining as f64 / target_rate)
        } else {
            Duration::MAX
        };
        let stopped = now.duration_since(target.last_progress) >= policy.stalled_for;
        let dribbling = target_rate > 0.0
            && target_rate <= policy.max_hedge_rate
            && predicted >= policy.pathological_remaining;
        let pathological = stopped || dribbling;
        if !pathological {
            return HedgeDecision::None;
        }

        let healthy_peer = states.iter().any(|(id, peer)| {
            if *id == target_id {
                return false;
            }
            let recently_healthy = if peer.active {
                now.duration_since(peer.last_progress) < policy.stalled_for
            } else {
                peer.succeeded
                    && peer
                        .ended_at
                        .is_some_and(|ended| now.duration_since(ended) < policy.stalled_for)
            };
            if !recently_healthy {
                return false;
            }
            let peer_rate = peer.rate_at(now);
            peer_rate >= 128_000.0 && peer_rate >= target_rate * 3.0
        });
        if healthy_peer {
            HedgeDecision::Localized
        } else if states.len() == 1 {
            HedgeDecision::Ambiguous
        } else {
            HedgeDecision::None
        }
    }
}

#[derive(Debug)]
struct AttemptHealth {
    #[allow(dead_code)]
    kind: AttemptKind,
    range: ByteRange,
    started: Instant,
    last_progress: Instant,
    received: u64,
    rolling_window: Duration,
    samples: VecDeque<(Instant, u64)>,
    active: bool,
    ended_at: Option<Instant>,
    succeeded: bool,
}

impl AttemptHealth {
    fn rate_at(&self, now: Instant) -> f64 {
        let Some((sample_time, sample_bytes)) = self.samples.front().copied() else {
            return 0.0;
        };
        let elapsed = now.duration_since(sample_time).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        self.received.saturating_sub(sample_bytes) as f64 / elapsed
    }
}

pub(super) struct AttemptHealthGuard {
    id: u64,
    registry: AttemptHealthRegistry,
    completed: bool,
}

impl AttemptHealthGuard {
    pub(super) fn progress(&self, bytes: u64) {
        self.registry.progress(self.id, bytes);
    }

    pub(super) fn complete(&mut self) {
        self.registry.finish(self.id, true);
        self.completed = true;
    }
}

impl Drop for AttemptHealthGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.registry.finish(self.id, false);
        }
    }
}
