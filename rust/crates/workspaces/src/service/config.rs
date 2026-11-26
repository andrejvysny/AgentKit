//! What the host chooses for the workspace service. Every value has the
//! spec's default; the host may pass its own.

use crate::limits::{LEASE_WAIT_MS, OutputLimit, ResourceLimits};
use crate::network::{DEFAULT_GRANTS, HostPort};

/// Settings of one [`super::WorkspaceService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Image every new generation starts from, by digest.
    pub image_digest: String,
    /// Machine resources. `disk_gib` is also the soft quota the service
    /// checks before each mutating operation.
    pub limits: ResourceLimits,
    /// How long a mutating call waits for the workspace lease before it
    /// answers `sandbox_busy` (20 s).
    pub lease_wait_ms: u64,
    /// Lease time-to-live. Once an operation's row exists, the worker checks
    /// its lease before every step that may wait on the backend and renews
    /// it when less than the longest such stretch is left
    /// ([`ServiceConfig::renewal_gap_ms`]), and renews it right before the
    /// dispatch and before every fenced write that ends the operation. The
    /// TTL must be longer than that stretch ([`ServiceConfig::validate`]),
    /// or a worker could lose the lease while it waits on the backend and
    /// leave the outcome and the fence to the next holder. Before the row
    /// exists, and in owner actions, the lease is renewed right before each
    /// fenced write; one that ran out answers `LeaseLost` as a refusal, with
    /// nothing dispatched.
    pub lease_ttl_ms: i64,
    /// Pause between polls (lease wait, observe).
    pub poll_interval_ms: u64,
    /// Longest the service waits for one backend call (dispatch, observe,
    /// cancel, stop, ensure). A dispatch that does not answer in time is
    /// a lost ack: uncertain.
    pub call_timeout_ms: u64,
    /// After a cancel, how long the service waits for the receipt before
    /// the outcome is uncertain (2 s). The watch keeps renewing the lease
    /// meanwhile, so the grace does not count toward the renewal gap.
    pub cancel_grace_ms: u64,
    /// A machine unused this long is stopped by `evict_idle`.
    pub idle_stop_ms: i64,
    /// Most machines this service keeps running at once; the least
    /// recently used idle one is stopped to make room, a busy one never.
    pub max_active_machines: usize,
    /// What a caller sees of one command's output: the last lines and
    /// bytes (2,000 lines or 50 KB). Also the byte cap of one file read.
    pub model_output: OutputLimit,
    /// Grants every new workspace is created with.
    pub default_grants: Vec<HostPort>,
    /// The host this service runs on, stable across restarts. Lease holders
    /// carry it with the boot id the service mints at construction, and
    /// startup recovery breaks only expired leases and leases of earlier
    /// boots of this host. One live service per host id is assumed.
    pub host_id: String,
}

/// A [`ServiceConfig`] the service refuses to run with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceConfigError {
    /// The lease would expire during the longest stretch between two lease
    /// checks, so a worker could lose it while it waits on the backend.
    #[error("lease TTL {ttl_ms} ms must exceed the longest wait between renewals, {gap_ms} ms")]
    LeaseTtlTooShort {
        /// The configured TTL.
        ttl_ms: i64,
        /// Two call timeouts and one poll interval.
        gap_ms: u64,
    },
    /// The poll interval or the call timeout is zero.
    #[error("poll_interval_ms and call_timeout_ms must be positive")]
    ZeroInterval,
}

impl ServiceConfig {
    /// The spec's defaults (desktop column) for `image_digest`.
    pub fn new(image_digest: impl Into<String>) -> Self {
        Self {
            image_digest: image_digest.into(),
            limits: ResourceLimits::DEFAULT,
            lease_wait_ms: LEASE_WAIT_MS,
            lease_ttl_ms: 60_000,
            poll_interval_ms: 250,
            call_timeout_ms: 20_000,
            cancel_grace_ms: 2_000,
            idle_stop_ms: 600_000,
            max_active_machines: 2,
            model_output: OutputLimit::MODEL,
            default_grants: DEFAULT_GRANTS
                .iter()
                .map(|(host, port)| HostPort::new(host, *port).expect("default grants are valid"))
                .collect(),
            host_id: "local".to_owned(),
        }
    }

    /// The longest stretch between two lease checks of a worker: a watch
    /// step (a cancel and an observe, each bounded by the call timeout, then
    /// one poll), a dispatch, or a stop. Every other wait is shorter.
    pub fn renewal_gap_ms(&self) -> u64 {
        self.call_timeout_ms
            .saturating_mul(2)
            .saturating_add(self.poll_interval_ms)
    }

    /// Refuse settings under which a worker could lose its lease while it
    /// waits on the backend.
    pub fn validate(&self) -> Result<(), ServiceConfigError> {
        if self.poll_interval_ms == 0 || self.call_timeout_ms == 0 {
            return Err(ServiceConfigError::ZeroInterval);
        }
        let gap_ms = self.renewal_gap_ms();
        let ttl = u64::try_from(self.lease_ttl_ms).unwrap_or(0);
        if ttl <= gap_ms {
            return Err(ServiceConfigError::LeaseTtlTooShort {
                ttl_ms: self.lease_ttl_ms,
                gap_ms,
            });
        }
        Ok(())
    }

    /// The soft disk quota in bytes.
    pub fn quota_bytes(&self) -> u64 {
        self.limits.disk_gib.saturating_mul(1024 * 1024 * 1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_leave_room_for_the_longest_wait() {
        let config = ServiceConfig::new("sha256:x");
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(config.renewal_gap_ms(), 40_250);
        let short = ServiceConfig {
            lease_ttl_ms: 40_250,
            ..config.clone()
        };
        assert_eq!(
            short.validate(),
            Err(ServiceConfigError::LeaseTtlTooShort {
                ttl_ms: 40_250,
                gap_ms: 40_250
            })
        );
        let zero = ServiceConfig {
            poll_interval_ms: 0,
            ..config
        };
        assert_eq!(zero.validate(), Err(ServiceConfigError::ZeroInterval));
    }
}
