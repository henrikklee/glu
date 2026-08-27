use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub(super) struct RequestCoordinator {
    inner: Arc<Mutex<CoordinatorState>>,
}

#[derive(Debug)]
struct CoordinatorState {
    available: usize,
    next_sequence: u64,
    waiters: BTreeMap<(u64, u64), oneshot::Sender<RequestPermit>>,
}

impl RequestCoordinator {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(CoordinatorState {
                available: limit.max(1),
                next_sequence: 0,
                waiters: BTreeMap::new(),
            })),
        }
    }

    pub(super) async fn acquire(&self, priority: u64) -> Option<RequestPermit> {
        let (sender, receiver) = oneshot::channel();
        let key = {
            let mut state = self
                .inner
                .lock()
                .expect("request coordinator lock poisoned");
            let key = (priority, state.next_sequence);
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.waiters.insert(key, sender);
            dispatch(&mut state, &self.inner);
            key
        };
        let mut cleanup = WaiterCleanup {
            coordinator: self.clone(),
            key,
            armed: true,
        };
        let permit = receiver.await.ok()?;
        cleanup.armed = false;
        Some(permit)
    }

    #[cfg(test)]
    pub(super) fn waiting(&self) -> usize {
        self.inner
            .lock()
            .expect("request coordinator lock poisoned")
            .waiters
            .len()
    }
}

fn dispatch(state: &mut CoordinatorState, coordinator: &Arc<Mutex<CoordinatorState>>) {
    while state.available > 0 {
        let Some((_, sender)) = state.waiters.pop_first() else {
            break;
        };
        state.available -= 1;
        let permit = RequestPermit {
            coordinator: Arc::clone(coordinator),
            armed: true,
        };
        if let Err(mut permit) = sender.send(permit) {
            permit.armed = false;
            state.available += 1;
        }
    }
}

#[derive(Debug)]
pub(super) struct RequestPermit {
    coordinator: Arc<Mutex<CoordinatorState>>,
    armed: bool,
}

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .coordinator
            .lock()
            .expect("request coordinator lock poisoned");
        state.available += 1;
        dispatch(&mut state, &self.coordinator);
    }
}

struct WaiterCleanup {
    coordinator: RequestCoordinator,
    key: (u64, u64),
    armed: bool,
}

impl Drop for WaiterCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .coordinator
            .inner
            .lock()
            .expect("request coordinator lock poisoned");
        state.waiters.remove(&self.key);
        dispatch(&mut state, &self.coordinator.inner);
    }
}
