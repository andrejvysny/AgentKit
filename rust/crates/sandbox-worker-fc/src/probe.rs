//! Host capability probe (§12.1). Fail-closed by construction: every
//! requirement is checked and surfaced; the worker refuses hostile
//! workloads when any hard requirement is missing (S05, S19).

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentkit_sandbox_contracts::api::{BackendCapabilities, CapabilitySupport};

use crate::{FC_MAX_MAJOR, FC_MIN_VERSION, FcError};

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub fc_bin: PathBuf,
    pub jailer_bin: PathBuf,
    pub cgroup_root: Option<PathBuf>,
    pub pinned_fc_version: Option<String>,
}

/// Parse `Firecracker v1.8.1` style version output.
pub fn parse_fc_version(out: &str) -> Option<(u32, u32, u32)> {
    let v = out.split_whitespace().find(|w| {
        w.starts_with('v') && w[1..].chars().next().is_some_and(|c| c.is_ascii_digit())
    })?;
    let num = |p: &str| -> Option<u32> {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    };
    let mut it = v[1..].split('.');
    let a = num(it.next()?)?;
    let b = num(it.next()?)?;
    let c = match it.next() {
        Some(p) => num(p)?,
        None => 0,
    };
    Some((a, b, c))
}

fn version_ok(v: (u32, u32, u32)) -> bool {
    v.0 == FC_MAX_MAJOR && v >= FC_MIN_VERSION
}

/// KVM device readable/writable? (open O_RDWR — what FC needs).
pub fn kvm_ok() -> bool {
    let fd = unsafe { libc::open(c"/dev/kvm".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    // KVM_GET_API_VERSION == 0xAE00
    let api = unsafe { libc::ioctl(fd, 0xAE00 as libc::c_ulong) };
    unsafe { libc::close(fd) };
    api >= 0
}

/// cgroup v2 unified mount present at the given delegation root (writable)?
pub fn cgroup_v2_writable(root: &Path) -> bool {
    // statfs f_type == CGROUP2_SUPER_MAGIC (0x63673265)
    let c_path = match std::ffi::CString::new(root.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated C string; st is writable.
    let ok =
        unsafe { libc::statfs(c_path.as_ptr(), &mut st) } == 0 && st.f_type as i64 == 0x6367_3265;
    ok && std::fs::metadata(root)
        .map(|m| m.mode() & 0o200 != 0)
        .unwrap_or(false)
}

fn exe_version(bin: &Path) -> Result<String, FcError> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .map_err(|e| FcError::Probe(format!("{} --version: {e}", bin.display())))?;
    if !out.status.success() {
        return Err(FcError::Probe(format!(
            "{} exited {}",
            bin.display(),
            out.status
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.len() > 4096 {
        return Err(FcError::Probe("version output oversized".into()));
    }
    Ok(text)
}

/// Full probe. Hard failures return `Err(Probe)`; the worker turns that
/// into `sandbox_isolation_unavailable` on admission.
pub fn probe(cfg: &ProbeConfig) -> Result<BackendCapabilities, FcError> {
    let mut notes = Vec::new();
    // Firecracker binary.
    if !cfg.fc_bin.exists() {
        return Err(FcError::Probe(format!(
            "firecracker binary missing: {}",
            cfg.fc_bin.display()
        )));
    }
    let fc_text = exe_version(&cfg.fc_bin)?;
    let fc_v = parse_fc_version(&fc_text)
        .ok_or_else(|| FcError::Probe(format!("unparseable firecracker version {fc_text:?}")))?;
    if !version_ok(fc_v) {
        return Err(FcError::Probe(format!(
            "firecracker v{}.{}.{} outside supported range >=v{}.{}.{} <v{}.",
            fc_v.0,
            fc_v.1,
            fc_v.2,
            FC_MIN_VERSION.0,
            FC_MIN_VERSION.1,
            FC_MIN_VERSION.2,
            FC_MAX_MAJOR + 1
        )));
    }
    if let Some(pin) = &cfg.pinned_fc_version {
        let want = format!("v{pin}");
        if !fc_text.contains(&want) {
            return Err(FcError::Probe(format!(
                "firecracker version not pinned ({want} expected)"
            )));
        }
    }
    // Jailer.
    if !cfg.jailer_bin.exists() {
        return Err(FcError::Probe(format!(
            "jailer missing: {}",
            cfg.jailer_bin.display()
        )));
    }
    let jailer_text = exe_version(&cfg.jailer_bin).ok();
    if let Some(jt) = &jailer_text
        && let Some(jv) = parse_fc_version(jt)
        && jv != fc_v
    {
        return Err(FcError::Probe(format!(
            "jailer version {}.{}.{} does not match firecracker",
            jv.0, jv.1, jv.2
        )));
    }
    // KVM.
    let kvm = kvm_ok();
    if !kvm {
        notes.push(
            "/dev/kvm missing or not R/W for the worker uid: hostile workloads refused".into(),
        );
    }
    // cgroups.
    let cg = cfg
        .cgroup_root
        .as_ref()
        .map(|p| cgroup_v2_writable(p))
        .unwrap_or(false);
    if !cg {
        notes.push("cgroup v2 delegation root not writable: physical ceilings cannot be enforced; host cgroup delegation must be configured before real-VM qualification (T12)".into());
    }
    let mut features = std::collections::BTreeMap::new();
    features.insert(
        "hardware_virtualization".to_string(),
        CapabilitySupport {
            supported: kvm,
            enforced: kvm,
            evidence: Some("/dev/kvm open+KVM_GET_API_VERSION".into()),
        },
    );
    features.insert(
        "vsock".to_string(),
        CapabilitySupport {
            supported: true,
            enforced: false,
            evidence: Some("firecracker built-in vsock device".into()),
        },
    );
    features.insert(
        "cgroups_v2".to_string(),
        CapabilitySupport {
            supported: cg,
            enforced: cg,
            evidence: cfg.cgroup_root.as_ref().map(|p| p.display().to_string()),
        },
    );
    Ok(BackendCapabilities {
        backend: "firecracker".into(),
        hardware_virtualization: kvm,
        kvm_device: kvm,
        vsock: true,
        cgroups_v2: cg,
        jailer: Some(cfg.jailer_bin.display().to_string()),
        firecracker_version: Some(format!("{}.{}.{}", fc_v.0, fc_v.1, fc_v.2)),
        features,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_and_bounds() {
        assert_eq!(parse_fc_version("Firecracker v1.8.1\n"), Some((1, 8, 1)));
        assert_eq!(parse_fc_version("v1.10.0-GIT"), Some((1, 10, 0)));
        assert_eq!(parse_fc_version("nonsense"), None);
        assert!(version_ok((1, 8, 0)));
        assert!(!version_ok((1, 7, 9)));
        assert!(!version_ok((2, 0, 0)));
    }

    #[test]
    fn probe_fails_closed_without_binaries() {
        let cfg = ProbeConfig {
            fc_bin: "/nonexistent/firecracker".into(),
            jailer_bin: "/nonexistent/jailer".into(),
            cgroup_root: None,
            pinned_fc_version: None,
        };
        let err = probe(&cfg).unwrap_err();
        assert!(matches!(err, FcError::Probe(_)), "{err}");
    }

    #[test]
    fn cgroup_probe_on_non_cgroup_dir_is_false() {
        assert!(!cgroup_v2_writable(Path::new("/tmp")));
    }
}
