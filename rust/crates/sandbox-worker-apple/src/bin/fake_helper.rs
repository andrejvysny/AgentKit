//! Fake VZ helper: speaks the exact helper wire protocol (§12.2) without
//! any hypervisor. It is the L1 contract-test twin of the Swift helper —
//! it proves the adapter's framing, lifecycle, fail-closed gates, and
//! relay plumbing. It is **not** a VM and never counts as isolation proof.

use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use agentkit_sandbox_worker_apple::proto::{
    read_json, write_frame, HelperCmd, HelperProbe, HelperRequest, HelperResponse, VmManifest,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio");
    let code = match rt.block_on(run(&args)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("fake-helper: {e}");
            1
        }
    };
    std::process::exit(code);
}

async fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        Some("probe") => {
            let probe = HelperProbe {
                hardware_virtualization: true,
                entitled: true,
                helper_version: "1.0.0-fake".into(),
                notes: vec!["fake helper: L1 contract tests only, NOT a VM".into()],
            };
            let mut out = std::io::stdout();
            <std::io::Stdout as std::io::Write>::write_all(
                &mut out,
                serde_json::to_string(&probe)?.as_bytes(),
            )?;
            <std::io::Stdout as std::io::Write>::write_all(&mut out, b"\n")?;
            Ok(())
        }
        Some("vm") => {
            let manifest = arg(args, "--manifest")?;
            vm(manifest).await
        }
        _ => Err("usage: fake-helper probe | vm --manifest <path>".into()),
    }
}

fn arg(args: &[String], flag: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .ok_or_else(|| format!("{flag} required").into())
}

async fn vm(manifest_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let manifest: VmManifest = serde_json::from_str(&std::fs::read_to_string(&manifest_path)?)?;
    // Fail closed like the real helper: refuse to "boot" on missing images.
    for p in [&manifest.kernel, &manifest.rootfs, &manifest.data_disk] {
        if !PathBuf::from(p).exists() {
            return Err(format!("manifest path missing: {p}").into());
        }
    }
    if manifest.schema != 1 {
        return Err("unsupported manifest schema".into());
    }
    let relay = PathBuf::from(&manifest.relay_socket);
    let _ = std::fs::remove_file(&relay);
    let listener = UnixListener::bind(&relay)?;
    let port = manifest.guest_vsock_port;

    // Guest vsock relay: fc-compatible CONNECT handshake, then raw byte pipe
    // into an in-helper loopback (guest agent bytes echo back).
    {
        let relay = relay.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                {
                    tokio::spawn(async move {
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        match reader.read_line(&mut line).await {
                            Ok(0) => return,
                            Err(_) => return,
                            Ok(_) => {}
                        }
                        if !line.starts_with("CONNECT ") {
                            let mut r = reader.into_inner();
                            let _ = r.write_all(b"ERR bad handshake\n").await;
                            return;
                        }
                        let mut r = reader.into_inner();
                        let _ = r.write_all(format!("OK {port} fake\n").as_bytes()).await;
                        // loopback: echo everything back to the peer
                        let mut buf = vec![0u8; 8192];
                        loop {
                            let n = match r.read(&mut buf).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            };
                            if r.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
            let _ = std::fs::remove_file(&relay);
        });
    }

    // Command loop on stdin; EOF (adapter dropped us) exits like a killed VM.
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    loop {
        let req: Option<HelperRequest> = match read_json(&mut stdin).await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // adapter gone
            Err(e) => {
                let resp = HelperResponse {
                    id: 0,
                    ok: false,
                    data: None,
                    error: Some(e.to_string()),
                };
                let _ = write_frame(&mut stdout, &resp).await;
                return Err(e.to_string().into());
            }
        };
        let Some(req) = req else { break };
        match req.cmd {
            HelperCmd::Status {} => {
                let resp = HelperResponse {
                    id: req.id,
                    ok: true,
                    data: Some(serde_json::json!({ "state": "running" })),
                    error: None,
                };
                write_frame(&mut stdout, &resp).await?;
            }
            HelperCmd::Stop {} => {
                let resp = HelperResponse {
                    id: req.id,
                    ok: true,
                    data: None,
                    error: None,
                };
                write_frame(&mut stdout, &resp).await?;
                let _ = std::fs::remove_file(&relay);
                return Ok(());
            }
        }
    }
    Ok(())
}
