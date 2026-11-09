//! Structured package broker (§9). The ONLY path by which bytes may leave
//! toward a package registry, and the only path by which registry bytes
//! reach the guest.
//!
//! Normative rules enforced here:
//! - Requests are *structured* [`PackageFetchRequest`]s; a URL/method/query
//!   surface does not exist, so exfiltration shapes are structurally
//!   impossible (§9.1).
//! - Destinations come only from the frozen [`RegistryPolicy`] endpoints
//!   (exact https origins) and fixed per-ecosystem URL templates. Metadata
//!   is read only through approved endpoints; artifact URLs found inside
//!   metadata are revalidated against the same policy before egress.
//! - Redirects are never auto-followed; each hop is revalidated (scheme,
//!   host-in-policy, size, deadline) per §9.2.
//! - Artifact identity is bound to a digest: the guest pin and/or the
//!   registry-declared digest, always re-hashed from the actual bytes.
//! - Per-tenant windowed budgets (request count + egress bytes) per S13.
//! - Content is untrusted: written only into the worker's private
//!   content-addressed cache and streamed to the requesting guest. It is
//!   never executed, extracted, or opened on the host here.
//! - Audit lines carry ids and digests only, never payload bytes (S18).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agentkit_sandbox_contracts::api::PackageFetchRequest;
use agentkit_sandbox_contracts::ids::{ArtifactDigest, TenantId};
use agentkit_sandbox_contracts::policy::{RegistryEndpoint, RegistryPolicy};
use agentkit_sandbox_contracts::spec::Ecosystem;
use sha2::{Digest, Sha512};

fn tracing_line(msg: &str) {
    eprintln!("audit: {msg}");
}

/// Terminal denial reason; surfaced to the guest and audit log. Stable,
/// bounded strings — never echo request-controlled content verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerDeny {
    pub reason: String,
}

impl BrokerDeny {
    fn new(reason: impl Into<String>) -> Self {
        let mut r: String = reason.into();
        if r.len() > 200 {
            r.truncate(200);
        }
        Self { reason: r }
    }
}

/// A completed brokered fetch: bytes are in the content-addressed cache.
#[derive(Debug, Clone)]
pub struct FetchOk {
    /// Cache file path (worker-private, content-addressed).
    pub path: PathBuf,
    pub sha256: ArtifactDigest,
    pub bytes: u64,
    /// Registry-declared filename (validated: no separators, sane charset).
    pub filename: String,
}

/// Admin-side broker configuration (S20: admin-only; wire frames never
/// influence any of this).
#[derive(Debug, Clone, Default)]
pub struct BrokerConfig {
    pub enabled: bool,
    /// TEST-ONLY exact `host:port` origins served over plain http by a
    /// local fake registry. Empty in production; presence is loudly audited
    /// at startup. Never derivable from guest or host wire input.
    pub test_origins: Vec<String>,
    pub cache_dir: PathBuf,
}

/// Conservative worker-build cap on resident artifact bytes; the frozen
/// policy may set lower per-endpoint caps, never higher effect.
pub const HARD_MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
/// Metadata documents are tiny; anything bigger is hostile.
const HARD_MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;

/// Per-tenant sliding-window accounting (S13).
#[derive(Debug, Default)]
struct Window {
    start_ms: u64,
    requests: u32,
    bytes: u64,
}

/// The broker. Cheap to clone-share via Arc.
pub struct Broker {
    cfg: BrokerConfig,
    client: reqwest::Client,
    windows: Mutex<HashMap<String, Window>>,
}

impl Broker {
    /// Build the broker. HTTPS-only against policy origins; redirects are
    /// never auto-followed (every hop is revalidated manually, §9.2).
    pub fn new(cfg: BrokerConfig) -> Result<Self, String> {
        for o in &cfg.test_origins {
            tracing_line(&format!(
                "AUDIT broker test origin override enabled: {o} (LOCAL TESTING ONLY)"
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .build()
            .map_err(|e| format!("broker http client: {e}"))?;
        std::fs::create_dir_all(&cfg.cache_dir).map_err(|e| format!("broker cache dir: {e}"))?;
        Ok(Self {
            cfg,
            client,
            windows: Mutex::new(HashMap::new()),
        })
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Charge one request + `bytes` against the tenant window (S13).
    fn charge(
        &self,
        tenant: &TenantId,
        policy: &RegistryPolicy,
        bytes: u64,
    ) -> Result<(), BrokerDeny> {
        let mut w = self.windows.lock().expect("broker windows lock");
        let key = tenant.as_str().to_string();
        let win = w.entry(key).or_default();
        let now = now_ms();
        let window_ms = u64::from(policy.window_sec) * 1_000;
        if win.start_ms == 0 || now.saturating_sub(win.start_ms) >= window_ms {
            *win = Window {
                start_ms: now,
                requests: 0,
                bytes: 0,
            };
        }
        if win.requests >= policy.requests_per_window {
            return Err(BrokerDeny::new("broker request budget exhausted"));
        }
        if win.bytes.saturating_add(bytes) > policy.egress_bytes_per_window {
            return Err(BrokerDeny::new("broker egress budget exhausted"));
        }
        win.requests = win.requests.saturating_add(1);
        win.bytes = win.bytes.saturating_add(bytes);
        Ok(())
    }

    /// Is this URL's origin permitted: https + a policy host, or an
    /// admin-declared exact test origin (http allowed then)?
    fn origin_allowed(
        &self,
        policy: &RegistryPolicy,
        url: &reqwest::Url,
    ) -> Result<(), BrokerDeny> {
        if self
            .cfg
            .test_origins
            .iter()
            .any(|o| matches_test_origin(o, url))
        {
            return Ok(());
        }
        let host = url
            .host_str()
            .ok_or_else(|| BrokerDeny::new("url has no host"))?;
        if url.scheme() != "https" {
            return Err(BrokerDeny::new("only https origins permitted"));
        }
        if !policy.endpoints.iter().any(|e| e.host == host) {
            return Err(BrokerDeny::new("origin not in frozen registry policy"));
        }
        Ok(())
    }

    fn base_origin(&self, host: &str) -> String {
        if let Some(o) = self.cfg.test_origins.first() {
            format!("http://{o}")
        } else {
            format!("https://{host}")
        }
    }

    /// One structured fetch end-to-end. Never touches the network when
    /// disabled or when the request fails structural validation.
    pub async fn fetch(
        &self,
        tenant: &TenantId,
        policy: &RegistryPolicy,
        req: &PackageFetchRequest,
    ) -> Result<FetchOk, BrokerDeny> {
        req.validate()
            .map_err(|e| BrokerDeny::new(format!("invalid broker request: {e}")))?;
        if !self.cfg.enabled {
            return Err(BrokerDeny::new(
                "broker egress not enabled in this worker config",
            ));
        }
        let eps: Vec<&RegistryEndpoint> = policy
            .endpoints
            .iter()
            .filter(|e| e.ecosystem == req.ecosystem)
            .collect();
        if eps.is_empty() {
            return Err(BrokerDeny::new("ecosystem not in frozen registry policy"));
        }
        let deadline = std::time::Duration::from_secs(u64::from(policy.fetch_deadline_sec));
        let max_art = eps
            .iter()
            .map(|e| e.max_artifact_bytes)
            .max()
            .unwrap_or(0)
            .min(HARD_MAX_ARTIFACT_BYTES);
        let exact = exact_version(&req.version_spec)?;

        // Phase 1 — resolve artifact identity through approved metadata
        // endpoints only (§9.1). Phase 2 — download + bind digest.
        let (artifact_url, filename, expected_sha) = match req.ecosystem {
            Ecosystem::Crates => {
                let host = pick(&eps, "crates.io")?;
                (
                    format!(
                        "{}/api/v1/crates/{}/{}/download",
                        self.base_origin(host),
                        req.package,
                        urlenc(&exact)
                    ),
                    format!("{}-{}.crate", req.package, exact),
                    req.artifact_digest.clone().map(ExpectedIntegrity::Sha256),
                )
            }
            Ecosystem::Pypi => {
                let host = pick(&eps, "pypi.org")?;
                let meta_url = format!(
                    "{}/pypi/{}/{}/json",
                    self.base_origin(host),
                    req.package,
                    urlenc(&exact)
                );
                let meta = self.get_json(tenant, policy, &meta_url, deadline).await?;
                let urls = meta
                    .get("urls")
                    .and_then(|u| u.as_array())
                    .ok_or_else(|| BrokerDeny::new("pypi metadata missing urls"))?;
                let pin_hex = req.artifact_digest.as_ref().map(hex_of);
                let mut chosen: Option<(String, String, Option<String>)> = None;
                for entry in urls {
                    let filename = entry
                        .get("filename")
                        .and_then(|f| f.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let url = entry
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let declared = entry
                        .get("digests")
                        .and_then(|d| d.get("sha256"))
                        .and_then(|s| s.as_str())
                        .map(str::to_ascii_lowercase);
                    if let Some(pin) = &pin_hex {
                        if declared.as_deref() != Some(pin.as_str()) {
                            continue;
                        }
                    } else if declared.is_none() {
                        continue;
                    }
                    chosen = Some((url, filename, declared));
                    break;
                }
                let (url, filename, declared) =
                    chosen.ok_or_else(|| BrokerDeny::new("no artifact matches pinned digest"))?;
                // The metadata-declared digest is a hard expectation even
                // without a guest pin (§9.1 "validate digests").
                let expected = ExpectedIntegrity::Sha256(
                    ArtifactDigest::from_hex(declared.as_deref().unwrap_or_default())
                        .map_err(|_| BrokerDeny::new("registry declared bad digest"))?,
                );
                validate_filename(&filename)?;
                (url, filename, Some(expected))
            }
            Ecosystem::Npm => {
                let host = pick(&eps, "registry.npmjs.org")?;
                let enc = req.package.replace('/', "%2F");
                let meta_url = format!("{}/{}/{}", self.base_origin(host), enc, urlenc(&exact));
                let meta = self.get_json(tenant, policy, &meta_url, deadline).await?;
                let dist = meta
                    .get("dist")
                    .ok_or_else(|| BrokerDeny::new("npm metadata missing dist"))?;
                let url = dist
                    .get("tarball")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| BrokerDeny::new("npm metadata missing tarball"))?
                    .to_string();
                let integrity = dist
                    .get("integrity")
                    .and_then(|i| i.as_str())
                    .map(str::to_string);
                let filename = url.rsplit('/').next().unwrap_or("package.tgz").to_string();
                validate_filename(&filename)?;
                // Guest pin wins; otherwise the registry sha512 integrity
                // claim is REQUIRED and verified after download.
                let expected = if let Some(p) = req.artifact_digest.clone() {
                    Some(ExpectedIntegrity::Sha256(p))
                } else if let Some(i) = integrity {
                    Some(ExpectedIntegrity::Sha512(i))
                } else {
                    return Err(BrokerDeny::new("npm artifact has no verifiable integrity"));
                };
                (url, filename, expected)
            }
        };

        let url = reqwest::Url::parse(&artifact_url)
            .map_err(|_| BrokerDeny::new("registry provided an unparsable url"))?;
        self.origin_allowed(policy, &url)?;

        let bytes = self
            .download_bytes(tenant, policy, &url, max_art, deadline)
            .await?;

        // Digest binding (§9.1): rehash the actual bytes and compare.
        let sha = ArtifactDigest::of_bytes(&bytes);
        match &expected_sha {
            Some(ExpectedIntegrity::Sha256(pin)) => {
                if &sha != pin {
                    return Err(BrokerDeny::new(
                        "artifact digest does not match expected digest",
                    ));
                }
            }
            Some(ExpectedIntegrity::Sha512(integrity)) => {
                if !verify_sha512_integrity(&bytes, integrity) {
                    return Err(BrokerDeny::new("artifact fails registry integrity claim"));
                }
            }
            None => return Err(BrokerDeny::new("no digest basis for artifact")),
        }

        // Publish into the content-addressed cache atomically.
        let final_path = self.cfg.cache_dir.join(format!("sha256-{}", sha_hex(&sha)));
        if !final_path.exists() {
            let tmp = self
                .cfg
                .cache_dir
                .join(format!("tmp-{}-{}", now_ms(), rand_tag()));
            std::fs::write(&tmp, &bytes).map_err(|_| BrokerDeny::new("cache write failed"))?;
            std::fs::rename(&tmp, &final_path)
                .map_err(|_| BrokerDeny::new("cache publish failed"))?;
        }

        tracing_line(&format!(
            "AUDIT broker fetch ok tenant={} eco={} package={} version={} digest={}",
            tenant.as_str(),
            ecosystem_str(req.ecosystem),
            redact_field(&req.package),
            redact_field(&req.version_spec),
            sha_hex(&sha)
        ));
        Ok(FetchOk {
            path: final_path,
            sha256: sha,
            bytes: bytes.len() as u64,
            filename,
        })
    }

    /// Fetch + parse a JSON metadata document (bounded, charged).
    async fn get_json(
        &self,
        tenant: &TenantId,
        policy: &RegistryPolicy,
        raw: &str,
        deadline: std::time::Duration,
    ) -> Result<serde_json::Value, BrokerDeny> {
        let url =
            reqwest::Url::parse(raw).map_err(|_| BrokerDeny::new("metadata url unparsable"))?;
        self.origin_allowed(policy, &url)?;
        let bytes = self
            .download_bytes(tenant, policy, &url, HARD_MAX_METADATA_BYTES, deadline)
            .await?;
        serde_json::from_slice(&bytes).map_err(|_| BrokerDeny::new("metadata is not valid json"))
    }

    /// GET with redirect revalidation + hard byte caps; every hop must pass
    /// [`Broker::origin_allowed`] (§9.2 "revalidate at every hop").
    async fn download_bytes(
        &self,
        tenant: &TenantId,
        policy: &RegistryPolicy,
        first: &reqwest::Url,
        max_bytes: u64,
        deadline: std::time::Duration,
    ) -> Result<Vec<u8>, BrokerDeny> {
        let mut url = first.clone();
        let mut hops = 0u8;
        let max_hops = policy
            .endpoints
            .iter()
            .map(|e| e.max_redirects)
            .max()
            .unwrap_or(0);
        loop {
            self.origin_allowed(policy, &url)?;
            self.charge(tenant, policy, 0)?;
            let resp = tokio::time::timeout(
                deadline,
                self.client
                    .get(url.clone())
                    .header(reqwest::header::USER_AGENT, "agentkit-sandbox-broker/1")
                    .send(),
            )
            .await
            .map_err(|_| BrokerDeny::new("fetch deadline exceeded"))?
            .map_err(|_| BrokerDeny::new("transport failure"))?;

            let status = resp.status();
            if status.is_redirection() {
                if hops >= max_hops {
                    return Err(BrokerDeny::new("too many redirects"));
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| BrokerDeny::new("redirect without location"))?;
                let next = url
                    .join(loc)
                    .map_err(|_| BrokerDeny::new("invalid redirect location"))?;
                tracing_line(&format!(
                    "AUDIT broker redirect tenant={} hop={} to_host={} scheme={}",
                    tenant.as_str(),
                    hops + 1,
                    next.host_str().unwrap_or("?"),
                    next.scheme()
                ));
                url = next;
                hops += 1;
                continue;
            }
            if !status.is_success() {
                return Err(BrokerDeny::new(format!("registry returned {status}")));
            }
            if matches!(resp.content_length(), Some(cl) if cl > max_bytes) {
                return Err(BrokerDeny::new("artifact exceeds policy size cap"));
            }
            let mut acc: Vec<u8> = Vec::new();
            let mut resp = resp;
            loop {
                let chunk = tokio::time::timeout(deadline, resp.chunk())
                    .await
                    .map_err(|_| BrokerDeny::new("fetch deadline exceeded"))?
                    .map_err(|_| BrokerDeny::new("stream failure"))?;
                let Some(chunk) = chunk else { break };
                if (acc.len() as u64) + (chunk.len() as u64) > max_bytes {
                    return Err(BrokerDeny::new("artifact exceeds policy size cap"));
                }
                acc.extend_from_slice(&chunk);
            }
            return Ok(acc);
        }
    }
}

enum ExpectedIntegrity {
    /// sha256 pinned by the guest or declared by the registry.
    Sha256(ArtifactDigest),
    /// sha512-<base64> integrity claim from npm metadata.
    Sha512(String),
}

fn pick<'a>(eps: &[&'a RegistryEndpoint], host: &str) -> Result<&'a str, BrokerDeny> {
    eps.iter()
        .find(|e| e.host == host)
        .map(|e| e.host.as_str())
        .or_else(|| eps.first().map(|e| e.host.as_str()))
        .ok_or_else(|| BrokerDeny::new("no endpoint for ecosystem"))
}

fn hex_of(d: &ArtifactDigest) -> String {
    sha_hex(d).to_ascii_lowercase()
}

fn verify_sha512_integrity(bytes: &[u8], integrity: &str) -> bool {
    let Some(b64) = integrity.strip_prefix("sha512-") else {
        return false;
    };
    use base64::Engine;
    let Ok(want) = base64::engine::general_purpose::STANDARD.decode(b64) else {
        return false;
    };
    let got = Sha512::digest(bytes).to_vec();
    got == want
}

/// `pypi`/`npm`/`crates` metadata endpoints address EXACT versions only;
/// ranges/operators are rejected (determinism, §9.2).
fn exact_version(spec: &str) -> Result<String, BrokerDeny> {
    let s = spec.trim();
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
        && !s.contains("..");
    if !ok {
        return Err(BrokerDeny::new(
            "broker requires an exact version (ranges/ops not supported)",
        ));
    }
    Ok(s.to_string())
}

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Artifact filenames: no separators, no dot-segments, sane length.
fn validate_filename(name: &str) -> Result<(), BrokerDeny> {
    if name.is_empty()
        || name.len() > 255
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name == "."
        || name == ".."
    {
        return Err(BrokerDeny::new("registry provided an invalid filename"));
    }
    Ok(())
}

fn matches_test_origin(o: &str, url: &reqwest::Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let port = url.port_or_known_default();
    match o.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(p) => h == host && port == Some(p),
            Err(_) => false,
        },
        None => false,
    }
}

fn ecosystem_str(e: Ecosystem) -> &'static str {
    match e {
        Ecosystem::Pypi => "pypi",
        Ecosystem::Npm => "npm",
        Ecosystem::Crates => "crates",
    }
}

fn redact_field(s: &str) -> String {
    let mut out: String = s.chars().take(80).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

fn sha_hex(d: &ArtifactDigest) -> &str {
    d.as_str().strip_prefix("sha256:").unwrap_or(d.as_str())
}

fn rand_tag() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", std::process::id(), n)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Shared handle type used by the service.
pub type SharedBroker = Arc<Broker>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_version_rejects_ranges_and_traversal() {
        assert_eq!(exact_version("2.32.3").unwrap(), "2.32.3");
        assert!(exact_version(">=1.0").is_err());
        assert!(exact_version("~2.0").is_err());
        assert!(exact_version("..").is_err());
        assert!(exact_version("1.0/../2").is_err());
        assert!(exact_version("").is_err());
        assert!(exact_version("1.0?x=1").is_err());
    }

    #[test]
    fn filename_policy() {
        assert!(validate_filename("requests-2.32.3-py3-none-any.whl").is_ok());
        assert!(validate_filename("a/b").is_err());
        assert!(validate_filename("..").is_err());
        assert!(validate_filename("x\\y").is_err());
        assert!(validate_filename("").is_err());
        assert!(validate_filename(&"a".repeat(300)).is_err());
    }

    #[test]
    fn test_origin_exact_match() {
        let good = reqwest::Url::parse("http://127.0.0.1:8099/pypi/x/1/json").unwrap();
        assert!(matches_test_origin("127.0.0.1:8099", &good));
        assert!(!matches_test_origin("127.0.0.1:8098", &good));
        assert!(!matches_test_origin("127.0.0.1", &good));
        let https = reqwest::Url::parse("https://127.0.0.1:8099/x").unwrap();
        assert!(!matches_test_origin("127.0.0.1:8099", &https));
    }

    #[test]
    fn sha512_integrity_verification() {
        use base64::Engine;
        let data = b"payload";
        let b64 = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(data));
        assert!(verify_sha512_integrity(data, &format!("sha512-{b64}")));
        assert!(!verify_sha512_integrity(b"other", &format!("sha512-{b64}")));
        assert!(!verify_sha512_integrity(data, "sha256-abc"));
        assert!(!verify_sha512_integrity(data, "sha512-not-base64!!"));
    }

    #[tokio::test]
    async fn disabled_broker_never_egresses() {
        let dir = std::env::temp_dir().join(format!("ak-broker-off-{}", std::process::id()));
        let b = Broker::new(BrokerConfig {
            enabled: false,
            test_origins: vec![],
            cache_dir: dir,
        })
        .unwrap();
        let req = PackageFetchRequest {
            version: 1,
            ecosystem: Ecosystem::Pypi,
            package: "requests".into(),
            version_spec: "2.32.3".into(),
            artifact_digest: None,
            sandbox_generation: 1,
        };
        let err = b
            .fetch(
                &TenantId::new("t").unwrap(),
                &RegistryPolicy::curated_v1().unwrap(),
                &req,
            )
            .await
            .unwrap_err();
        assert!(err.reason.contains("not enabled"), "{err:?}");
    }

    #[tokio::test]
    async fn off_policy_origin_refused_even_when_enabled() {
        let dir = std::env::temp_dir().join(format!("ak-broker-origin-{}", std::process::id()));
        let b = Broker::new(BrokerConfig {
            enabled: true,
            test_origins: vec![],
            cache_dir: dir,
        })
        .unwrap();
        let mut req = PackageFetchRequest {
            version: 1,
            ecosystem: Ecosystem::Pypi,
            package: "requests".into(),
            version_spec: "2.32.3".into(),
            artifact_digest: None,
            sandbox_generation: 1,
        };
        // Malformed package shape is refused pre-network.
        req.package = "bad name with spaces".into();
        let err = b
            .fetch(
                &TenantId::new("t").unwrap(),
                &RegistryPolicy::curated_v1().unwrap(),
                &req,
            )
            .await
            .unwrap_err();
        assert!(err.reason.contains("invalid broker request"), "{err:?}");
    }
}
