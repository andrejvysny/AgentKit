//! Hit merging and include-domain filtering shared by all adapters.

use std::collections::HashSet;

use url::Url;

use crate::backend::{BackendError, SearchHit};
use crate::normalize::normalize_domain;

/// Any invalid entry fails the request: dropping it would widen the search.
pub(super) fn normalize_domains(raw: &[String]) -> Result<Vec<String>, BackendError> {
    raw.iter()
        .map(|d| {
            normalize_domain(d).ok_or_else(|| {
                BackendError::InvalidRequest("include_domains entry is not a host name".into())
            })
        })
        .collect()
}

pub(super) fn domain_allowed(url: &Url, domains: &[String]) -> bool {
    if domains.is_empty() {
        return true;
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    domains
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// Appends ` site:<d>` when exactly one domain restricts the query.
pub(super) fn with_site_operator(query: &str, domains: &[String]) -> String {
    let mut q = query.to_owned();
    if let [only] = domains {
        q.push_str(" site:");
        q.push_str(only);
    }
    q
}

/// Domain-filtered, URL-deduplicated hit list.
#[derive(Default)]
pub(super) struct HitSet {
    pub hits: Vec<SearchHit>,
    seen: HashSet<String>,
}

impl HitSet {
    pub fn absorb(&mut self, hits: Vec<SearchHit>, domains: &[String]) {
        for hit in hits {
            if domain_allowed(&hit.url, domains) && self.seen.insert(hit.url.to_string()) {
                self.hits.push(hit);
            }
        }
    }
}
