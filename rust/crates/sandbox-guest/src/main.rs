//! Guest agent entrypoint (§8.1).
//!
//! Startup refuses to serve (exit 3, listener never bound) unless the kernel
//! cmdline carries a well-formed boot nonce and the private workspace root
//! opens. The only production listener is vsock; TCP loopback exists purely
//! for integration tests behind a double opt-in (`--test-tcp` **and**
//! `AGENTKIT_GUEST_TEST_TCP=1`) and is a defect if ever used in production.

use std::sync::Arc;

#[cfg(target_os = "linux")]
use agentkit_sandbox_contracts::api::ExecEventKind;

#[cfg(target_os = "linux")]
fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("agentkit-sandbox-guest: {msg}");
            std::process::exit(2);
        }
    };
    let params = match &args.cmdline_file {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(raw) => agentkit_sandbox_guest::cmdline::parse(&raw),
            Err(_) => agentkit_sandbox_guest::cmdline::BootParams {
                boot_nonce: None,
                allow_guest_root: false,
                workspace: agentkit_sandbox_guest::cmdline::DEFAULT_WORKSPACE.to_string(),
            },
        },
        None => agentkit_sandbox_guest::cmdline::from_proc_cmdline(),
    };
    let Some(nonce) = params.boot_nonce.clone() else {
        // T18 shape: refuse before any listener exists.
        eprintln!("agentkit-sandbox-guest: refusing to serve: missing/invalid boot nonce");
        std::process::exit(3);
    };

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("runtime: {}", e.kind());
            std::process::exit(3);
        }
    };
    rt.block_on(run(args, params, nonce));
}

#[cfg(target_os = "linux")]
async fn run(args: Args, params: agentkit_sandbox_guest::cmdline::BootParams, nonce: Vec<u8>) {
    use agentkit_sandbox_guest::{Server, Supervisor, vsock};

    let listener = if args.test_tcp {
        if std::env::var("AGENTKIT_GUEST_TEST_TCP").unwrap_or_default() != "1" {
            eprintln!("agentkit-sandbox-guest: --test-tcp requires AGENTKIT_GUEST_TEST_TCP=1");
            std::process::exit(2);
        }
        match vsock::bind_test_tcp(args.listen_port).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("test-tcp bind failed: {}", e.kind());
                std::process::exit(3);
            }
        }
    } else {
        match vsock::bind_vsock(args.listen_port) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("vsock bind failed: {}", e.kind());
                std::process::exit(3);
            }
        }
    };

    let fs = match open_roots(&args, &params) {
        Ok(fs) => Arc::new(fs),
        Err(e) => {
            eprintln!("refusing to serve: workspace roots unavailable: {}", e.code);
            std::process::exit(3);
        }
    };
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(
        agentkit_sandbox_contracts::ids::ExecutionId,
        ExecEventKind,
    )>();
    let sup = Arc::new(Supervisor::new(
        params.allow_guest_root,
        Arc::clone(&fs),
        tx,
    ));
    let server = Arc::new(Server::new(fs, sup, rx, nonce));
    server.accept_loop(&listener).await;
}

/// Open the guest roots. The hidden `--test-home`/`--test-tmp` overrides are
/// TEST-ONLY (same double opt-in as `--test-tcp`): production roots come from
/// boot params plus fixed guest conventions only.
#[cfg(target_os = "linux")]
fn open_roots(
    args: &Args,
    params: &agentkit_sandbox_guest::cmdline::BootParams,
) -> Result<agentkit_sandbox_guest::GuestFs, agentkit_sandbox_guest::error::GuestFail> {
    use agentkit_sandbox_guest::Roots;
    if (args.test_home.is_some() || args.test_tmp.is_some())
        && (!args.test_tcp || std::env::var("AGENTKIT_GUEST_TEST_TCP").unwrap_or_default() != "1")
    {
        eprintln!("agentkit-sandbox-guest: root overrides require --test-tcp + env gate");
        std::process::exit(2);
    }
    let roots = if args.test_home.is_some() || args.test_tmp.is_some() {
        let home = args.test_home.as_deref().unwrap_or("/home/agent");
        let tmp = args.test_tmp.as_deref().unwrap_or("/tmp");
        Roots::with_dirs(&params.workspace, home, tmp)
    } else {
        Roots::new(&params.workspace)
    };
    agentkit_sandbox_guest::GuestFs::open(roots)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("agentkit-sandbox-guest: linux-only guest agent");
    std::process::exit(3);
}

/// Parsed CLI. `--test-tcp` and `--cmdline-file` are hidden test hooks; the
/// TCP one additionally requires `AGENTKIT_GUEST_TEST_TCP=1`.
#[derive(Debug, PartialEq, Eq)]
pub struct Args {
    pub listen_port: u32,
    pub test_tcp: bool,
    pub cmdline_file: Option<String>,
    pub test_home: Option<String>,
    pub test_tmp: Option<String>,
}

impl Args {
    fn parse<I: Iterator<Item = String>>(it: I) -> Result<Self, String> {
        let mut a = Self {
            listen_port: 52,
            test_tcp: false,
            cmdline_file: None,
            test_home: None,
            test_tmp: None,
        };
        let mut it = it.peekable();
        while let Some(flag) = it.next() {
            match flag.as_str() {
                "--listen-port" => {
                    let v = it.next().ok_or("--listen-port needs a value")?;
                    a.listen_port = v
                        .parse::<u32>()
                        .map_err(|_| "--listen-port must be a number".to_string())?;
                    if a.listen_port == 0 || a.listen_port > u32::from(u16::MAX) {
                        return Err("--listen-port out of range".into());
                    }
                }
                "--test-tcp" => a.test_tcp = true,
                "--test-home" => {
                    a.test_home = Some(it.next().ok_or("--test-home needs a value")?);
                }
                "--test-tmp" => {
                    a.test_tmp = Some(it.next().ok_or("--test-tmp needs a value")?);
                }
                "--cmdline-file" => {
                    a.cmdline_file = Some(it.next().ok_or("--cmdline-file needs a value")?);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(
            Args::parse(std::iter::empty::<String>()).unwrap(),
            Args {
                listen_port: 52,
                test_tcp: false,
                cmdline_file: None,
                test_home: None,
                test_tmp: None,
            }
        );
    }

    #[test]
    fn flags() {
        let a = Args::parse(
            [
                "--listen-port",
                "5000",
                "--test-tcp",
                "--cmdline-file",
                "/tmp/c",
                "--test-home",
                "/h",
                "--test-tmp",
                "/t",
            ]
            .iter()
            .map(ToString::to_string),
        )
        .unwrap();
        assert_eq!(a.listen_port, 5000);
        assert!(a.test_tcp);
        assert_eq!(a.cmdline_file.as_deref(), Some("/tmp/c"));
        assert_eq!(a.test_home.as_deref(), Some("/h"));
        assert_eq!(a.test_tmp.as_deref(), Some("/t"));
        assert!(Args::parse(["--listen-port", "x"].iter().map(ToString::to_string)).is_err());
        assert!(Args::parse(["--nope"].iter().map(ToString::to_string)).is_err());
    }
}
