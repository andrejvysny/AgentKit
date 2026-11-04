//! One deadline worker per store; registrations cancel synchronously on drop.
use crate::{Result, StoreError, actor::Worker};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

type Key = (Instant, u64);
struct State {
    stopped: bool,
    next: u64,
    waiters: BTreeMap<Key, oneshot::Sender<()>>,
    releases: VecDeque<Release>,
}
enum Release {
    Worker(Arc<Worker>),
    Barrier(oneshot::Sender<()>),
}
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}
pub(crate) struct Deadlines {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}
pub(crate) struct Registration {
    shared: Arc<Shared>,
    key: Key,
}
pub(crate) struct ReleaseQueue(Arc<Shared>);
impl ReleaseQueue {
    pub(crate) fn release(&self, worker: Arc<Worker>) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.releases.push_back(Release::Worker(worker));
        self.0.wake.notify_one();
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.waiters.remove(&self.key);
        self.shared.wake.notify_one();
    }
}
impl Deadlines {
    pub(crate) fn new() -> Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                stopped: false,
                next: 0,
                waiters: BTreeMap::new(),
                releases: VecDeque::new(),
            }),
            wake: Condvar::new(),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("agentkit-store-deadlines".into())
            .spawn(move || run(worker))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
    pub(crate) fn register(
        &self,
        duration: Duration,
    ) -> Result<(Registration, oneshot::Receiver<()>)> {
        let instant = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| StoreError::InvalidInput("transaction timeout too large".into()))?;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.stopped {
            return Err(StoreError::Closed);
        }
        let id = state.next;
        state.next = state.next.checked_add(1).ok_or_else(|| {
            StoreError::InvalidInput("transaction deadline counter exhausted".into())
        })?;
        let key = (instant, id);
        let (sender, receiver) = oneshot::channel();
        state.waiters.insert(key, sender);
        self.shared.wake.notify_one();
        Ok((
            Registration {
                shared: self.shared.clone(),
                key,
            },
            receiver,
        ))
    }
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.shared.state.lock().unwrap().waiters.len()
    }
    pub(crate) fn release_queue(&self) -> ReleaseQueue {
        ReleaseQueue(self.shared.clone())
    }
    pub(crate) async fn flush_releases(&self) -> Result<()> {
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.stopped {
                return Err(StoreError::Closed);
            }
            state.releases.push_back(Release::Barrier(sender));
            self.shared.wake.notify_one();
        }
        receiver.await.map_err(|_| StoreError::Closed)
    }
}
impl Drop for Deadlines {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.stopped = true;
        self.shared.wake.notify_one();
        drop(state);
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
fn run(shared: Arc<Shared>) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    loop {
        if state.stopped {
            break;
        }
        if let Some(release) = state.releases.pop_front() {
            drop(state);
            match release {
                Release::Worker(worker) => drop(worker),
                Release::Barrier(sender) => {
                    let _ = sender.send(());
                }
            }
            state = shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            continue;
        }
        match state.waiters.first_key_value().map(|(key, _)| *key) {
            None => {
                state = shared
                    .wake
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner())
            }
            Some(key) if key.0 <= Instant::now() => {
                if let Some(sender) = state.waiters.remove(&key) {
                    let _ = sender.send(());
                }
            }
            Some(key) => {
                let duration = key.0.saturating_duration_since(Instant::now());
                state = shared
                    .wake
                    .wait_timeout(state, duration)
                    .unwrap_or_else(|error| error.into_inner())
                    .0;
            }
        }
    }
}
