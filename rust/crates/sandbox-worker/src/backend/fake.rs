//! Deterministic in-process backend for contract tests (labeled as such in
//! evidence reports — fake-backend runs are contract tests, not L2 evidence).
//!
//! Tracks `VmObservedState` in memory, gives conclusive waitid-style
//! termination evidence, and consults a shared `FaultInjector` so integration
//! tests can crash the flow at each effect boundary (T11 L1 crash/reconcile).

use super::ReconcileDecision;
use agentkit_sandbox_contracts::api::{
    BackendCapabilities, CapabilitySupport, ProcessIdentity, SandboxBackend, TerminationEvidence,
    VerifiedLaunchSpec, VmHandle, VmObservedState, VmPhysState,
};
use agentkit_sandbox_contracts::error::{SandboxError, SandboxErrorCode};
use agentkit_sandbox_contracts::ids::{ArtifactDigest, SandboxId};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// Crash points integration tests can arm (T11 L1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    PreProvision,
    PostProvision,
    PostLaunch,
    PostSpawn,
    PostExit,
}

/// Shared fault switchboard: the fake backend AND the test guest simulator
/// consult it before their respective effects.
#[derive(Debug, Default)]
pub struct FaultInjector {
    armed: Mutex<HashMap<String, HashSet<FaultPoint>>>,
}

impl FaultInjector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn arm(&self, sandbox: &SandboxId, point: FaultPoint) {
        self.armed
            .lock()
            .expect("fault injector lock")
            .entry(sandbox.as_str().to_string())
            .or_default()
            .insert(point);
    }

    pub fn clear(&self, sandbox: &SandboxId) {
        self.armed
            .lock()
            .expect("fault injector lock")
            .remove(sandbox.as_str());
    }

    /// True when `point` is armed for `sandbox`; used by backend + sim guest.
    #[must_use]
    pub fn hit(&self, sandbox: &SandboxId, point: FaultPoint) -> bool {
        self.armed
            .lock()
            .expect("fault injector lock")
            .get(sandbox.as_str())
            .is_some_and(|set| set.contains(&point))
    }

    #[must_use]
    pub fn fault_error(sandbox: &SandboxId, point: FaultPoint) -> SandboxError {
        SandboxError::new(
            SandboxErrorCode::Unavailable,
            format!("injected fault at {point:?} for sandbox {sandbox}"),
        )
    }
}

#[derive(Debug)]
struct FakeVm {
    observed: VmObservedState,
    launched: bool,
    terminated: bool,
    /// Test hook: force the next terminate() to report inconclusive.
    inconclusive_next: bool,
    /// Test hook: every terminate() reports inconclusive (unwedgeable VM).
    inconclusive_sticky: bool,
}

/// In-memory fake hypervisor.
#[derive(Debug, Default)]
pub struct FakeBackend {
    inner: Mutex<HashMap<String, FakeVm>>,
    next_pid: AtomicU32,
    faults: Option<Arc<FaultInjector>>,
}

impl FakeBackend {
    /// Construct with a shared fault injector.
    #[must_use]
    pub fn with_faults(faults: Arc<FaultInjector>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_pid: AtomicU32::new(100),
            faults: Some(faults),
        }
    }

    fn hit(&self, sandbox: &SandboxId, point: FaultPoint) -> bool {
        self.faults.as_ref().is_some_and(|f| f.hit(sandbox, point))
    }

    /// Arm the next terminate for `sandbox` to report inconclusive evidence
    /// (tests the quarantine path, S14).
    pub fn make_terminate_inconclusive(&self, sandbox: &SandboxId) {
        let key = sandbox.as_str().to_string();
        if let Some(vm) = self.inner.lock().expect("fake backend lock").get_mut(&key) {
            vm.inconclusive_next = true;
        }
    }

    /// Arm every terminate for `sandbox` to report inconclusive: models an
    /// unwedgeable hypervisor where termination can never be proven.
    pub fn make_terminate_permanently_inconclusive(&self, sandbox: &SandboxId) {
        let key = sandbox.as_str().to_string();
        if let Some(vm) = self.inner.lock().expect("fake backend lock").get_mut(&key) {
            vm.inconclusive_sticky = true;
        }
    }

    /// Worker-side reconcile probe mirroring the fc contract.
    #[must_use]
    pub fn reconcile_decision(&self, sandbox: &SandboxId) -> ReconcileDecision {
        let key = sandbox.as_str().to_string();
        let map = self.inner.lock().expect("fake backend lock");
        match map.get(&key) {
            Some(vm) if vm.launched && !vm.terminated => ReconcileDecision::Adopt,
            Some(vm) if vm.terminated => ReconcileDecision::Kill,
            Some(_) | None => ReconcileDecision::Quarantine,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[async_trait]
impl SandboxBackend for FakeBackend {
    async fn probe(&self) -> Result<BackendCapabilities, SandboxError> {
        let mut features = BTreeMap::new();
        for f in ["exec", "fs", "staging", "quarantine", "reconcile"] {
            features.insert(
                f.to_string(),
                CapabilitySupport {
                    supported: true,
                    enforced: true,
                    evidence: Some(format!("sandbox-worker/tests/{f}.rs")),
                },
            );
        }
        Ok(BackendCapabilities {
            backend: "fake".into(),
            hardware_virtualization: true,
            kvm_device: true,
            vsock: true,
            cgroups_v2: true,
            jailer: None,
            firecracker_version: None,
            features,
            notes: vec!["fake backend: contract tests only, not L2 evidence".into()],
        })
    }

    async fn provision(&self, spec: &VerifiedLaunchSpec) -> Result<VmHandle, SandboxError> {
        if self.hit(&spec.sandbox_id, FaultPoint::PreProvision) {
            return Err(FaultInjector::fault_error(
                &spec.sandbox_id,
                FaultPoint::PreProvision,
            ));
        }
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst) + 4000;
        let handle = VmHandle {
            sandbox_id: spec.sandbox_id.clone(),
            instance_generation: spec.instance_generation,
            backend: "fake".into(),
            hypervisor_pid: Some(pid),
        };
        let identity = ProcessIdentity {
            pid,
            start_time_ms: now_ms(),
            uid: unsafe { libc::getuid() },
            exe_digest: ArtifactDigest::of_bytes(b"fake-vmm"),
        };
        self.inner.lock().expect("fake backend lock").insert(
            spec.sandbox_id.as_str().to_string(),
            FakeVm {
                observed: VmObservedState {
                    state: VmPhysState::NotCreated,
                    hypervisor_pid: Some(pid),
                    process_identity: Some(identity),
                    last_observed_ms: now_ms(),
                },
                launched: false,
                terminated: false,
                inconclusive_next: false,
                inconclusive_sticky: false,
            },
        );
        if self.hit(&spec.sandbox_id, FaultPoint::PostProvision) {
            return Err(FaultInjector::fault_error(
                &spec.sandbox_id,
                FaultPoint::PostProvision,
            ));
        }
        Ok(handle)
    }

    async fn launch(&self, handle: &VmHandle) -> Result<VmObservedState, SandboxError> {
        let key = handle.sandbox_id.as_str().to_string();
        let observed = {
            let mut map = self.inner.lock().expect("fake backend lock");
            let vm = map
                .get_mut(&key)
                .ok_or_else(|| SandboxError::new(SandboxErrorCode::NotFound, "unknown fake vm"))?;
            vm.observed.state = VmPhysState::Running;
            vm.observed.last_observed_ms = now_ms();
            vm.launched = true;
            vm.observed.clone()
        };
        if self.hit(&handle.sandbox_id, FaultPoint::PostLaunch) {
            return Err(FaultInjector::fault_error(
                &handle.sandbox_id,
                FaultPoint::PostLaunch,
            ));
        }
        Ok(observed)
    }

    async fn terminate(&self, handle: &VmHandle) -> Result<TerminationEvidence, SandboxError> {
        let key = handle.sandbox_id.as_str().to_string();
        let mut map = self.inner.lock().expect("fake backend lock");
        let vm = map
            .get_mut(&key)
            .ok_or_else(|| SandboxError::new(SandboxErrorCode::NotFound, "unknown fake vm"))?;
        let conclusive = !vm.inconclusive_next && !vm.inconclusive_sticky;
        vm.inconclusive_next = false;
        if conclusive {
            vm.terminated = true;
            vm.observed.state = VmPhysState::Stopped;
        } else {
            // Termination unverified: the process fate is unknown, not
            // "stopped". Nothing may claim the VM is gone (S14).
            vm.observed.state = VmPhysState::Unknown;
        }
        vm.observed.last_observed_ms = now_ms();
        Ok(TerminationEvidence {
            method: "waitid".into(),
            observed_ms: now_ms(),
            identity: vm.observed.process_identity.clone(),
            conclusive,
        })
    }

    async fn inspect(&self, handle: &VmHandle) -> Result<VmObservedState, SandboxError> {
        let key = handle.sandbox_id.as_str().to_string();
        let map = self.inner.lock().expect("fake backend lock");
        map.get(&key)
            .map(|v| v.observed.clone())
            .ok_or_else(|| SandboxError::new(SandboxErrorCode::NotFound, "unknown fake vm"))
    }
}
