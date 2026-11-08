//! Canonical executable discovery, official auth-status reading and the
//! version-qualified support matrix (AGENTKIT-109).
//!
//! Everything here uses only documented official interfaces:
//! * `<exe> --version` for version identity,
//! * `<exe> auth status` for the authentication route
//!   (`authMethod` ∈ none|claude.ai|oauth_token|api_key|api_key_helper|
//!   third_party, plus `configDirectory`),
//! * a presence/content check of `managed-settings.json` inside the
//!   configuration directory the official status reports.
//!
//! Hard rules (AGENTKIT-109):
//! * NO model invocation: only the two documented read-only subcommand
//!   shapes are ever spawned (a test asserts the exact argv).
//! * NO credential material is ever read: OAuth tokens, keychain entries and
//!   credential files under the configuration directory are off-limits; the
//!   status output is trusted only as a *route* observation. Status is
//!   best-effort: it never proves entitlement, quota, token refresh or
//!   inference success, and parse failure degrades to
//!   [`AuthMode::Unknown`] rather than a fatal refusal.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agentkit_native::{
    AuthEvidence, AuthMode, ExecutableEvidence, ExecutableLocator, NativeRefusal, PolicyState,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Qualified minimum official CLI version for this delivery (AGENTKIT-109).
/// Below this floor the support matrix marks the driver unqualified.
pub const QUALIFIED_FLOOR: (u32, u32, u32) = (2, 1, 259);

/// Honest qualification ledger (review §3: minimum feature compatibility is
/// not tested readiness). A capability flag reaches
/// [`agentkit_native::CapabilityState::Supported`] only once a live
/// qualification run is RECORDED here — the run id lives in the entry and
/// its evidence in `docs/qualification/AGENTKIT-118-evidence.md`. Every
/// other floor-satisfying flag is `Capable`: admitted, but never presented
/// as tested. Entries are plain flag names.
pub const RECORDED_QUALIFIED: &[&str] = &[];

/// Default bound for one probe subprocess (`--version`, `auth status`).
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 10_000;

/// Executables larger than this are never digest-scanned (replacement
/// recheck falls back to existence checks only, keeping probes bounded).
const MAX_DIGEST_BYTES: u64 = 200 * 1024 * 1024;

/// Streaming digest chunk size.
const DIGEST_CHUNK_BYTES: usize = 64 * 1024;

/// Documented filename of the managed policy file inside the configuration
/// directory reported by the official status.
const MANAGED_SETTINGS_FILE: &str = "managed-settings.json";

/// Extra spawn attempts tolerated for a transient `ETXTBSY` (installer
/// still holding the fresh executable open for writing).
const SELF_HEAL_SPAWN_RETRY: u32 = 2;

/// Resolves the canonical `claude` executable and reads official auth
/// status for a host-supplied environment (AGENTKIT-109). Probe and
/// execution use the same official configuration namespace; nothing here
/// mutates state, invokes a model, or reads credential material.
pub struct ClaudeLocator {
    explicit_path: Option<PathBuf>,
    probe_timeout_ms: u64,
    /// Test/host override for the `PATH` used in discovery. The port is
    /// defined for a *host-supplied environment* (AGENTKIT-108), so an
    /// explicit override is part of the contract, not a back door.
    path_override: Option<std::ffi::OsString>,
    /// Test/host override for `HOME` (used only for the documented common
    /// install locations; credential files are never read).
    home_override: Option<PathBuf>,
}

impl ClaudeLocator {
    /// New locator; when `explicit_path` is set it is the only candidate.
    pub fn new(explicit_path: Option<PathBuf>) -> Self {
        ClaudeLocator {
            explicit_path,
            probe_timeout_ms: DEFAULT_PROBE_TIMEOUT_MS,
            path_override: None,
            home_override: None,
        }
    }

    /// Override the bounded probe timeout (milliseconds).
    pub fn with_probe_timeout_ms(mut self, probe_timeout_ms: u64) -> Self {
        self.probe_timeout_ms = probe_timeout_ms;
        self
    }

    /// Override the `PATH` considered during discovery.
    pub fn with_path_env(mut self, path: impl Into<std::ffi::OsString>) -> Self {
        self.path_override = Some(path.into());
        self
    }

    /// Override `HOME` for the common-install fallback.
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home_override = Some(home.into());
        self
    }

    /// Resolve the canonical executable path without probing it.
    fn resolve_path(&self) -> Result<PathBuf, NativeRefusal> {
        if let Some(explicit) = &self.explicit_path {
            return match std::fs::metadata(explicit) {
                Ok(md) if md.is_file() => Ok(explicit.clone()),
                _ => Err(NativeRefusal::new(
                    "executable_missing",
                    format!(
                        "explicit claude executable '{}' is absent or not a regular file",
                        explicit.display()
                    ),
                    false,
                )),
            };
        }
        // PATH search: first regular executable named `claude` wins.
        let path = self
            .path_override
            .clone()
            .or_else(|| std::env::var_os("PATH"));
        if let Some(path) = path {
            for dir in std::env::split_paths(&path) {
                if dir.as_os_str().is_empty() {
                    continue;
                }
                let candidate = dir.join("claude");
                if is_regular_executable(&candidate) {
                    return Ok(candidate);
                }
            }
        }
        // Documented common install locations, in order. Only the executable
        // file itself is stat'ed; no credential file is ever touched.
        let home = self
            .home_override
            .clone()
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from));
        let mut fallbacks: Vec<PathBuf> = Vec::new();
        if let Some(home) = home {
            fallbacks.push(home.join(".local/bin/claude"));
            fallbacks.push(home.join(".claude/local/claude"));
        }
        fallbacks.push(PathBuf::from("/usr/local/bin/claude"));
        for candidate in fallbacks {
            if is_regular_executable(&candidate) {
                return Ok(candidate);
            }
        }
        Err(NativeRefusal::new(
            "executable_missing",
            "no `claude` executable found on PATH or in the documented common install \
             locations; install the official CLI or set an explicit executable path",
            false,
        ))
    }

    /// Run one bounded, read-only probe command with documented argv.
    /// Never invokes a model; kills the child on timeout by dropping it
    /// (kill-on-drop).
    async fn run_probe(
        &self,
        exe: &Path,
        argv: &[&str],
    ) -> Result<(Option<i32>, Vec<u8>), NativeRefusal> {
        let build_command = || {
            let mut command = Command::new(exe);
            command
                .args(argv)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                // Probe output on stderr is diagnostic only; discarding
                // bounds memory and cannot deadlock the child.
                .stderr(Stdio::null())
                .kill_on_drop(true);
            // Probes must observe the profile's own sign-in, never an
            // ambient credential that could fake a ready state.
            for key in crate::transport::credential_env_keys() {
                command.env_remove(key);
            }
            command
        };
        // Bounded retry for transient ETXTBSY: an installer/updater may
        // still hold the freshly written executable open for writing. Two
        // extra 25 ms attempts; anything else stays an honest refusal.
        let mut spawn_attempts = 0u32;
        let mut child = loop {
            match build_command().spawn() {
                Ok(child) => break child,
                Err(err)
                    if err.raw_os_error() == Some(libc::ETXTBSY)
                        && spawn_attempts < SELF_HEAL_SPAWN_RETRY =>
                {
                    spawn_attempts += 1;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => {
                    return Err(NativeRefusal::new(
                        "executable_missing",
                        format!("cannot spawn '{}': {err}", exe.display()),
                        false,
                    ));
                }
            }
        };
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let limit = Duration::from_millis(self.probe_timeout_ms);
        let outcome = tokio::time::timeout(limit, async {
            let mut buffer = Vec::new();
            // Best effort: read whatever arrived even if the read errors.
            let _ = stdout.read_to_end(&mut buffer).await;
            let status = child.wait().await.map_err(|err| err.to_string());
            (buffer, status)
        })
        .await;
        match outcome {
            // Future (and child) dropped at the deadline: kill-on-drop.
            Err(_elapsed) => Err(NativeRefusal::new(
                "probe_timeout",
                format!(
                    "`{} {}` exceeded the {} ms probe bound; killed",
                    exe.display(),
                    argv.join(" "),
                    self.probe_timeout_ms
                ),
                true,
            )),
            Ok((_buffer, Err(_wait_err))) => Err(NativeRefusal::new(
                "probe_failed",
                format!(
                    "`{} {}` could not be waited on",
                    exe.display(),
                    argv.join(" ")
                ),
                true,
            )),
            Ok((buffer, Ok(status))) => Ok((status.code(), buffer)),
        }
    }
}

fn is_regular_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Classify the documented `claude auth status` exit code: `0` means the
/// status command reported a signed-in state, `1` means not logged in.
pub(crate) fn classify_status_exit(code: Option<i32>) -> bool {
    code == Some(0)
}

/// Streaming sha256 of the executable bytes (64 KiB chunks) for replacement
/// detection. Best effort: IO errors or oversized files yield `None`.
async fn digest_file(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DIGEST_BYTES {
        return None;
    }
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; DIGEST_CHUNK_BYTES];
    loop {
        let read = file.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// First semver-ish `major.minor.patch` triple found in probe output, e.g.
/// `"2.1.310 (Claude Code)"` → `(2, 1, 310)`. Garbage yields `None`.
fn parse_version_triple(text: &str) -> Option<(u32, u32, u32)> {
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let Some((major, after_major)) = read_number(bytes, index) else {
            index += 1;
            continue;
        };
        if after_major >= bytes.len() || bytes[after_major] != b'.' {
            index = after_major.max(index + 1);
            continue;
        }
        let Some((minor, after_minor)) = read_number(bytes, after_major + 1) else {
            index = after_major + 1;
            continue;
        };
        if after_minor >= bytes.len() || bytes[after_minor] != b'.' {
            index = after_minor + 1;
            continue;
        }
        if let Some((patch, _)) = read_number(bytes, after_minor + 1) {
            return Some((major, minor, patch));
        }
        index = after_minor + 1;
    }
    None
}

fn read_number(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start {
        return None;
    }
    let text = std::str::from_utf8(&bytes[start..end]).ok()?;
    text.parse::<u32>().ok().map(|value| (value, end))
}

fn version_at_least(version: (u32, u32, u32), floor: (u32, u32, u32)) -> bool {
    version >= floor
}

#[async_trait]
impl ExecutableLocator for ClaudeLocator {
    /// Resolve the canonical executable, probe `--version` (the only argv
    /// shape besides `auth status` ever executed — no model invocation),
    /// and bind identity with a best-effort sha256 digest.
    async fn locate(&self) -> Result<ExecutableEvidence, NativeRefusal> {
        let path = self.resolve_path()?;
        let (code, stdout) = self.run_probe(&path, &["--version"]).await?;
        if code != Some(0) {
            return Err(NativeRefusal::new(
                "unsupported_version",
                format!(
                    "`{} --version` exited with {:?}; cannot establish identity",
                    path.display(),
                    code
                ),
                false,
            ));
        }
        let text = String::from_utf8_lossy(&stdout).trim().to_owned();
        if parse_version_triple(&text).is_none() {
            return Err(NativeRefusal::new(
                "unsupported_version",
                format!("version unparseable: official `--version` output {text:?}"),
                false,
            ));
        }
        let digest = digest_file(&path).await;
        Ok(ExecutableEvidence {
            path,
            version: text,
            digest,
        })
    }

    /// Official `auth status` reading. Best effort and NEVER fatal for
    /// parse problems: an uninterpretable status degrades to
    /// [`AuthMode::Unknown`]. Status is a route observation only — it is
    /// not proof of entitlement, quota, token refresh or inference success
    /// (AGENTKIT-109).
    async fn auth_status(&self) -> Result<AuthEvidence, NativeRefusal> {
        let path = self.resolve_path()?;
        let (code, stdout) = self.run_probe(&path, &["auth", "status"]).await?;
        let unknown = || AuthEvidence {
            mode: AuthMode::Unknown,
            policy: PolicyState::Unknown,
            config_directory: None,
        };
        let text = String::from_utf8_lossy(&stdout).trim().to_owned();
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            // Documented: exit 1 means "not logged in"; unparseable output
            // with that code is therefore an honest signed-out observation.
            if !classify_status_exit(code) {
                return Ok(AuthEvidence {
                    mode: AuthMode::None,
                    policy: PolicyState::None,
                    config_directory: None,
                });
            }
            return Ok(unknown());
        };
        let config_directory = value
            .get("configDirectory")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        let policy = match &config_directory {
            Some(dir) => policy_state(Path::new(dir)),
            // User layer unobservable without a reported directory: the
            // system-managed surface still decides when it has evidence,
            // otherwise the observation stays honestly Unknown.
            None => {
                let system_dirs = system_managed_dirs();
                let system = system_dirs
                    .iter()
                    .map(PathBuf::as_path)
                    .map(observe_managed_settings);
                match merge_observations(system) {
                    ManagedObservation::Absent | ManagedObservation::Unreadable(_) => {
                        PolicyState::Unknown
                    }
                    ManagedObservation::Present { restrictive: true } => PolicyState::Conflicting,
                    ManagedObservation::Present { restrictive: false } => {
                        PolicyState::PresentCompatible
                    }
                }
            }
        };
        if !classify_status_exit(code) {
            // Non-zero exit: documented "not logged in" regardless of body.
            return Ok(AuthEvidence {
                mode: AuthMode::None,
                policy,
                config_directory,
            });
        }
        let mode = match value.get("authMethod").and_then(|value| value.as_str()) {
            Some("claude.ai") => AuthMode::Subscription {
                account_scope: "claude.ai".to_owned(),
            },
            Some("oauth_token") => AuthMode::Subscription {
                account_scope: "oauth_token".to_owned(),
            },
            Some("api_key") => AuthMode::ApiKey,
            Some("api_key_helper") => AuthMode::ApiKeyHelper,
            Some("third_party") => AuthMode::ThirdParty {
                provider: value
                    .get("provider")
                    .and_then(|value| value.as_str())
                    .unwrap_or("third_party")
                    .to_owned(),
            },
            Some("none") => AuthMode::None,
            _ => AuthMode::Unknown,
        };
        Ok(AuthEvidence {
            mode,
            policy,
            config_directory,
        })
    }
}

/// What a present `managed-settings.json` says, without reading credential
/// material (the file is the documented managed-policy surface).
enum ManagedObservation {
    Absent,
    Unreadable(Option<String>),
    Present { restrictive: bool },
}

/// Documented system-managed policy locations per OS (Claude Code docs).
/// A system-managed `managed-settings.json` governs the install no matter
/// what the user configuration directory reports — it must be observed
/// separately (review §3: the user-dir check alone returned a wrong
/// "absent" beside a real system policy).
#[must_use]
pub fn system_managed_dirs() -> Vec<PathBuf> {
    // Injection seam for qualification harnesses/tests; production default
    // is the documented OS locations only.
    if let Ok(list) = std::env::var("AGENTKIT_MANAGED_SETTINGS_SYSTEM_DIRS") {
        return list
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    if cfg!(target_os = "macos") {
        vec![PathBuf::from("/Library/Application Support/ClaudeCode")]
    } else if cfg!(target_os = "linux") {
        vec![PathBuf::from("/etc/claude-code")]
    } else if cfg!(windows) {
        vec![PathBuf::from(r"C:\Program Files\ClaudeCode")]
    } else {
        vec![]
    }
}

/// Merge observations: the most restrictive evidence wins — a restrictive
/// system policy is a conflict even if the user directory is clean; an
/// unreadable file only ever degrades to honest `Unknown` when nothing
/// stricter was seen.
fn merge_observations(
    observations: impl Iterator<Item = ManagedObservation>,
) -> ManagedObservation {
    let mut saw_present_compatible = false;
    let mut saw_unreadable = false;
    for observation in observations {
        match observation {
            ManagedObservation::Present { restrictive: true } => {
                return ManagedObservation::Present { restrictive: true };
            }
            ManagedObservation::Present { restrictive: false } => {
                saw_present_compatible = true;
            }
            ManagedObservation::Unreadable(_) => saw_unreadable = true,
            ManagedObservation::Absent => {}
        }
    }
    if saw_present_compatible {
        ManagedObservation::Present { restrictive: false }
    } else if saw_unreadable {
        ManagedObservation::Unreadable(None)
    } else {
        ManagedObservation::Absent
    }
}

fn policy_state_for(config_dir: Option<&Path>, system_dirs: &[PathBuf]) -> PolicyState {
    match observe_managed_settings_all(config_dir, system_dirs) {
        ManagedObservation::Absent => PolicyState::None,
        ManagedObservation::Unreadable(_) => PolicyState::Unknown,
        ManagedObservation::Present { restrictive: true } => PolicyState::Conflicting,
        ManagedObservation::Present { restrictive: false } => PolicyState::PresentCompatible,
    }
}

fn observe_managed_settings_all(
    config_dir: Option<&Path>,
    system_dirs: &[PathBuf],
) -> ManagedObservation {
    merge_observations(
        config_dir
            .into_iter()
            .chain(system_dirs.iter().map(PathBuf::as_path))
            .map(observe_managed_settings),
    )
}

fn observe_managed_settings(config_dir: &Path) -> ManagedObservation {
    let path = config_dir.join(MANAGED_SETTINGS_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return ManagedObservation::Absent;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return ManagedObservation::Unreadable(Some(path.display().to_string().into()));
    };
    // Conservative restrictive markers: a permissions block gating bypass
    // mode, or any allow-list-style MCP server key the official CLI honors.
    let restrictive = value
        .get("permissions")
        .and_then(|permissions| permissions.get("disableBypassPermissionsMode"))
        .is_some()
        || value.as_object().is_some_and(|object| {
            object
                .keys()
                .any(|key| key.to_ascii_lowercase().contains("allowedmcpservers"))
        });
    ManagedObservation::Present { restrictive }
}

fn policy_state(config_dir: &Path) -> PolicyState {
    policy_state_for(Some(config_dir), &system_managed_dirs())
}

/// Driver-level managed-policy check exposed for preflight (AGENTKIT-109).
/// Returns a human-actionable reason when a documented managed settings
/// file exists — in the official configuration directory reported by
/// `auth status` OR at an OS system-managed location. Restrictions are
/// respected, never bypassed.
pub fn managed_policy_conflict(config_dir: &Path) -> Option<String> {
    managed_policy_conflict_for(Some(config_dir), &system_managed_dirs())
}

fn managed_policy_conflict_for(
    config_dir: Option<&Path>,
    system_dirs: &[std::path::PathBuf],
) -> Option<String> {
    match observe_managed_settings_all(config_dir, system_dirs) {
        ManagedObservation::Absent => None,
        ManagedObservation::Unreadable(path) => Some(match path {
            Some(path) => format!("managed policy unreadable: {path}"),
            None => "managed policy unreadable".to_owned(),
        }),
        ManagedObservation::Present { restrictive: true } => {
            Some("managed policy present".to_owned())
        }
        ManagedObservation::Present { restrictive: false } => {
            Some("managed policy file present".to_owned())
        }
    }
}

/// Version-qualified capability matrix for the official CLI (AGENTKIT-109).
pub struct ClaudeSupportMatrix;

impl ClaudeSupportMatrix {
    /// Frozen capability snapshot for one observed version (or `None` when
    /// the version was never observed). Support requires the qualified
    /// floor; anything else is explicit `Unsupported`/`Unknown` — never a
    /// guessed "yes".
    pub fn snapshot(version: Option<&str>) -> agentkit_native::CapabilitySnapshot {
        use agentkit_native::{CapabilitySnapshot, CapabilityState};
        let mut flags = std::collections::BTreeMap::new();
        let qualified = version.and_then(parse_version_triple);
        let below_floor_reason = format!(
            "version below qualified floor {}.{}.{}",
            QUALIFIED_FLOOR.0, QUALIFIED_FLOOR.1, QUALIFIED_FLOOR.2
        );
        let supported_flags = [
            "plain-chat",
            "owned-mcp-chat",
            "text-input",
            "extracted-text-input",
            "exact-resume",
            "seeded-new-session",
            "tool-observations",
            "live-interactions",
            "partial-deltas",
            "usage-tokens",
            "usage-cost-estimate",
            "interrupt-graceful",
            "permission-prompts-none",
            "safe-mode",
            "strict-mcp-config",
        ];
        // Honest qualification ledger lives at module level:
        // `RECORDED_QUALIFIED`.
        let qualified_version = version.unwrap_or("unknown").to_owned();
        match qualified {
            Some(triple) if version_at_least(triple, QUALIFIED_FLOOR) => {
                for flag in supported_flags {
                    // Two honest tiers (review §3): a flag is `Supported`
                    // only when a live qualification run is RECORDED for it;
                    // everything else the floor covers is `Capable` —
                    // admitted, but presented as "compatible, not yet
                    // qualified", never as "tested".
                    flags.insert(
                        flag.to_owned(),
                        if RECORDED_QUALIFIED.contains(&flag) {
                            CapabilityState::Supported {
                                qualified_version: qualified_version.clone(),
                                evidence: "qualification:AGENTKIT-118".to_owned(),
                            }
                        } else {
                            CapabilityState::Capable {
                                reason: format!(
                                    "version floor {}.{}.{} met; docs-declared, no recorded live qualification",
                                    QUALIFIED_FLOOR.0, QUALIFIED_FLOOR.1, QUALIFIED_FLOOR.2
                                ),
                            }
                        },
                    );
                }
                flags.insert(
                    "vendor-fork".to_owned(),
                    CapabilityState::Unsupported {
                        reason: "exact-parent behavior not yet qualified".to_owned(),
                    },
                );
                for flag in ["image-input", "file-input"] {
                    flags.insert(
                        flag.to_owned(),
                        CapabilityState::Unsupported {
                            reason: "outside this delivery".to_owned(),
                        },
                    );
                }
            }
            Some(_) => {
                for flag in agentkit_native::well_known_flags() {
                    flags.insert(
                        (*flag).to_owned(),
                        CapabilityState::Unsupported {
                            reason: below_floor_reason.clone(),
                        },
                    );
                }
            }
            None => {
                for flag in agentkit_native::well_known_flags() {
                    flags.insert((*flag).to_owned(), CapabilityState::Unknown);
                }
            }
        }
        // Out-of-scope surfaces are explicit `Unsupported` on every version
        // (admission validation requires them absent-or-unsupported).
        for flag in ["native-subagents", "codex", "native-coding-tools"] {
            flags.insert(
                flag.to_owned(),
                CapabilityState::Unsupported {
                    reason: "outside this delivery".to_owned(),
                },
            );
        }
        CapabilitySnapshot {
            backend: agentkit_native_backend_id(),
            matrix_version: 1,
            qualified_version,
            flags,
        }
    }

    /// True when `version` parses to at least [`QUALIFIED_FLOOR`]. Garbage
    /// is never qualified.
    pub fn is_qualified(version: &str) -> bool {
        parse_version_triple(version)
            .is_some_and(|triple| version_at_least(triple, QUALIFIED_FLOOR))
    }
}

fn agentkit_native_backend_id() -> String {
    // Keep the backend identity string in one place with the contract.
    "claude-code".to_owned()
}

/// Detect executable replacement between probe time and spawn time
/// (AGENTKIT-109). Compares the recorded digest (when one exists) against
/// the current bytes; a missing or non-regular path is replacement. Files
/// too large to digest at probe time carry no digest and are only checked
/// for existence as a regular file — an honest, bounded best effort.
pub fn recheck_replaced(probed: &ExecutableEvidence) -> Result<(), NativeRefusal> {
    let replaced = |detail: String| {
        NativeRefusal::new(
            "executable_replaced",
            format!(
                "'{}' changed since discovery: {detail}",
                probed.path.display()
            ),
            true,
        )
    };
    let Ok(metadata) = std::fs::metadata(&probed.path) else {
        return Err(replaced("path no longer exists".to_owned()));
    };
    if !metadata.is_file() {
        return Err(replaced("path is no longer a regular file".to_owned()));
    }
    let Some(expected) = &probed.digest else {
        // No digest was ever computed (oversized or unreadable at probe
        // time): existence as a regular file is the bounded guarantee.
        return Ok(());
    };
    if metadata.len() > MAX_DIGEST_BYTES {
        return Err(replaced(
            "file outgrew the digestable size bound".to_owned(),
        ));
    }
    // Synchronous here: preflight runs this on the readiness path and the
    // file is already size-bounded.
    let current = digest_blocking(&probed.path);
    match current {
        Some(current) if current == *expected => Ok(()),
        Some(_) => Err(replaced("sha256 digest differs".to_owned())),
        None => Err(replaced("file became unreadable".to_owned())),
    }
}

fn digest_blocking(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    use std::io::Read;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; DIGEST_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}
#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_native::{
        AuthExpectation, CapabilityState, ContentPolicy, EXECUTION_TARGET_SCHEMA_VERSION,
        ExecutionTarget, NativeExecutionTarget, NativeProfile, validate_execution_target,
    };

    /// Write a fake official `claude` executable: `--version` prints
    /// `version_out`; every other argv prints `auth_body` and exits
    /// `auth_exit` (documented read-only status shapes only).
    fn write_fake_exe(
        dir: &Path,
        name: &str,
        version_out: &str,
        auth_body: &str,
        auth_exit: i32,
    ) -> PathBuf {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{version_out}'; exit 0; fi\nprintf '%s\\n' '{auth_body}'\nexit {auth_exit}\n"
        );
        std::fs::write(&path, script).unwrap();
        make_executable(&path);
        path
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn locator(exe: &Path) -> ClaudeLocator {
        ClaudeLocator::new(Some(exe.to_path_buf()))
            // Keep unit tests fast and independent of machine load.
            .with_probe_timeout_ms(5_000)
    }

    #[tokio::test]
    async fn missing_binary_refuses_actionably() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope/claude");
        let err = locator(&missing).locate().await.unwrap_err();
        assert_eq!(err.code, "executable_missing");
        assert!(!err.retryable);
        assert!(err.message.contains("not a regular file") || err.message.contains("absent"));
    }

    #[tokio::test]
    async fn explicit_path_directory_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let err = locator(dir.path()).locate().await.unwrap_err();
        assert_eq!(err.code, "executable_missing");
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn path_search_picks_first_executable() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let exe_a = write_fake_exe(a.path(), "claude", "2.1.300 (Claude Code)", "{}", 0);
        write_fake_exe(b.path(), "claude", "1.0.0 (Claude Code)", "{}", 0);
        let path_env = format!("{}:{}", a.path().display(), b.path().display());
        let evidence = ClaudeLocator::new(None)
            .with_path_env(path_env)
            .with_home(dir_none_home())
            .locate()
            .await
            .unwrap();
        assert_eq!(evidence.path, exe_a);
        assert_eq!(evidence.version, "2.1.300 (Claude Code)");
        assert!(
            evidence.digest.is_some(),
            "digest is best-effort but must succeed for a regular file"
        );
    }

    fn dir_none_home() -> PathBuf {
        // A home with no claude installs so fallbacks never mask PATH order.
        PathBuf::from("/nonexistent-ak-test-home")
    }

    #[tokio::test]
    async fn common_install_fallback_is_found() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_fake_exe(&bin, "claude", "2.1.259 (Claude Code)", "{}", 0);
        let evidence = ClaudeLocator::new(None)
            .with_path_env("")
            .with_home(home.path())
            .locate()
            .await
            .unwrap();
        assert!(evidence.path.ends_with(".local/bin/claude"));
    }

    #[tokio::test]
    async fn version_probe_timeout_is_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then sleep 30; fi\nexit 0\n",
        )
        .unwrap();
        make_executable(&path);
        let err = locator(&path)
            .with_probe_timeout_ms(150)
            .locate()
            .await
            .unwrap_err();
        assert_eq!(err.code, "probe_timeout");
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn unparseable_version_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = write_fake_exe(dir.path(), "claude", "Claude Code (dev build)", "{}", 0);
        let err = locator(&exe).locate().await.unwrap_err();
        assert_eq!(err.code, "unsupported_version");
        assert!(!err.retryable);
        assert!(err.message.contains("version unparseable"));
    }

    #[tokio::test]
    async fn probe_argv_is_exactly_documented_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let path = dir.path().join("claude");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '2.1.310 (Claude Code)'; exit 0; fi\nprintf '%s\\n' '{{\"authMethod\":\"none\"}}'\nexit 1\n",
                log.display()
            ),
        )
        .unwrap();
        make_executable(&path);
        let loc = locator(&path);
        loc.locate().await.unwrap();
        loc.auth_status().await.unwrap();
        let logged = std::fs::read_to_string(&log).unwrap();
        // NO model-invoking argv is ever executed — only the two documented
        // read-only shapes, in order.
        assert_eq!(logged, "--version\nauth status\n");
    }

    #[tokio::test]
    async fn digest_detects_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let exe = write_fake_exe(dir.path(), "claude", "2.1.310 (Claude Code)", "{}", 0);
        let evidence = locator(&exe).locate().await.unwrap();
        assert!(
            evidence.digest.is_some(),
            "small regular file must carry a digest"
        );
        // Unchanged: recheck passes.
        recheck_replaced(&evidence).unwrap();
        // Rewrite the executable (v2): replacement must be detected.
        std::fs::write(&exe, "#!/bin/sh\nprintf '%s\\n' '2.1.311 (Claude Code)'\n").unwrap();
        make_executable(&exe);
        let err = recheck_replaced(&evidence).unwrap_err();
        assert_eq!(err.code, "executable_replaced");
        assert!(err.retryable);
        // Deleted executable is also replacement.
        std::fs::remove_file(&exe).unwrap();
        assert_eq!(
            recheck_replaced(&evidence).unwrap_err().code,
            "executable_replaced"
        );
    }

    async fn auth_for(body: &str, exit: i32) -> AuthEvidence {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir for the duration of the process: the fake
        // executable embeds the body inline, so only the script file needs
        // to persist until the call completes.
        let exe = write_fake_exe(
            dir.keep().as_path(),
            "claude",
            "2.1.310 (Claude Code)",
            body,
            exit,
        );
        locator(&exe).auth_status().await.unwrap()
    }

    #[tokio::test]
    async fn auth_status_maps_documented_methods() {
        let cases: Vec<(&str, i32, AuthMode)> = vec![
            (
                r#"{"authMethod":"claude.ai"}"#,
                0,
                AuthMode::Subscription {
                    account_scope: "claude.ai".into(),
                },
            ),
            (
                r#"{"authMethod":"oauth_token"}"#,
                0,
                AuthMode::Subscription {
                    account_scope: "oauth_token".into(),
                },
            ),
            (r#"{"authMethod":"api_key"}"#, 0, AuthMode::ApiKey),
            (
                r#"{"authMethod":"api_key_helper"}"#,
                0,
                AuthMode::ApiKeyHelper,
            ),
            (
                r#"{"authMethod":"third_party","provider":"bedrock"}"#,
                0,
                AuthMode::ThirdParty {
                    provider: "bedrock".into(),
                },
            ),
            (
                r#"{"authMethod":"third_party"}"#,
                0,
                AuthMode::ThirdParty {
                    provider: "third_party".into(),
                },
            ),
            (r#"{"authMethod":"none"}"#, 0, AuthMode::None),
            // Documented exit 1 = not logged in, even with a parseable body.
            (r#"{"authMethod":"oauth_token"}"#, 1, AuthMode::None),
            // Exit 1 + garbage = honest signed-out observation.
            ("status: not logged in", 1, AuthMode::None),
            // Exit 0 + garbage = best-effort Unknown, never fatal.
            ("!!!garbage!!!", 0, AuthMode::Unknown),
        ];
        for (body, exit, expected) in cases {
            let evidence = auth_for(body, exit).await;
            assert_eq!(evidence.mode, expected, "body={body} exit={exit}");
        }
    }

    #[tokio::test]
    async fn auth_status_carries_config_directory_and_policy() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        // Reported configDirectory, no managed file: policy None.
        let evidence = auth_for(
            &format!(
                r#"{{"authMethod":"oauth_token","configDirectory":"{}"}}"#,
                config.display()
            ),
            0,
        )
        .await;
        assert_eq!(
            evidence.config_directory.as_deref(),
            Some(config.to_str().unwrap())
        );
        assert_eq!(evidence.policy, PolicyState::None);
        // Non-restrictive managed file: PresentCompatible.
        std::fs::write(config.join("managed-settings.json"), "{}").unwrap();
        let evidence = auth_for(
            &format!(
                r#"{{"authMethod":"oauth_token","configDirectory":"{}"}}"#,
                config.display()
            ),
            0,
        )
        .await;
        assert_eq!(evidence.policy, PolicyState::PresentCompatible);
        // Restrictive managed file: Conflicting (driver-level check).
        std::fs::write(
            config.join("managed-settings.json"),
            r#"{"permissions":{"disableBypassPermissionsMode":"disable"}}"#,
        )
        .unwrap();
        let evidence = auth_for(
            &format!(
                r#"{{"authMethod":"oauth_token","configDirectory":"{}"}}"#,
                config.display()
            ),
            0,
        )
        .await;
        assert_eq!(evidence.policy, PolicyState::Conflicting);
        // Unreadable/invalid managed file: Unknown, never fatal.
        std::fs::write(config.join("managed-settings.json"), "{{{invalid").unwrap();
        let evidence = auth_for(
            &format!(
                r#"{{"authMethod":"oauth_token","configDirectory":"{}"}}"#,
                config.display()
            ),
            0,
        )
        .await;
        assert_eq!(evidence.policy, PolicyState::Unknown);
    }

    #[tokio::test]
    async fn managed_policy_conflict_reports_reasons() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(managed_policy_conflict_for(Some(dir.path()), &[]), None);
        std::fs::write(dir.path().join("managed-settings.json"), "{}").unwrap();
        assert_eq!(
            managed_policy_conflict_for(Some(dir.path()), &[]).as_deref(),
            Some("managed policy file present")
        );
        std::fs::write(
            dir.path().join("managed-settings.json"),
            r#"{"customAllowedMcpServers":["x"]}"#,
        )
        .unwrap();
        assert_eq!(
            managed_policy_conflict_for(Some(dir.path()), &[]).as_deref(),
            Some("managed policy present")
        );
        std::fs::write(dir.path().join("managed-settings.json"), "not json").unwrap();
        assert!(
            managed_policy_conflict_for(Some(dir.path()), &[])
                .is_some_and(|reason| reason.starts_with("managed policy unreadable")),
            "unreadable must name itself (and may name the path)"
        );
    }

    /// Review §3: system-managed policy governs the install regardless of
    /// what the user configuration directory reports — observing only the
    /// user directory used to claim "absent" beside a real system policy.
    #[test]
    fn system_managed_policy_is_observed_beyond_the_user_directory() {
        let user = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        let system_dir = system.path().to_path_buf();

        // Clean on both layers: none.
        assert_eq!(
            policy_state_for(Some(user.path()), &[system_dir.clone()]),
            PolicyState::None
        );

        // A restrictive SYSTEM policy is a conflict even when the user
        // directory is perfectly clean (the old user-dir-only check missed
        // this entirely).
        std::fs::write(
            system.path().join("managed-settings.json"),
            r#"{"customAllowedMcpServers":["only-this"]}"#,
        )
        .unwrap();
        assert_eq!(
            policy_state_for(Some(user.path()), &[system_dir.clone()]),
            PolicyState::Conflicting
        );
        assert_eq!(
            managed_policy_conflict_for(Some(user.path()), &[system_dir.clone()]).as_deref(),
            Some("managed policy present")
        );

        // Non-restrictive system file: present-compatible.
        std::fs::write(system.path().join("managed-settings.json"), "{}").unwrap();
        assert_eq!(
            policy_state_for(Some(user.path()), &[system_dir.clone()]),
            PolicyState::PresentCompatible
        );

        // Unreadable system file degrades to honest Unknown when nothing
        // stricter was seen — never a guessed None or a fake "compatible".
        std::fs::write(system.path().join("managed-settings.json"), "!!!").unwrap();
        assert_eq!(
            policy_state_for(Some(user.path()), &[system_dir.clone()]),
            PolicyState::Unknown
        );

        // Restrictive user file dominates an unreadable system file.
        std::fs::write(
            user.path().join("managed-settings.json"),
            r#"{"permissions":{"disableBypassPermissionsMode":"disable"}}"#,
        )
        .unwrap();
        assert_eq!(
            policy_state_for(Some(user.path()), &[system_dir]),
            PolicyState::Conflicting
        );
    }

    #[tokio::test]
    async fn missing_config_directory_leaves_policy_unknown() {
        let evidence = auth_for(r#"{"authMethod":"oauth_token"}"#, 0).await;
        assert_eq!(evidence.config_directory, None);
        assert_eq!(evidence.policy, PolicyState::Unknown);
    }

    const REQUIRED_WHEN_QUALIFIED: &[&str] = &[
        "plain-chat",
        "owned-mcp-chat",
        "text-input",
        "extracted-text-input",
        "exact-resume",
        "seeded-new-session",
        "tool-observations",
        "live-interactions",
        "partial-deltas",
        "usage-tokens",
        "usage-cost-estimate",
        "interrupt-graceful",
        "permission-prompts-none",
        "safe-mode",
        "strict-mcp-config",
    ];

    #[test]
    fn matrix_qualified_version_supports_required_flags() {
        let snap = ClaudeSupportMatrix::snapshot(Some("2.1.310"));
        assert_eq!(snap.backend, "claude-code");
        assert_eq!(snap.matrix_version, 1);
        assert_eq!(snap.qualified_version, "2.1.310");
        for flag in REQUIRED_WHEN_QUALIFIED {
            assert!(
                snap.state(flag).is_capable(),
                "{flag} must be admitted at 2.1.310"
            );
            // Honest tiers (review §3): only the RECORDED_QUALIFIED ledger
            // earns `Supported`; everything else the floor covers is
            // `Capable` — never presented as tested.
            assert_eq!(
                snap.state(flag).is_supported(),
                crate::discovery::RECORDED_QUALIFIED.contains(&flag),
                "{flag}: Supported must mirror the recorded-qualification ledger"
            );
        }
        // Out of scope is never supported, on any version.
        for flag in [
            "vendor-fork",
            "image-input",
            "file-input",
            "native-subagents",
            "codex",
            "native-coding-tools",
        ] {
            assert!(!snap.supports(flag), "{flag} must never be supported");
        }
        assert_eq!(snap.qualified_version, "2.1.310");
    }

    #[test]
    fn matrix_below_floor_is_all_unsupported_with_reason() {
        let snap = ClaudeSupportMatrix::snapshot(Some("2.1.258"));
        for flag in REQUIRED_WHEN_QUALIFIED {
            match snap.state(flag) {
                CapabilityState::Unsupported { reason } => {
                    assert!(reason.contains("2.1.259"), "{flag}: {reason}");
                }
                other => panic!("{flag} should be unsupported, got {other:?}"),
            }
        }
        // Floor boundary itself qualifies.
        let at_floor = ClaudeSupportMatrix::snapshot(Some("2.1.259"));
        assert!(at_floor.supports("plain-chat"));
        // Ancient majors are below floor too.
        let ancient = ClaudeSupportMatrix::snapshot(Some("1.9.9"));
        assert!(!ancient.supports("plain-chat"));
    }

    #[test]
    fn matrix_unknown_version_is_honest_unknown() {
        let snap = ClaudeSupportMatrix::snapshot(None);
        assert_eq!(snap.qualified_version, "unknown");
        for flag in agentkit_native::well_known_flags() {
            if matches!(*flag, "native-subagents" | "codex" | "native-coding-tools") {
                continue;
            }
            assert_eq!(snap.state(flag), CapabilityState::Unknown, "{flag}");
        }
        // Out-of-scope is explicit Unsupported even when unobserved.
        for flag in ["native-subagents", "codex", "native-coding-tools"] {
            assert!(matches!(
                snap.state(flag),
                CapabilityState::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn matrix_snapshot_serializes_without_credential_shaped_fields() {
        for version in [Some("2.1.310"), Some("2.0.0"), None] {
            let json = serde_json::to_string(&ClaudeSupportMatrix::snapshot(version)).unwrap();
            let lower = json.to_lowercase();
            for forbidden in [
                "credential",
                "secret",
                "apikey",
                "oauth",
                "keychain",
                "bearer",
            ] {
                assert!(
                    !lower.contains(forbidden),
                    "credential-shaped {forbidden} in {json}"
                );
            }
        }
    }

    #[test]
    fn is_qualified_rejects_garbage_and_old_versions() {
        assert!(ClaudeSupportMatrix::is_qualified("2.1.259"));
        assert!(ClaudeSupportMatrix::is_qualified("2.1.310 (Claude Code)"));
        assert!(ClaudeSupportMatrix::is_qualified("10.0.0"));
        assert!(!ClaudeSupportMatrix::is_qualified("2.1.258"));
        assert!(!ClaudeSupportMatrix::is_qualified("2.0.999"));
        assert!(!ClaudeSupportMatrix::is_qualified("garbage"));
        assert!(!ClaudeSupportMatrix::is_qualified(""));
    }

    fn target_with(snapshot: agentkit_native::CapabilitySnapshot) -> ExecutionTarget {
        ExecutionTarget::Native(NativeExecutionTarget {
            schema_version: EXECUTION_TARGET_SCHEMA_VERSION,
            backend: "claude-code".to_owned(),
            backend_version: Some("2.1.258".to_owned()),
            profile: NativeProfile::PlainChat,
            executable: "/usr/local/bin/claude".to_owned(),
            requested_model: None,
            requested_effort: None,
            auth_expectation: AuthExpectation::AsConfigured,
            capabilities: snapshot,
            config_identity: "sha256:test".to_owned(),
            system_prompt_revision: None,
            content: ContentPolicy::default(),
        })
    }

    #[test]
    fn below_floor_snapshot_fails_admission_validation() {
        let target = target_with(ClaudeSupportMatrix::snapshot(Some("2.1.258")));
        let violations = validate_execution_target(&target).unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.code == "capability_unsupported"),
            "expected capability_unsupported, got {violations:?}"
        );
    }

    #[test]
    fn unknown_version_snapshot_fails_admission_validation() {
        let target = target_with(ClaudeSupportMatrix::snapshot(None));
        let violations = validate_execution_target(&target).unwrap_err();
        assert!(violations.iter().any(|v| v.code == "capability_unknown"));
    }

    #[test]
    fn qualified_snapshot_passes_admission_validation() {
        let mut target = target_with(ClaudeSupportMatrix::snapshot(Some("2.1.310")));
        if let ExecutionTarget::Native(native) = &mut target {
            native.backend_version = Some("2.1.310".to_owned());
        }
        validate_execution_target(&target).unwrap();
    }

    #[test]
    fn version_triple_parser_is_first_match() {
        assert_eq!(
            parse_version_triple("2.1.310 (Claude Code)"),
            Some((2, 1, 310))
        );
        assert_eq!(parse_version_triple("claude 10.20.30"), Some((10, 20, 30)));
        assert_eq!(parse_version_triple("no numbers here"), None);
        assert_eq!(parse_version_triple("2.1"), None);
    }
}
