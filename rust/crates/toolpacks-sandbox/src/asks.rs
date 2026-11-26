//! What the owner was shown when the guard asked, kept so the tool body can
//! act on exactly that and nothing else (F-14 for exports, the displayed
//! requests for network hosts).
//!
//! The guard does not know the tool call id and an `ApprovalGrant` carries
//! no prompt, so an ask is keyed by `(run, tool, argument hash)` and held
//! here, in the pack. Every ask and every staging gets a number from one
//! counter: a body accepts only an ask created before its own staging,
//! i.e. one a human could have answered before this attempt began. An ask
//! also keeps the answers its staging already carried (that staging's
//! durable port: one immutable set of answers per attempt), so a body can
//! refuse an answer that existed before the ask was raised: such an ask
//! was spent without being shown. An ask is single use: the first body of
//! its key in a later staging takes it, whatever it then decides. The
//! state is in memory only; after a restart (or an eviction) nothing is
//! found and the body fails closed.
//!
//! One record serves every run of the host, so it is bounded: by runs, by
//! asks per run and by asks in all. A run parked on an ask is the least
//! recently touched by nature, so a run's record is dropped by last touch,
//! a run holding no unanswered ask before one that does; the total bound
//! drops the oldest asks of the least recently touched runs.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use agentkit_governance::{ApprovalGrant, DurableExecutionPort};
use agentkit_workspaces::{RequestId, Sha256};

/// Runs whose record is kept.
const MAX_RUNS: usize = 1_024;
/// Unanswered asks kept across all runs. An export ask is at most a few
/// KiB (the card bounds path, title and preview), so the record stays
/// within a few MiB.
const MAX_ASKS: usize = 1_024;
/// Asks kept per run; the oldest is dropped first.
const MAX_ASKS_PER_RUN: usize = 64;
/// Requests remembered as shown per run (four times a workspace's pending
/// requests); a request beyond it is asked about again.
const MAX_SHOWN_PER_RUN: usize = 256;

/// The call shape an ask belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AskKey {
    pub(crate) tool: &'static str,
    pub(crate) args_sha256: String,
}

/// What the owner saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shown {
    /// An export of exactly this content.
    Export { path: String, sha256: Sha256 },
    /// Exactly these pending network requests.
    Network { requests: Vec<RequestId> },
}

/// One ask as the guard raised it. Re-presented verbatim: the same call
/// asking again must park byte-identical request content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ask {
    pub(crate) created: u64,
    pub(crate) reason: String,
    pub(crate) preview: String,
    pub(crate) shown: Shown,
}

/// The answers the raising staging carried.
type Answers = Option<Arc<dyn DurableExecutionPort>>;

/// A stored ask and the answers known when it was raised.
struct Entry {
    ask: Ask,
    answers: Answers,
}

/// An ask a body took: answerable before the body's attempt began.
pub(crate) struct Taken {
    pub(crate) ask: Ask,
    answers: Answers,
}

impl Taken {
    /// True when `grant`, the answer call `call_id` carries, did not exist
    /// when the ask was raised: the owner answered after seeing it.
    pub(crate) fn answered_after_ask(&self, call_id: &str, grant: &ApprovalGrant) -> bool {
        self.answers
            .as_ref()
            .and_then(|answers| answers.approval_grant(call_id))
            .is_none_or(|known| known.request_id != grant.request_id)
    }
}

#[derive(Default)]
struct RunAsks {
    asks: HashMap<AskKey, Entry>,
    shown: HashSet<RequestId>,
    /// The counter when the run's record was last read or written.
    touched: u64,
}

/// What the guard shows: the approvable reason, the card, and what it
/// binds.
pub(crate) struct NewAsk {
    pub(crate) reason: String,
    pub(crate) preview: String,
    pub(crate) shown: Shown,
}

#[derive(Default)]
struct Inner {
    counter: u64,
    runs: HashMap<String, RunAsks>,
}

impl Inner {
    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// The record of `run_id` (created when missing), touched now.
    fn run(&mut self, run_id: &str) -> &mut RunAsks {
        let now = self.next();
        let run = self.runs.entry(run_id.to_owned()).or_default();
        run.touched = now;
        run
    }

    /// The existing record of `run_id`, touched now.
    fn touch(&mut self, run_id: &str) -> Option<&mut RunAsks> {
        let now = self.next();
        let run = self.runs.get_mut(run_id)?;
        run.touched = now;
        Some(run)
    }

    /// The run to drop from first: least recently touched, one without
    /// unanswered asks before one with, `keep` (just written) last.
    fn victim(&self, keep: &str, with_asks: bool) -> Option<String> {
        self.runs
            .iter()
            .filter(|(_, run)| !with_asks || !run.asks.is_empty())
            .min_by_key(|(id, run)| (id.as_str() == keep, !run.asks.is_empty(), run.touched))
            .map(|(id, _)| id.clone())
    }

    /// Restore the bounds on runs and on asks in all after `keep` wrote.
    fn evict(&mut self, keep: &str) {
        while self.runs.len() > MAX_RUNS
            && let Some(victim) = self.victim(keep, false)
        {
            self.runs.remove(&victim);
        }
        while self.runs.values().map(|run| run.asks.len()).sum::<usize>() > MAX_ASKS
            && let Some(victim) = self.victim(keep, true)
            && let Some(run) = self.runs.get_mut(&victim)
        {
            run.drop_oldest();
        }
    }
}

impl RunAsks {
    fn drop_oldest(&mut self) {
        let oldest = self
            .asks
            .iter()
            .min_by_key(|(_, entry)| entry.ask.created)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.asks.remove(&key);
        }
    }
}

/// The pack's asks, shared by its guard and its tools.
#[derive(Default)]
pub(crate) struct Asks {
    inner: Mutex<Inner>,
}

impl Asks {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A number for a staging: asks created after it compare greater.
    pub(crate) fn staging(&self) -> u64 {
        self.lock().next()
    }

    /// The stored ask of `key`, if any.
    pub(crate) fn stored(&self, run_id: &str, key: &AskKey) -> Option<Ask> {
        let mut inner = self.lock();
        let run = inner.touch(run_id)?;
        run.asks.get(key).map(|entry| entry.ask.clone())
    }

    /// Drop the ask of `key`.
    pub(crate) fn forget(&self, run_id: &str, key: &AskKey) {
        if let Some(run) = self.lock().runs.get_mut(run_id) {
            run.asks.remove(key);
        }
    }

    /// Store a new ask for `key`, raised in a staging that carried
    /// `answers`, and return it; an ask already stored for `key` wins and
    /// is returned unchanged (it is never extended after it was shown).
    /// Network requests in it count as shown to the run.
    pub(crate) fn raise(&self, run_id: &str, key: AskKey, ask: NewAsk, answers: Answers) -> Ask {
        let mut inner = self.lock();
        let created = inner.next();
        let run = inner.run(run_id);
        if let Some(existing) = run.asks.get(&key) {
            return existing.ask.clone();
        }
        let NewAsk {
            reason,
            preview,
            shown,
        } = ask;
        if let Shown::Network { requests } = &shown
            && run.shown.len() + requests.len() <= MAX_SHOWN_PER_RUN
        {
            run.shown.extend(requests.iter().cloned());
        }
        if run.asks.len() >= MAX_ASKS_PER_RUN {
            run.drop_oldest();
        }
        let ask = Ask {
            created,
            reason,
            preview,
            shown,
        };
        let entry = Entry {
            ask: ask.clone(),
            answers,
        };
        run.asks.insert(key, entry);
        inner.evict(run_id);
        ask
    }

    /// Requests not yet shown to this run.
    pub(crate) fn unshown(&self, run_id: &str, requests: Vec<RequestId>) -> Vec<RequestId> {
        let inner = self.lock();
        let Some(run) = inner.runs.get(run_id) else {
            return requests;
        };
        requests
            .into_iter()
            .filter(|id| !run.shown.contains(id))
            .collect()
    }

    /// Take the ask of `key` (single use). It is returned only when it was
    /// created before `staged`; a younger one was never answerable in this
    /// attempt, so its requests count as not shown again.
    pub(crate) fn take(&self, run_id: &str, key: &AskKey, staged: u64) -> Option<Taken> {
        let mut inner = self.lock();
        let run = inner.touch(run_id)?;
        let Entry { ask, answers } = run.asks.remove(key)?;
        if ask.created < staged {
            return Some(Taken { ask, answers });
        }
        if let Shown::Network { requests } = &ask.shown {
            for id in requests {
                run.shown.remove(id);
            }
        }
        None
    }

    /// Count `requests` as not shown to the run again: the card that showed
    /// them got no answer that may grant them.
    pub(crate) fn unshow(&self, run_id: &str, requests: &[RequestId]) {
        if let Some(run) = self.lock().touch(run_id) {
            for id in requests {
                run.shown.remove(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(args: &str) -> AskKey {
        AskKey {
            tool: "sandbox_run",
            args_sha256: args.into(),
        }
    }

    fn card(text: &str, ids: &[&str]) -> NewAsk {
        NewAsk {
            reason: text.into(),
            preview: text.into(),
            shown: Shown::Network {
                requests: ids.iter().map(|id| RequestId::new(*id)).collect(),
            },
        }
    }

    #[test]
    fn an_ask_counts_only_for_a_later_staging_and_only_once() {
        let asks = Asks::default();
        let before = asks.staging();
        let first = asks.raise("r", key("a"), card("why", &["q1"]), None);
        let again = asks.raise("r", key("a"), card("other", &["q2"]), None);
        assert_eq!(first, again, "a stored ask is never extended");
        assert!(asks.unshown("r", vec![RequestId::new("q1")]).is_empty());
        let after = asks.staging();
        let young = asks.take("r", &key("a"), before);
        assert!(young.is_none(), "younger than the staging");
        let unshown = asks.unshown("r", vec![RequestId::new("q1")]);
        assert_eq!(unshown.len(), 1, "never shown");
        asks.raise("r", key("a"), card("why", &["q1"]), None);
        let later = asks.staging();
        assert!(after < later);
        let taken = asks.take("r", &key("a"), later).expect("answerable");
        let grant = ApprovalGrant {
            request_id: "hreq:r:c1".into(),
            args_sha256: "a".into(),
            uncertainty_generation: None,
            context_sha256: None,
            uncertainty_card: false,
        };
        assert!(
            taken.answered_after_ask("c1", &grant),
            "nothing was known then"
        );
        assert!(asks.take("r", &key("a"), later).is_none(), "single use");
    }

    #[test]
    fn runs_and_asks_are_bounded() {
        let asks = Asks::default();
        for run in 0..(MAX_RUNS + 1) {
            asks.raise(&format!("r{run}"), key("a"), card("", &[]), None);
        }
        let oldest = asks.stored("r0", &key("a"));
        assert!(
            oldest.is_none(),
            "the least recently touched run is dropped"
        );
        for n in 0..(MAX_ASKS_PER_RUN + 1) {
            asks.raise("big", key(&n.to_string()), card("", &[]), None);
        }
        assert!(
            asks.stored("big", &key("0")).is_none(),
            "the oldest ask is dropped"
        );
        assert!(asks.stored("big", &key("64")).is_some());
    }

    #[test]
    fn reading_a_run_keeps_it_over_runs_touched_earlier() {
        let asks = Asks::default();
        for run in 0..MAX_RUNS {
            asks.raise(&format!("r{run}"), key("a"), card("", &[]), None);
        }
        assert!(asks.stored("r0", &key("a")).is_some(), "re-presented");
        asks.raise("new", key("a"), card("", &[]), None);
        assert!(asks.stored("r0", &key("a")).is_some(), "touched");
        assert!(asks.stored("r1", &key("a")).is_none(), "touched earliest");
    }

    #[test]
    fn a_parked_runs_ask_outlives_runs_without_unanswered_asks() {
        let asks = Asks::default();
        asks.raise("parked", key("a"), card("", &[]), None);
        for run in 0..(MAX_RUNS + 8) {
            let id = format!("r{run}");
            asks.raise(&id, key("a"), card("", &[]), None);
            let later = asks.staging();
            assert!(asks.take(&id, &key("a"), later).is_some());
        }
        assert_eq!(asks.lock().runs.len(), MAX_RUNS);
        assert!(asks.stored("parked", &key("a")).is_some());
    }

    #[test]
    fn asks_in_all_are_bounded_oldest_touched_runs_first() {
        let asks = Asks::default();
        let runs = MAX_ASKS / MAX_ASKS_PER_RUN + 2;
        for run in 0..runs {
            for n in 0..MAX_ASKS_PER_RUN {
                let id = format!("r{run}");
                asks.raise(&id, key(&n.to_string()), card("", &[]), None);
            }
        }
        let total: usize = asks.lock().runs.values().map(|r| r.asks.len()).sum();
        assert_eq!(total, MAX_ASKS, "a hard bound");
        for run in [0, 1] {
            let kept = asks.lock().runs[&format!("r{run}")].asks.len();
            assert_eq!(kept, 0, "r{run} touched earliest");
        }
        let newest = format!("r{}", runs - 1);
        for n in 0..MAX_ASKS_PER_RUN {
            assert!(asks.stored(&newest, &key(&n.to_string())).is_some());
        }
    }

    #[test]
    fn an_unanswered_card_shows_its_requests_again() {
        let asks = Asks::default();
        asks.raise("r", key("a"), card("why", &["q1"]), None);
        assert!(asks.unshown("r", vec![RequestId::new("q1")]).is_empty());
        asks.unshow("r", &[RequestId::new("q1")]);
        assert_eq!(asks.unshown("r", vec![RequestId::new("q1")]).len(), 1);
    }
}
