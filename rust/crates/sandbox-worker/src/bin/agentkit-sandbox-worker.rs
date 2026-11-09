//! `agentkit-sandbox-worker` — the unprivileged worker daemon binary.
//!
//! All paths arrive only from this admin CLI (S20); wire frames never carry
//! host paths. SIGTERM triggers a graceful drain: the accept loop stops,
//! live connections finish their in-flight replies, and the journal is
//! fsynced before exit.

use agentkit_sandbox_worker::backend::{
    BackendKind, FakeBackend, make_apple_vz_backend, make_firecracker_backend,
};
use agentkit_sandbox_worker::config::WorkerConfig;
use agentkit_sandbox_worker::guestlink::{DefaultGuestLinkFactory, GuestLinkFactory};
use agentkit_sandbox_worker::registry::QuotaCeilings;
use agentkit_sandbox_worker::service::WorkerService;
use agentkit_sandbox_worker::transport;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("import-image") {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        match rt.block_on(run_import(&args[1..])) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("agentkit-sandbox-worker import-image: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let config = match WorkerConfig::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentkit-sandbox-worker: {e}");
            std::process::exit(2);
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    if let Err(e) = rt.block_on(run(config)) {
        eprintln!("agentkit-sandbox-worker: fatal: {e}");
        std::process::exit(1);
    }
}

async fn run(config: WorkerConfig) -> agentkit_sandbox_worker::WorkerResult<()> {
    // Backend selection — fail closed, never silently fake.
    let backend: Arc<dyn agentkit_sandbox_contracts::api::SandboxBackend> = match config.backend {
        BackendKind::Fake => Arc::new(FakeBackend::default()),
        BackendKind::Firecracker => {
            let cfg = config
                .firecracker_config()
                .ok_or_else(|| agentkit_sandbox_worker::WorkerError::Contract(
                    agentkit_sandbox_contracts::error::SandboxError::new(
                        agentkit_sandbox_contracts::error::SandboxErrorCode::CapabilityUnsupported,
                        "firecracker backend requires --kernel-path/--rootfs-path/--fc-bin/--jailer-bin",
                    ),
                ))?;
            make_firecracker_backend(cfg).ok_or_else(|| {
                agentkit_sandbox_worker::WorkerError::Contract(
                    agentkit_sandbox_contracts::error::SandboxError::new(
                        agentkit_sandbox_contracts::error::SandboxErrorCode::CapabilityUnsupported,
                        "firecracker backend not linked into this build",
                    ),
                )
            })?
        }
        BackendKind::AppleVz => {
            let cfg = config.apple_config().ok_or_else(|| {
                agentkit_sandbox_worker::WorkerError::Contract(
                    agentkit_sandbox_contracts::error::SandboxError::new(
                        agentkit_sandbox_contracts::error::SandboxErrorCode::CapabilityUnsupported,
                        "apple-vz backend requires --kernel-path/--rootfs-path/--helper-path",
                    ),
                )
            })?;
            make_apple_vz_backend(cfg).ok_or_else(|| {
                agentkit_sandbox_worker::WorkerError::Contract(
                    agentkit_sandbox_contracts::error::SandboxError::new(
                        agentkit_sandbox_contracts::error::SandboxErrorCode::CapabilityUnsupported,
                        "apple-vz backend not linked into this build",
                    ),
                )
            })?
        }
    };
    let guest_factory: Arc<dyn GuestLinkFactory> = Arc::new(DefaultGuestLinkFactory);
    let service = WorkerService::new(
        config.clone(),
        backend,
        guest_factory,
        QuotaCeilings::default(),
    )?;
    if config.broker_enabled {
        let broker = agentkit_sandbox_worker::broker::Broker::new(
            agentkit_sandbox_worker::broker::BrokerConfig {
                enabled: true,
                test_origins: config.broker_test_origins.clone(),
                cache_dir: config.storage_root.join("broker-cache"),
            },
        )
        .map_err(agentkit_sandbox_worker::WorkerError::Internal)?;
        service.attach_broker(Arc::new(broker));
        eprintln!("audit: structured package broker ENABLED");
    }
    let listener = transport::bind_uds(&config.uds)?;
    eprintln!(
        "audit: worker {} rev {} listening on {} (backend {}, peers {:?})",
        config.worker_id,
        config.revision,
        config.uds.display(),
        config.backend_flag(),
        config.peer_uids
    );
    let (tx, rx) = tokio::sync::broadcast::channel::<()>(1);
    spawn_signal_handler(tx.clone());
    transport::serve(listener, Arc::clone(&service), config.peer_uids.clone(), rx).await;
    // Graceful drain: give in-flight work a moment, then durable-sync.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    service.journal_flush()?;
    eprintln!("audit: worker drained and exited cleanly");
    Ok(())
}

/// SIGTERM/SIGINT graceful drain. The tokio feature set deliberately omits
/// `signal`, so we block the signals process-wide and `sigwait` on a
/// dedicated thread, forwarding a tick to the async accept loop.
#[cfg(unix)]
fn spawn_signal_handler(tx: tokio::sync::broadcast::Sender<()>) {
    std::thread::spawn(move || {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTERM);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }
        let mut sig = 0i32;
        loop {
            let rc = unsafe { libc::sigwait(&set, &mut sig) };
            if rc == 0 && (sig == libc::SIGTERM || sig == libc::SIGINT) {
                let _ = tx.send(());
                return;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_signal_handler(_tx: tokio::sync::broadcast::Sender<()>) {}

/// Admin CLI: `import-image --registry <host[:port]> --repository <r>
/// --digest sha256:<hex> [--arch <a>] --staging <dir> --images-dir <dir>
/// --guest-agent-bin <path> --guest-agent-digest sha256:<hex>
/// [--kernel-digest sha256:<hex>] [--insecure-test-origins]`
/// (§10.2; admin-only paths — S20).
async fn run_import(args: &[String]) -> Result<(), String> {
    use agentkit_sandbox_worker::oci::{OciConfig, OciImporter};
    let mut registry: Option<String> = None;
    let mut repository: Option<String> = None;
    let mut digest: Option<String> = None;
    let mut arch = agentkit_sandbox_contracts::host_arch().to_string();
    let mut staging: Option<std::path::PathBuf> = None;
    let mut images: Option<std::path::PathBuf> = None;
    let mut agent_bin: Option<std::path::PathBuf> = None;
    let mut agent_digest: Option<String> = None;
    let mut kernel_digest: Option<String> = None;
    let mut insecure = false;
    let mut i = 0usize;
    let next = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} requires a value"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--registry" => registry = Some(next(&mut i, "--registry")?),
            "--repository" => repository = Some(next(&mut i, "--repository")?),
            "--digest" => digest = Some(next(&mut i, "--digest")?),
            "--arch" => arch = next(&mut i, "--arch")?,
            "--staging" => staging = Some(std::path::PathBuf::from(next(&mut i, "--staging")?)),
            "--images-dir" => {
                images = Some(std::path::PathBuf::from(next(&mut i, "--images-dir")?))
            }
            "--guest-agent-bin" => {
                agent_bin = Some(std::path::PathBuf::from(next(&mut i, "--guest-agent-bin")?));
            }
            "--guest-agent-digest" => agent_digest = Some(next(&mut i, "--guest-agent-digest")?),
            "--kernel-digest" => kernel_digest = Some(next(&mut i, "--kernel-digest")?),
            "--insecure-test-origins" => insecure = true,
            "--list" => {
                let images = images.ok_or("--list requires --images-dir")?;
                let cfg = OciConfig {
                    images_dir: images,
                    ..Default::default()
                };
                let imp = OciImporter::new(cfg).map_err(|e| e.to_string())?;
                for rec in imp.list().map_err(|e| e.to_string())? {
                    println!(
                        "{}",
                        serde_json::to_string(&rec).map_err(|e| e.to_string())?
                    );
                }
                return Ok(());
            }
            other => return Err(format!("import-image: unknown flag {other:?}")),
        }
        i += 1;
    }
    let registry = registry.ok_or("--registry is required")?;
    let cfg = OciConfig {
        registries: vec![registry.clone()],
        allow_insecure_test_origins: insecure,
        staging_dir: staging.ok_or("--staging is required")?,
        images_dir: images.ok_or("--images-dir is required")?,
        guest_agent_bin: agent_bin.ok_or("--guest-agent-bin is required")?,
        guest_agent_digest: agentkit_sandbox_contracts::ids::ArtifactDigest::parse(
            &agent_digest.ok_or("--guest-agent-digest is required")?,
        )
        .map_err(|e| format!("--guest-agent-digest: {e}"))?,
        kernel_digest: kernel_digest
            .map(|k| {
                agentkit_sandbox_contracts::ids::ImageDigest::parse(&k)
                    .map_err(|e| format!("--kernel-digest: {e}"))
            })
            .transpose()?,
        limits: Default::default(),
    };
    let imp = OciImporter::new(cfg).map_err(|e| e.to_string())?;
    let digest =
        agentkit_sandbox_contracts::ids::ImageDigest::parse(&digest.ok_or("--digest is required")?)
            .map_err(|e| format!("--digest: {e}"))?;
    let rec = imp
        .import(
            &registry,
            &repository.ok_or("--repository is required")?,
            &digest,
            &arch,
        )
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&rec).map_err(|e| e.to_string())?
    );
    Ok(())
}
