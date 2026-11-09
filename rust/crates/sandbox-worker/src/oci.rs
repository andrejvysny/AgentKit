//! OCI digest importer: registry → verified layers → policy-extracted
//! rootfs → content-addressed image store (§10.2/§10.3).
//!
//! Security posture:
//! - Import happens **by immutable digest only**; tags are not accepted
//!   (identity must be content, not a mutable pointer).
//! - Registry origins come only from the admin configuration allowlist
//!   (S20); redirects are fully revalidated per hop.
//! - Every blob (manifest, config, layers) is re-hashed and must equal the
//!   digest the manifest promises; the manifest itself must equal the
//!   requested digest.
//! - **Nothing extracted is ever executed or trusted.** Tar extraction is a
//!   strict, hand-rolled policy: regular files, directories, relative
//!   symlinks/hardlinks only; absolute paths, dot-segments, device nodes,
//!   FIFOs, sockets, setuid/setgid/sticky bits, oversized members,
//!   file-count/depth/name-length caps all fail the import closed (§10.3).
//! - Whiteouts (`.wh.*`, `.wh..wh..opq`) are applied per OCI spec.
//! - The guest agent is injected from an admin-pinned binary whose digest
//!   was verified from disk — never from layer content.
//! - Output is published content-addressed and immutable; a mutated image
//!   yields a new digest and never overwrites (§10.3).

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use agentkit_sandbox_contracts::ids::{ArtifactDigest, ImageDigest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn tracing_line(msg: &str) {
    eprintln!("audit: {msg}");
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("oci import failed: {0}")]
    Failed(String),
}

fn fail<T>(msg: impl Into<String>) -> Result<T, ImportError> {
    Err(ImportError::Failed(msg.into()))
}

/// Admin-side configuration (S20: CLI/env only; never wire-supplied).
#[derive(Debug, Clone)]
pub struct OciConfig {
    /// Bare `host[:port]` origins the importer may contact (exact match).
    pub registries: Vec<String>,
    /// TEST-ONLY: permit plain http for allowlisted origins (fake registry).
    pub allow_insecure_test_origins: bool,
    pub staging_dir: PathBuf,
    pub images_dir: PathBuf,
    /// Admin-pinned guest agent binary + its digest (verified on disk).
    pub guest_agent_bin: PathBuf,
    pub guest_agent_digest: ArtifactDigest,
    /// Kernel the fleet boots; recorded in the image metadata.
    pub kernel_digest: Option<ImageDigest>,
    pub limits: ExtractLimits,
}

impl Default for OciConfig {
    fn default() -> Self {
        Self {
            registries: Vec::new(),
            allow_insecure_test_origins: false,
            staging_dir: PathBuf::from("staging"),
            images_dir: PathBuf::from("images"),
            guest_agent_bin: PathBuf::new(),
            guest_agent_digest: ArtifactDigest::of_bytes(b"unset"),
            kernel_digest: None,
            limits: ExtractLimits::default(),
        }
    }
}

/// Hard extraction bounds (§10.3 "Bound decompressed bytes, file count,
/// depth, name length and CPU/time").
#[derive(Debug, Clone)]
pub struct ExtractLimits {
    pub max_layers: u32,
    pub max_layer_bytes: u64,
    pub max_decompressed_bytes: u64,
    pub max_files: u32,
    pub max_path_len: usize,
    pub max_depth: u32,
    pub max_manifest_bytes: u64,
    pub max_config_bytes: u64,
}

impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_layers: 128,
            max_layer_bytes: 1 << 30,
            max_decompressed_bytes: 4 << 30,
            max_files: 500_000,
            max_path_len: 1024,
            max_depth: 32,
            max_manifest_bytes: 8 << 20,
            max_config_bytes: 8 << 20,
        }
    }
}

/// Immutable published image metadata (§10.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRecord {
    pub registry: String,
    pub repository: String,
    /// The digest that was requested AND the manifest bytes verified
    /// against it.
    pub oci_digest: ImageDigest,
    pub os: String,
    pub arch: String,
    /// Layer digests in application order (OCI order).
    pub layers: Vec<ArtifactDigest>,
    pub config_digest: ArtifactDigest,
    pub conversion_version: u32,
    pub guest_agent_digest: ArtifactDigest,
    pub kernel_digest: Option<ImageDigest>,
    /// Deterministic digest over the extracted tree (see `tree_digest`).
    pub rootfs_digest: ArtifactDigest,
    /// ext4 image filename inside the published directory, when built.
    pub rootfs_file: Option<String>,
    pub imported_at_ms: u64,
    /// Human-readable provenance (tool versions etc.), audit-safe.
    pub provenance: String,
}

/// The importer.
pub struct OciImporter {
    cfg: OciConfig,
    client: reqwest::Client,
}

impl OciImporter {
    pub fn new(cfg: OciConfig) -> Result<Self, ImportError> {
        std::fs::create_dir_all(&cfg.staging_dir)
            .map_err(|e| ImportError::Failed(format!("staging dir: {e}")))?;
        std::fs::create_dir_all(&cfg.images_dir)
            .map_err(|e| ImportError::Failed(format!("images dir: {e}")))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .build()
            .map_err(|e| ImportError::Failed(format!("http client: {e}")))?;
        Ok(Self { cfg, client })
    }

    fn scheme(&self) -> &'static str {
        if self.cfg.allow_insecure_test_origins {
            "http"
        } else {
            "https"
        }
    }

    fn check_registry(&self, registry: &str) -> Result<(), ImportError> {
        let matches_registry = self
            .cfg
            .registries
            .iter()
            .any(|r| r == registry || r.starts_with(&format!("{registry}:")));
        if !matches_registry {
            return fail(format!("registry {registry:?} not in admin allowlist"));
        }
        Ok(())
    }

    /// Bounded GET with full per-call revalidation (allowlist, scheme,
    /// size caps) and one-hop-at-a-time redirect revalidation.
    fn get<'a>(
        &'a self,
        url: &'a str,
        accept: Option<&'a str>,
        max: u64,
        hops_left: u8,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<u8>, ImportError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let parsed =
                reqwest::Url::parse(url).map_err(|e| ImportError::Failed(format!("url: {e}")))?;
            let host = parsed
                .host_str()
                .ok_or_else(|| ImportError::Failed("url missing host".into()))?;
            let with_port = format!("{host}:{}", parsed.port_or_known_default().unwrap_or(0));
            let in_allow = self
                .cfg
                .registries
                .iter()
                .any(|r| r == host || r == &with_port);
            if !in_allow {
                return fail("resolved host not in admin allowlist");
            }
            match parsed.scheme() {
                "https" => {}
                "http" if self.cfg.allow_insecure_test_origins => {}
                _ => return fail("unsupported scheme"),
            }
            let mut req = self.client.get(parsed.clone());
            if let Some(a) = accept {
                req = req.header(reqwest::header::ACCEPT, a);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| ImportError::Failed(format!("transport: {e}")))?;
            let status = resp.status();
            if status.is_redirection() {
                if hops_left == 0 {
                    return fail("too many redirects");
                }
                let loc = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| ImportError::Failed("redirect without location".into()))?;
                let next = parsed
                    .join(loc)
                    .map_err(|_| ImportError::Failed("bad redirect location".into()))?;
                return self.get(next.as_str(), accept, max, hops_left - 1).await;
            }
            if !status.is_success() {
                return fail(format!("registry returned {status}"));
            }
            if matches!(resp.content_length(), Some(cl) if cl > max) {
                return fail("blob exceeds size cap");
            }
            let mut acc: Vec<u8> = Vec::new();
            let mut resp = resp;
            loop {
                let chunk = resp
                    .chunk()
                    .await
                    .map_err(|e| ImportError::Failed(format!("stream: {e}")))?;
                let Some(chunk) = chunk else { break };
                if (acc.len() as u64) + (chunk.len() as u64) > max {
                    return fail("blob exceeds size cap");
                }
                acc.extend_from_slice(&chunk);
            }
            Ok(acc)
        })
    }

    /// Import `registry/repository@digest` for `arch`. Returns the published
    /// immutable record.
    pub async fn import(
        &self,
        registry: &str,
        repository: &str,
        digest: &ImageDigest,
        arch: &str,
    ) -> Result<ImageRecord, ImportError> {
        self.check_registry(registry)?;
        validate_repository(repository)?;
        let base = format!(
            "{}://{}/v2/{}/manifests/",
            self.scheme(),
            registry,
            repository
        );
        let accept = concat!(
            "application/vnd.oci.image.manifest.v1+json,",
            "application/vnd.oci.image.index.v1+json,",
            "application/vnd.docker.distribution.manifest.v2+json,",
            "application/vnd.docker.distribution.manifest.list.v2+json"
        );
        let hex = digest
            .as_str()
            .strip_prefix("sha256:")
            .ok_or_else(|| ImportError::Failed("digest shape".into()))?;
        let manifest_bytes = self
            .get(
                &format!("{base}{hex}"),
                Some(accept),
                self.cfg.limits.max_manifest_bytes,
                4,
            )
            .await?;
        // Identity gate: the served manifest must hash to the requested digest.
        if digest.as_str() != ArtifactDigest::of_bytes(&manifest_bytes).as_str() {
            return fail("served manifest does not match requested digest");
        }
        let mut manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| ImportError::Failed(format!("manifest json: {e}")))?;

        // Index → platform selection (linux + requested arch only).
        let media = manifest
            .get("mediaType")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        if media.contains("index")
            || media.contains("manifest.list")
            || manifest.get("manifests").is_some()
        {
            let manifests = manifest
                .get("manifests")
                .and_then(|m| m.as_array())
                .ok_or_else(|| ImportError::Failed("index without manifests".into()))?;
            let mut chosen: Option<String> = None;
            for m in manifests {
                let plat = m.get("platform");
                let os = plat.and_then(|p| p.get("os")).and_then(|o| o.as_str());
                let a = plat
                    .and_then(|p| p.get("architecture"))
                    .and_then(|o| o.as_str());
                if os == Some("linux") && a == Some(arch) {
                    chosen = m.get("digest").and_then(|d| d.as_str()).map(str::to_string);
                    break;
                }
            }
            let inner = chosen
                .ok_or_else(|| ImportError::Failed(format!("no linux/{arch} platform in index")))?;
            let bytes = self
                .get(
                    &format!("{base}{}", inner.trim_start_matches("sha256:")),
                    Some(accept),
                    self.cfg.limits.max_manifest_bytes,
                    4,
                )
                .await?;
            if ArtifactDigest::of_bytes(&bytes).as_str() != inner {
                return fail("inner manifest digest mismatch");
            }
            manifest = serde_json::from_slice(&bytes)
                .map_err(|e| ImportError::Failed(format!("inner manifest json: {e}")))?;
        }

        // Manifest validation (§10.2 step C).
        if manifest.get("schemaVersion").and_then(|s| s.as_u64()) != Some(2) {
            return fail("unsupported schemaVersion");
        }
        let config = manifest
            .get("config")
            .ok_or_else(|| ImportError::Failed("manifest without config".into()))?;
        let config_digest_str = config
            .get("digest")
            .and_then(|d| d.as_str())
            .ok_or_else(|| ImportError::Failed("config without digest".into()))?
            .to_string();
        if config.get("size").and_then(|s| s.as_u64()).unwrap_or(0)
            > self.cfg.limits.max_config_bytes
        {
            return fail("config blob exceeds cap");
        }
        let layers = manifest
            .get("layers")
            .and_then(|l| l.as_array())
            .ok_or_else(|| ImportError::Failed("manifest without layers".into()))?;
        if layers.is_empty() || layers.len() as u32 > self.cfg.limits.max_layers {
            return fail("layer count outside policy");
        }
        let mut layer_digests: Vec<ArtifactDigest> = Vec::new();
        for l in layers {
            let d = l
                .get("digest")
                .and_then(|d| d.as_str())
                .ok_or_else(|| ImportError::Failed("layer without digest".into()))?;
            let mt = l.get("mediaType").and_then(|m| m.as_str()).unwrap_or("");
            if !(mt.contains("tar") || mt.is_empty()) {
                return fail(format!("unexpected layer mediaType {mt:?}"));
            }
            if matches!(l.get("size").and_then(|s| s.as_u64()), Some(sz) if sz > self.cfg.limits.max_layer_bytes)
            {
                return fail("layer exceeds size cap");
            }
            layer_digests.push(
                ArtifactDigest::from_hex(d.trim_start_matches("sha256:"))
                    .map_err(|e| ImportError::Failed(format!("layer digest: {e}")))?,
            );
        }

        // Config blob: platform must match, identity verified.
        let blobs = format!("{}://{}/v2/{}/blobs/", self.scheme(), registry, repository);
        let cfg_hex = config_digest_str.trim_start_matches("sha256:").to_string();
        let cfg_bytes = self
            .get(
                &format!("{blobs}{cfg_hex}"),
                None,
                self.cfg.limits.max_config_bytes,
                4,
            )
            .await?;
        if format!("sha256:{cfg_hex}") != ArtifactDigest::of_bytes(&cfg_bytes).as_str() {
            return fail("config blob digest mismatch");
        }
        let cfg_json: serde_json::Value = serde_json::from_slice(&cfg_bytes)
            .map_err(|e| ImportError::Failed(format!("config json: {e}")))?;
        let cfg_arch = cfg_json.get("architecture").and_then(|a| a.as_str());
        let cfg_os = cfg_json.get("os").and_then(|o| o.as_str());
        if cfg_arch != Some(arch) || cfg_os != Some("linux") {
            return fail("image platform does not match request");
        }
        // env/cmd/entrypoint are deliberately NEVER interpreted (§10.2).

        // Staging workspace for this import.
        let tag = format!(
            "import-{}-{}",
            std::process::id(),
            &hex[..12.min(hex.len())]
        );
        let staging = self.cfg.staging_dir.join(&tag);
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .map_err(|e| ImportError::Failed(format!("staging: {e}")))?;
        let rootfs = staging.join("rootfs");
        std::fs::create_dir_all(&rootfs)
            .map_err(|e| ImportError::Failed(format!("rootfs dir: {e}")))?;

        // Layers: download → verify digest → extract under policy.
        for (i, ld) in layer_digests.iter().enumerate() {
            let blob = self
                .get(
                    &format!("{blobs}{}", ld.bytes_hex()),
                    None,
                    self.cfg.limits.max_layer_bytes,
                    4,
                )
                .await?;
            if ArtifactDigest::of_bytes(&blob) != *ld {
                return fail(format!("layer {i} digest mismatch"));
            }
            extract_layer(&blob, &rootfs, &self.cfg.limits)?;
        }

        // Guest agent injection from the admin-pinned binary (never from
        // layer content).
        let agent_bytes = std::fs::read(&self.cfg.guest_agent_bin)
            .map_err(|e| ImportError::Failed(format!("agent binary: {e}")))?;
        if ArtifactDigest::of_bytes(&agent_bytes) != self.cfg.guest_agent_digest {
            return fail("guest agent binary does not match admin digest");
        }
        let ak_dir = rootfs.join("agentkit");
        std::fs::create_dir_all(&ak_dir)
            .map_err(|e| ImportError::Failed(format!("agentkit dir: {e}")))?;
        let agent_path = ak_dir.join("agentkit-sandbox-guest");
        std::fs::write(&agent_path, &agent_bytes)
            .map_err(|e| ImportError::Failed(format!("agent write: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&agent_path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| ImportError::Failed(format!("agent mode: {e}")))?;
        }

        let rootfs_digest = tree_digest(&rootfs)?;
        let mut record = ImageRecord {
            registry: registry.to_string(),
            repository: repository.to_string(),
            oci_digest: digest.clone(),
            os: "linux".into(),
            arch: arch.to_string(),
            layers: layer_digests,
            config_digest: ArtifactDigest::from_hex(&cfg_hex)
                .map_err(|e| ImportError::Failed(format!("config digest: {e}")))?,
            conversion_version: 1,
            guest_agent_digest: self.cfg.guest_agent_digest.clone(),
            kernel_digest: self.cfg.kernel_digest.clone(),
            rootfs_digest: rootfs_digest.clone(),
            rootfs_file: None,
            imported_at_ms: now_ms(),
            provenance: format!(
                "agentkit-oci-import/1 (worker {}, flate2 1.1.10)",
                env!("CARGO_PKG_VERSION")
            ),
        };

        // ext4 build via unprivileged `mkfs.ext4 -d`; honest skip when the
        // host tool is absent (boot validation then NOT RUN).
        if which("mkfs.ext4") {
            let img = staging.join("rootfs.ext4");
            let total = dir_size(&rootfs)?;
            let size_mib = ((total / (1024 * 1024)) * 13 / 10 + 64).max(256);
            let file = std::fs::File::create(&img)
                .map_err(|e| ImportError::Failed(format!("img: {e}")))?;
            file.set_len(size_mib * 1024 * 1024)
                .map_err(|e| ImportError::Failed(format!("img size: {e}")))?;
            drop(file);
            let status = std::process::Command::new("mkfs.ext4")
                .arg("-q")
                .arg("-F")
                .arg("-d")
                .arg(&rootfs)
                .arg(&img)
                .status()
                .map_err(|e| ImportError::Failed(format!("mkfs spawn: {e}")))?;
            if !status.success() {
                return fail("mkfs.ext4 failed to build rootfs image");
            }
            std::fs::write(staging.join("rootfs.ext4.sha256"), {
                let bytes = std::fs::read(&img)
                    .map_err(|e| ImportError::Failed(format!("img read: {e}")))?;
                format!("{}\n", ArtifactDigest::of_bytes(&bytes).as_str())
            })
            .map_err(|e| ImportError::Failed(format!("img sidecar: {e}")))?;
            record.rootfs_file = Some("rootfs.ext4".into());
        } else {
            tracing_line("mkfs.ext4 unavailable: publishing tree-only image (ext4/boot NOT RUN)");
        }

        // Publish content-addressed + immutable.
        let dir_name = format!("sha256-{}", rootfs_digest.bytes_hex());
        let published = self.cfg.images_dir.join(&dir_name);
        if published.exists() {
            let existing: ImageRecord = serde_json::from_str(
                &std::fs::read_to_string(published.join("image.json"))
                    .map_err(|e| ImportError::Failed(format!("existing record: {e}")))?,
            )
            .map_err(|e| ImportError::Failed(format!("existing record json: {e}")))?;
            if existing.rootfs_digest != record.rootfs_digest
                || existing.oci_digest != record.oci_digest
            {
                return fail("published image collision with different content");
            }
        } else {
            let tmp = self.cfg.images_dir.join(format!("{dir_name}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp)
                .map_err(|e| ImportError::Failed(format!("publish dir: {e}")))?;
            copy_tree(&rootfs, &tmp.join("rootfs"))?;
            if let Some(f) = &record.rootfs_file {
                std::fs::copy(staging.join(f), tmp.join(f))
                    .map_err(|e| ImportError::Failed(format!("copy img: {e}")))?;
            }
            std::fs::write(
                tmp.join("image.json"),
                serde_json::to_vec_pretty(&record)
                    .map_err(|e| ImportError::Failed(format!("record json: {e}")))?,
            )
            .map_err(|e| ImportError::Failed(format!("record write: {e}")))?;
            std::fs::rename(&tmp, &published)
                .map_err(|e| ImportError::Failed(format!("publish rename: {e}")))?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        tracing_line(&format!(
            "AUDIT image imported registry={registry} repository={repository} oci={} rootfs={}",
            record.oci_digest.as_str(),
            record.rootfs_digest.as_str()
        ));
        Ok(record)
    }

    /// List published image records (for host display/selection).
    pub fn list(&self) -> Result<Vec<ImageRecord>, ImportError> {
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&self.cfg.images_dir) {
            Ok(rd) => rd,
            Err(_) => return Ok(out),
        };
        for e in rd.flatten() {
            let file = e.path().join("image.json");
            if let Ok(text) = std::fs::read_to_string(&file)
                && let Ok(rec) = serde_json::from_str::<ImageRecord>(&text)
            {
                out.push(rec);
            }
        }
        out.sort_by(|a, b| a.rootfs_digest.as_str().cmp(b.rootfs_digest.as_str()));
        Ok(out)
    }
}

fn validate_repository(repo: &str) -> Result<(), ImportError> {
    let ok = !repo.is_empty()
        && repo.len() <= 255
        && repo.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_' | b'/')
        })
        && !repo.starts_with('/')
        && !repo.ends_with('/')
        && !repo.contains("//")
        && repo
            .split('/')
            .all(|s| !s.is_empty() && s != "." && s != "..");
    if !ok {
        return fail(format!("invalid repository name {repo:?}"));
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(bin).is_file()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Strict tar reader (§10.3)
// ---------------------------------------------------------------------------

const BLOCK: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Hardlink,
}

struct TarEntry {
    kind: EntryKind,
    path: String,
    link: String,
    mode: u32,
    size: u64,
    payload: Vec<u8>,
}

fn octal(field: &[u8]) -> Result<u64, ImportError> {
    let s = std::str::from_utf8(field)
        .map_err(|_| ImportError::Failed("non-utf8 octal field".into()))?
        .trim_matches(|c| c == '\0' || c == ' ');
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| ImportError::Failed("bad octal field".into()))
}

fn cstr(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

fn parse_header(h: &[u8]) -> Result<TarEntry, ImportError> {
    if &h[257..263] != b"ustar\0" && &h[257..262] != b"ustar" {
        return fail("not a ustar archive");
    }
    let raw_path = cstr(&h[0..100]);
    let mode = octal(&h[100..108])? as u32;
    let size = octal(&h[124..136])?;
    let typeflag = h[156];
    let link = cstr(&h[157..257]);
    let prefix = cstr(&h[345..500]);
    let path = if prefix.is_empty() {
        raw_path
    } else {
        format!("{prefix}/{raw_path}")
    };
    let kind = match typeflag {
        b'0' | 0 => EntryKind::File,
        b'5' => EntryKind::Dir,
        b'2' => EntryKind::Symlink,
        b'1' => EntryKind::Hardlink,
        // Devices ('3','4'), FIFOs ('6'), contiguous ('7'), and all GNU/pax
        // extension flags are rejected outright (§10.3).
        _ => {
            return fail(format!(
                "tar entry type {:?} not permitted",
                typeflag as char
            ));
        }
    };
    Ok(TarEntry {
        kind,
        path,
        link,
        mode,
        size,
        payload: Vec::new(),
    })
}

/// Parse and apply one gzip'd (or plain) tar layer under the policy.
fn extract_layer(blob: &[u8], rootfs: &Path, limits: &ExtractLimits) -> Result<(), ImportError> {
    let decompressed = if blob.starts_with(&[0x1f, 0x8b]) {
        let mut dec = flate2::read::GzDecoder::new(blob);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = dec
                .read(&mut chunk)
                .map_err(|e| ImportError::Failed(format!("gunzip: {e}")))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > limits.max_decompressed_bytes {
                return fail("layer decompression exceeds byte cap");
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        buf
    } else {
        blob.to_vec()
    };

    let mut files_here: u32 = 0;
    let mut off = 0usize;
    let data = &decompressed[..];
    loop {
        if off + BLOCK > data.len() {
            let rest = &data[off..];
            if rest.is_empty() || rest.iter().all(|b| *b == 0) {
                break;
            }
            return fail("truncated tar stream");
        }
        let header = &data[off..off + BLOCK];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let mut e = parse_header(header)?;
        off += BLOCK;
        let need = (e.size as usize).div_ceil(BLOCK) * BLOCK;
        if off + need > data.len() {
            return fail("truncated tar payload");
        }
        e.payload = data[off..off + e.size as usize].to_vec();
        off += need;
        if e.size > limits.max_layer_bytes {
            return fail("member exceeds size cap");
        }
        files_here += 1;
        if files_here > limits.max_files {
            return fail("layer exceeds file count cap");
        }
        apply_entry(&e, rootfs, limits)?;
    }
    Ok(())
}

/// Validate a member path: relative, no dot-segments, bounded.
fn validate_member_path(path: &str, limits: &ExtractLimits) -> Result<PathBuf, ImportError> {
    if path.is_empty() || path.len() > limits.max_path_len {
        return fail("member path length outside policy");
    }
    if path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        return fail("absolute or escaped member path");
    }
    let mut out = PathBuf::new();
    let mut depth = 0u32;
    for c in Path::new(path).components() {
        match c {
            Component::Normal(seg) => {
                depth += 1;
                if depth > limits.max_depth {
                    return fail("member path depth exceeds policy");
                }
                out.push(seg);
            }
            _ => return fail("dot-segment or root in member path"),
        }
    }
    if out.as_os_str().is_empty() {
        return fail("empty member path");
    }
    Ok(out)
}

/// Validate a link target stays inside the rootfs (relative-only).
fn validate_link_target(
    target: &str,
    entry_parent: &Path,
    limits: &ExtractLimits,
) -> Result<PathBuf, ImportError> {
    if target.starts_with('/') || target.contains('\0') {
        return fail("absolute link target");
    }
    let joined = entry_parent.join(target);
    let mut out = PathBuf::new();
    let mut depth = 0u32;
    for c in joined.components() {
        match c {
            Component::Normal(seg) => {
                depth += 1;
                if depth > limits.max_depth {
                    return fail("link target depth exceeds policy");
                }
                out.push(seg);
            }
            Component::CurDir => {}
            _ => return fail("link target escapes root"),
        }
    }
    if out.as_os_str().is_empty() {
        return fail("empty link target");
    }
    Ok(out)
}

fn apply_entry(e: &TarEntry, rootfs: &Path, limits: &ExtractLimits) -> Result<(), ImportError> {
    let rel = validate_member_path(&e.path, limits)?;

    // OCI whiteouts.
    if let Some(name) = rel.file_name().and_then(|n| n.to_str())
        && let Some(victim) = name.strip_prefix(".wh.")
    {
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let parent_dir = rootfs.join(parent);
        if victim == ".wh..opq" {
            if parent_dir.exists() {
                let entries = std::fs::read_dir(&parent_dir)
                    .map_err(|er| ImportError::Failed(format!("opaque read: {er}")))?;
                for entry in entries.flatten() {
                    let p = entry.path();
                    let _ = if p.is_dir() && !p.is_symlink() {
                        std::fs::remove_dir_all(&p)
                    } else {
                        std::fs::remove_file(&p)
                    };
                }
            }
            return Ok(());
        }
        let target = parent_dir.join(victim);
        let _ = if target.is_dir() && !target.is_symlink() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        return Ok(());
    }

    // Special-mode rejection (§10.3).
    if e.mode & 0o7000 != 0 {
        return fail(format!(
            "setuid/setgid/sticky bits not permitted on {}",
            e.path
        ));
    }

    let full = rootfs.join(&rel);
    match e.kind {
        EntryKind::Dir => std::fs::create_dir_all(&full)
            .map_err(|er| ImportError::Failed(format!("mkdir {}: {er}", e.path))),
        EntryKind::File => {
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|er| ImportError::Failed(format!("mkdirp: {er}")))?;
            }
            // Do not follow a pre-existing symlink at the destination.
            if full.symlink_metadata().is_ok() && full.is_symlink() {
                return fail("refusing to write through symlinked destination");
            }
            use std::io::Write as _;
            let mut f = std::fs::File::create(&full)
                .map_err(|er| ImportError::Failed(format!("create {}: {er}", e.path)))?;
            f.write_all(&e.payload)
                .map_err(|er| ImportError::Failed(format!("write {}: {er}", e.path)))?;
            let perm = e.mode & 0o777;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&full, std::fs::Permissions::from_mode(perm))
                    .map_err(|er| ImportError::Failed(format!("mode: {er}")))?;
            }
            #[cfg(not(unix))]
            {
                let _ = perm;
            }
            Ok(())
        }
        EntryKind::Symlink => {
            let parent = rel.parent().unwrap_or_else(|| Path::new(""));
            let target = validate_link_target(&e.link, parent, limits)?;
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|er| ImportError::Failed(format!("mkdirp link: {er}")))?;
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&target, &full)
                    .map_err(|er| ImportError::Failed(format!("symlink: {er}")))?;
            }
            #[cfg(not(unix))]
            {
                let _ = (target, full);
                return fail("symlinks unsupported on this platform");
            }
            Ok(())
        }
        EntryKind::Hardlink => {
            // tar hardlink linknames are ARCHIVE-ROOT relative (unlike
            // symlinks): validate as a member path.
            let target_rel = validate_member_path(&e.link, limits)?;
            let source = rootfs.join(&target_rel);
            let sm = source
                .symlink_metadata()
                .map_err(|_| ImportError::Failed("hardlink to missing target".into()))?;
            if sm.file_type().is_symlink() || !sm.is_file() {
                return fail("hardlink target must be a regular file");
            }
            if let Some(p) = full.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|er| ImportError::Failed(format!("mkdirp hard: {er}")))?;
            }
            std::fs::hard_link(&source, &full)
                .map_err(|er| ImportError::Failed(format!("hardlink: {er}")))?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Tree helpers
// ---------------------------------------------------------------------------

fn dir_size(root: &Path) -> Result<u64, ImportError> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd =
            std::fs::read_dir(&dir).map_err(|e| ImportError::Failed(format!("size walk: {e}")))?;
        for e in rd.flatten() {
            let p = e.path();
            let md = p
                .symlink_metadata()
                .map_err(|e| ImportError::Failed(format!("size meta: {e}")))?;
            if md.is_dir() {
                stack.push(p);
            } else {
                total += md.len();
            }
        }
    }
    Ok(total)
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), ImportError> {
    std::fs::create_dir_all(to).map_err(|e| ImportError::Failed(format!("copy mkdir: {e}")))?;
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        let rd = std::fs::read_dir(&src_dir)
            .map_err(|e| ImportError::Failed(format!("copy walk: {e}")))?;
        for e in rd.flatten() {
            let sp = e.path();
            let dp = dst_dir.join(e.file_name());
            let md = sp
                .symlink_metadata()
                .map_err(|e| ImportError::Failed(format!("copy meta: {e}")))?;
            if md.is_dir() {
                std::fs::create_dir_all(&dp)
                    .map_err(|e| ImportError::Failed(format!("copy dir: {e}")))?;
                stack.push((sp, dp));
            } else if md.is_symlink() {
                #[cfg(unix)]
                {
                    let t = std::fs::read_link(&sp)
                        .map_err(|e| ImportError::Failed(format!("copy link: {e}")))?;
                    std::os::unix::fs::symlink(&t, &dp)
                        .map_err(|e| ImportError::Failed(format!("copy link: {e}")))?;
                }
            } else {
                std::fs::copy(&sp, &dp)
                    .map_err(|e| ImportError::Failed(format!("copy file: {e}")))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        &dp,
                        std::fs::Permissions::from_mode(md.permissions().mode() & 0o7777),
                    )
                    .map_err(|e| ImportError::Failed(format!("copy mode: {e}")))?;
                }
            }
        }
    }
    Ok(())
}

/// Deterministic digest over the extracted tree: sorted relative entries,
/// each contributing kind|mode|path|link-target|content-digest.
pub fn tree_digest(root: &Path) -> Result<ArtifactDigest, ImportError> {
    let mut entries: Vec<(String, String, u32, String)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir)
            .map_err(|e| ImportError::Failed(format!("digest walk: {e}")))?;
        for e in rd.flatten() {
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .map_err(|_| ImportError::Failed("digest rel".into()))?
                .to_string_lossy()
                .replace('\\', "/");
            let md = p
                .symlink_metadata()
                .map_err(|e| ImportError::Failed(format!("digest meta: {e}")))?;
            if md.is_dir() {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    md.permissions().mode() & 0o7777
                };
                #[cfg(not(unix))]
                let mode = 0u32;
                entries.push((rel, "d".into(), mode, String::new()));
                stack.push(p);
            } else if md.is_symlink() {
                let t = std::fs::read_link(&p)
                    .map_err(|e| ImportError::Failed(format!("digest link: {e}")))?;
                entries.push((rel, "l".into(), 0, t.to_string_lossy().into_owned()));
            } else {
                #[cfg(unix)]
                let mode = {
                    use std::os::unix::fs::PermissionsExt;
                    md.permissions().mode() & 0o7777
                };
                #[cfg(not(unix))]
                let mode = 0u32;
                let bytes = std::fs::read(&p)
                    .map_err(|e| ImportError::Failed(format!("digest read: {e}")))?;
                entries.push((
                    rel,
                    "f".into(),
                    mode,
                    ArtifactDigest::of_bytes(&bytes).as_str().to_string(),
                ));
            }
        }
    }
    entries.sort();
    let mut h = Sha256::new();
    for (rel, kind, mode, extra) in &entries {
        h.update(format!("{kind}|{mode:o}|{rel}|{extra}\n").as_bytes());
    }
    Ok(ArtifactDigest::of_bytes(&h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> ExtractLimits {
        ExtractLimits {
            max_layer_bytes: 1 << 20,
            max_decompressed_bytes: 1 << 22,
            max_files: 64,
            max_path_len: 128,
            max_depth: 4,
            ..Default::default()
        }
    }

    /// Build an in-memory ustar archive (optionally gzip'd).
    fn tar_build(entries: &[(&str, EntryKind, u32, &[u8], &str)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for (path, kind, mode, payload, link) in entries {
            let mut h = vec![0u8; BLOCK];
            let name_bytes = path.as_bytes();
            h[..name_bytes.len().min(100)]
                .copy_from_slice(&name_bytes[..name_bytes.len().min(100)]);
            let mode_s = format!("{:07o}\0", mode);
            h[100..108].copy_from_slice(mode_s.as_bytes());
            let size_s = format!("{:011o}\0", payload.len());
            h[124..136].copy_from_slice(size_s.as_bytes());
            h[156] = match kind {
                EntryKind::File => b'0',
                EntryKind::Dir => b'5',
                EntryKind::Symlink => b'2',
                EntryKind::Hardlink => b'1',
            };
            let l = link.as_bytes();
            h[157..157 + l.len().min(100)].copy_from_slice(&l[..l.len().min(100)]);
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            // Header checksum.
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            let cs = format!("{:06o}\0 ", sum);
            h[148..156].copy_from_slice(cs.as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(payload);
            let pad = payload.len().div_ceil(BLOCK) * BLOCK - payload.len();
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
        out
    }

    fn tar_gz(entries: &[(&str, EntryKind, u32, &[u8], &str)]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar_build(entries)).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn extracts_files_dirs_and_whiteouts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        extract_layer(
            &tar_gz(&[
                ("etc", EntryKind::Dir, 0o755, b"", ""),
                ("etc/passwd", EntryKind::File, 0o644, b"root:x:0:0", ""),
                ("etc/shadow", EntryKind::File, 0o640, b"secret", ""),
            ]),
            root,
            &limits(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("etc/passwd")).unwrap(),
            b"root:x:0:0"
        );
        // Second layer deletes shadow via whiteout.
        extract_layer(
            &tar_gz(&[("etc/.wh.shadow", EntryKind::File, 0o644, b"", "")]),
            root,
            &limits(),
        )
        .unwrap();
        assert!(!root.join("etc/shadow").exists());
    }

    #[test]
    fn opaque_dir_marker_clears_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        extract_layer(
            &tar_gz(&[
                ("var", EntryKind::Dir, 0o755, b"", ""),
                ("var/a", EntryKind::File, 0o644, b"1", ""),
            ]),
            root,
            &limits(),
        )
        .unwrap();
        extract_layer(
            &tar_gz(&[("var/.wh..wh..opq", EntryKind::File, 0o644, b"", "")]),
            root,
            &limits(),
        )
        .unwrap();
        assert!(!root.join("var/a").exists());
        assert!(root.join("var").is_dir());
    }

    #[test]
    fn rejects_absolute_escape_and_dot_segments() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["/etc/passwd", "../../escape", "a/../../b"] {
            let r = extract_layer(
                &tar_gz(&[(bad, EntryKind::File, 0o644, b"x", "")]),
                tmp.path(),
                &limits(),
            );
            assert!(r.is_err(), "must reject {bad}");
        }
    }

    #[test]
    fn rejects_setuid_and_device_and_fifo() {
        let tmp = tempfile::tempdir().unwrap();
        let r = extract_layer(
            &tar_gz(&[("suid", EntryKind::File, 0o4755, b"x", "")]),
            tmp.path(),
            &limits(),
        );
        assert!(r.is_err() && r.unwrap_err().to_string().contains("setuid"));
        // Device node: hand-build a header with typeflag '3'.
        let mut raw = tar_build(&[("dev", EntryKind::File, 0o644, b"", "")]);
        raw[156] = b'3';
        let r = extract_layer(&raw, tmp.path(), &limits());
        assert!(r.is_err(), "device nodes must be rejected");
    }

    #[test]
    fn rejects_symlink_escape_and_absolute_target() {
        let tmp = tempfile::tempdir().unwrap();
        let r = extract_layer(
            &tar_gz(&[(
                "evil",
                EntryKind::Symlink,
                0o777,
                b"",
                "../../../../etc/passwd",
            )]),
            tmp.path(),
            &limits(),
        );
        assert!(r.is_err(), "symlink escape must be rejected");
        let r = extract_layer(
            &tar_gz(&[("abs", EntryKind::Symlink, 0o777, b"", "/etc/passwd")]),
            tmp.path(),
            &limits(),
        );
        assert!(r.is_err(), "absolute symlink target must be rejected");
    }

    #[test]
    fn relative_symlink_and_hardlink_within_root_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        extract_layer(
            &tar_gz(&[
                ("dir", EntryKind::Dir, 0o755, b"", ""),
                ("dir/file", EntryKind::File, 0o644, b"data", ""),
                ("dir/link", EntryKind::Symlink, 0o777, b"", "file"),
                ("dir/hard", EntryKind::Hardlink, 0o644, b"", "dir/file"),
            ]),
            root,
            &limits(),
        )
        .unwrap();
        assert_eq!(std::fs::read(root.join("dir/hard")).unwrap(), b"data");
        // Link targets are normalized to rootfs-root-relative paths.
        assert_eq!(
            std::fs::read_link(root.join("dir/link")).unwrap(),
            PathBuf::from("dir/file")
        );
    }

    #[test]
    fn enforces_file_count_depth_and_length_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = "a/b/c/d/e/f"; // depth 6 > max_depth 4
        let r = extract_layer(
            &tar_gz(&[(deep, EntryKind::File, 0o644, b"x", "")]),
            tmp.path(),
            &limits(),
        );
        assert!(r.is_err(), "depth cap");
        let mut many: Vec<(&str, EntryKind, u32, &[u8], &str)> = Vec::new();
        let names: Vec<String> = (0..70).map(|i| format!("f{i}")).collect();
        let payloads: Vec<Vec<u8>> = names.iter().map(|_| b"x".to_vec()).collect();
        for (n, p) in names.iter().zip(&payloads) {
            many.push((n.as_str(), EntryKind::File, 0o644, p.as_slice(), ""));
        }
        let r = extract_layer(&tar_build(&many), tmp.path(), &limits());
        assert!(r.is_err(), "file count cap");
    }

    #[test]
    fn decompression_bomb_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        // 8 MiB of zeros gzips tiny; cap is 4 MiB.
        let big = vec![0u8; 8 * 1024 * 1024];
        let blob = {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), Compression::default());
            enc.write_all(&big).unwrap();
            enc.finish().unwrap()
        };
        let r = extract_layer(&blob, tmp.path(), &limits());
        assert!(r.is_err(), "decompression cap");
    }

    #[test]
    fn repository_shape_policy() {
        assert!(validate_repository("library/ubuntu").is_ok());
        assert!(validate_repository("Library/Ubuntu").is_err());
        assert!(validate_repository("../evil").is_err());
        assert!(validate_repository("a//b").is_err());
        assert!(validate_repository("").is_err());
    }

    #[test]
    fn tree_digest_is_deterministic_and_content_sensitive() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(a.path().join("d")).unwrap();
        std::fs::create_dir_all(b.path().join("d")).unwrap();
        std::fs::write(a.path().join("d/x"), b"1").unwrap();
        std::fs::write(b.path().join("d/x"), b"1").unwrap();
        assert_eq!(
            tree_digest(a.path()).unwrap(),
            tree_digest(b.path()).unwrap()
        );
        std::fs::write(b.path().join("d/x"), b"2").unwrap();
        assert_ne!(
            tree_digest(a.path()).unwrap(),
            tree_digest(b.path()).unwrap()
        );
    }
}
