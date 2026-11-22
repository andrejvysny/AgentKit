//! Ordered backend fallback with per-run budget, per-run cache and a total
//! deadline. All state is instance state; nothing is global.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::backend::{BackendError, BackendId, SearchBackend, SearchRequest, SearchResponse};
use crate::config::{CacheConfig, RunLimits};

/// A successful routed search.
#[derive(Clone, Debug)]
pub struct Routed {
    pub response: SearchResponse,
    pub cached: bool,
    /// Backends tried before the one that answered, in order.
    pub failed: Vec<(BackendId, BackendError)>,
}

#[derive(Clone, Debug)]
pub enum RouterError {
    BudgetExhausted,
    AllFailed(Vec<(BackendId, BackendError)>),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BudgetExhausted => f.write_str("search budget exhausted"),
            Self::AllFailed(_) => f.write_str("all search backends failed"),
        }
    }
}

impl std::error::Error for RouterError {}

type CacheKey = String;

struct RunState {
    searches: u32,
    fetches: u32,
    cache: VecDeque<(CacheKey, Instant, SearchResponse)>,
    last_used: Instant,
}

impl RunState {
    fn new() -> Self {
        Self {
            searches: 0,
            fetches: 0,
            cache: VecDeque::new(),
            last_used: Instant::now(),
        }
    }
}

enum Admission {
    Cached(SearchResponse),
    Proceed,
}

pub struct BackendRouter {
    backends: Vec<Arc<dyn SearchBackend>>,
    limits: RunLimits,
    cache: CacheConfig,
    total_timeout: Duration,
    runs: Mutex<HashMap<String, RunState>>,
}

fn cache_key(req: &SearchRequest) -> CacheKey {
    let mut domains: Vec<String> = req
        .include_domains
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    domains.sort();
    format!(
        "{}\u{1f}{}\u{1f}{:?}\u{1f}{}\u{1f}{}",
        req.query.trim().to_lowercase(),
        req.max_results,
        req.freshness,
        domains.join(","),
        req.language.as_deref().unwrap_or("")
    )
}

impl BackendRouter {
    pub fn new(
        backends: Vec<Arc<dyn SearchBackend>>,
        limits: RunLimits,
        cache: CacheConfig,
        total_timeout: Duration,
    ) -> Self {
        Self {
            backends,
            limits,
            cache,
            total_timeout,
            runs: Mutex::new(HashMap::new()),
        }
    }

    pub fn backend(&self, id: BackendId) -> Option<Arc<dyn SearchBackend>> {
        self.backends.iter().find(|b| b.id() == id).cloned()
    }

    /// Consumes one fetch from the run budget; false when it is used up.
    pub fn try_consume_fetch(&self, run_id: &str) -> bool {
        let mut runs = self.lock();
        let run = self.run_entry(&mut runs, run_id);
        if run.fetches >= self.limits.fetches_per_run {
            return false;
        }
        run.fetches += 1;
        true
    }

    pub async fn search(
        &self,
        run_id: &str,
        req: &SearchRequest,
        cancel: &CancellationToken,
    ) -> Result<Routed, RouterError> {
        let deadline = Instant::now() + self.total_timeout;
        let key = cache_key(req);
        match self.admit(run_id, &key)? {
            Admission::Cached(response) => {
                return Ok(Routed {
                    response,
                    cached: true,
                    failed: vec![],
                });
            }
            Admission::Proceed => {}
        }
        let mut failed = Vec::new();
        for backend in &self.backends {
            let Some(left) = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
            else {
                break;
            };
            let outcome = match tokio::time::timeout(left, backend.search(req, cancel)).await {
                Ok(result) => result,
                Err(_) => Err(BackendError::Timeout),
            };
            match outcome {
                Ok(response) => {
                    self.store(run_id, key, &response);
                    return Ok(Routed {
                        response,
                        cached: false,
                        failed,
                    });
                }
                Err(error) => {
                    let stop = error.stops_fallback();
                    failed.push((backend.id(), error));
                    if stop {
                        break;
                    }
                }
            }
        }
        Err(RouterError::AllFailed(failed))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, RunState>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Gets or creates the run state, evicting the least recently used run
    /// when a new one would exceed `max_runs`.
    fn run_entry<'a>(
        &self,
        runs: &'a mut HashMap<String, RunState>,
        run_id: &str,
    ) -> &'a mut RunState {
        if !runs.contains_key(run_id) {
            while runs.len() >= self.cache.max_runs.max(1) {
                let oldest = runs
                    .iter()
                    .min_by_key(|(_, state)| state.last_used)
                    .map(|(id, _)| id.clone());
                match oldest {
                    Some(id) => runs.remove(&id),
                    None => break,
                };
            }
        }
        let run = runs.entry(run_id.to_owned()).or_insert_with(RunState::new);
        run.last_used = Instant::now();
        run
    }

    fn admit(&self, run_id: &str, key: &CacheKey) -> Result<Admission, RouterError> {
        let mut runs = self.lock();
        let run = self.run_entry(&mut runs, run_id);
        let ttl = self.cache.ttl;
        run.cache.retain(|(_, at, _)| at.elapsed() < ttl);
        if let Some(pos) = run.cache.iter().position(|(k, _, _)| k == key)
            && let Some(entry) = run.cache.remove(pos)
        {
            let response = entry.2.clone();
            run.cache.push_back(entry);
            return Ok(Admission::Cached(response));
        }
        if run.searches >= self.limits.searches_per_run {
            return Err(RouterError::BudgetExhausted);
        }
        run.searches += 1;
        Ok(Admission::Proceed)
    }

    fn store(&self, run_id: &str, key: CacheKey, response: &SearchResponse) {
        let mut runs = self.lock();
        let run = self.run_entry(&mut runs, run_id);
        run.cache.retain(|(k, _, _)| *k != key);
        run.cache.push_back((key, Instant::now(), response.clone()));
        while run.cache.len() > self.cache.entries_per_run {
            run.cache.pop_front();
        }
    }
}

impl crate::fetch::FetchBudget for BackendRouter {
    fn try_consume_fetch(&self, run_id: &str) -> bool {
        BackendRouter::try_consume_fetch(self, run_id)
    }
}
