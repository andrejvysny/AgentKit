//! INV-10 and NET-1..NET-8 from inside a real sandbox container, against
//! a real launcher (same environment as `real_backend.rs`, plus
//! `AGENTKIT_SANDBOX_PRIVATE_NAME`: a host name the server's `/etc/hosts`
//! maps to a private address, for NET-3). Each test uses a fresh
//! workspace and destroys it at the end. NET-9 (the proxy logs no
//! content) is a property of the host's logs and is not visible from
//! inside; the egress crate's tests cover it.
//!
//! Commands name the guest's egress shim explicitly (`--proxy`): the
//! image's environment file presets the proxy variables only from the
//! image version that ships `agentkit-sandboxd` (ADR 0039, Consequences).

mod support;

use std::time::{Duration, Instant};

use agentkit_workspaces::{NetworkPolicy, OperationState, SandboxBackend};
use support::real::{self, denials, exec, finish, run, spec, with_cleanup};

/// The guest's egress shim (`agentkit-sandboxd`).
const SHIM: &str = "http://127.0.0.1:3128";

/// `url` fetched through the shim; fails on any refusal.
fn curl(url: &str) -> String {
    format!("curl -fsS -o /dev/null --max-time 20 --proxy {SHIM} {url}")
}

/// INV-10, NET-1, NET-5: with no grant nothing leaves, by name, address,
/// DNS or UDP, and the refused name is drained with its operation.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn inv10_nothing_leaves_without_a_grant() {
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let ws = created.fresh("inv10");
        let m = backend.ensure_machine(&spec(&ws, &[])).await.unwrap();
        let named = run(&backend, &m, "op-name", &curl("https://example.com/")).await;
        assert!(!named.succeeded(), "{named:?}");
        let found = denials(&backend, &m).await;
        assert!(
            found.iter().any(|d| d.host == "example.com"
                && d.port == 443
                && d.operation_id.as_ref().map(|o| o.as_str()) == Some("op-name")),
            "{found:?}"
        );
        let by_address = curl("https://1.1.1.1/");
        let probes = [
            (
                "op-direct",
                "curl -fsS -o /dev/null --max-time 10 --noproxy '*' https://example.com/",
            ),
            (
                "op-tcp",
                "timeout 10 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443'",
            ),
            (
                "op-udp",
                "timeout 10 bash -c 'echo x > /dev/udp/1.1.1.1/53'",
            ),
            ("op-dns", "timeout 10 getent hosts example.com"),
            ("op-ip", by_address.as_str()),
        ];
        for (id, command) in probes {
            let done = run(&backend, &m, id, command).await;
            assert!(!done.succeeded(), "{id} left the sandbox: {done:?}");
        }
    })
    .await;
}

/// NET-2, NET-4: a grant opens exactly its host and port; another port,
/// another name and an address literal stay refused.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn net2_a_grant_opens_exactly_its_host_and_port() {
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let ws = created.fresh("net2");
        let grants = [("example.com", 443)];
        let m = backend.ensure_machine(&spec(&ws, &grants)).await.unwrap();
        let granted = run(&backend, &m, "op-ok", &curl("https://example.com/")).await;
        assert!(granted.succeeded(), "{granted:?}");
        for (id, url) in [
            ("op-port", "http://example.com/"),
            ("op-name", "https://www.example.com/"),
            ("op-ip", "https://1.1.1.1/"),
        ] {
            let done = run(&backend, &m, id, &curl(url)).await;
            assert!(!done.succeeded(), "{url}: {done:?}");
        }
        let found = denials(&backend, &m).await;
        let targets: Vec<(String, u16)> = found.iter().map(|d| (d.host.clone(), d.port)).collect();
        for expected in [
            ("example.com", 80),
            ("www.example.com", 443),
            ("1.1.1.1", 443),
        ] {
            assert!(
                targets.contains(&(expected.0.to_owned(), expected.1)),
                "{expected:?} not in {targets:?}"
            );
        }
    })
    .await;
}

/// NET-3: a granted name that resolves (on the host) to a private address
/// is refused and drained.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST, AGENTKIT_SANDBOX_PRIVATE_NAME"]
async fn net3_a_granted_name_on_a_private_address_is_refused() {
    let name = std::env::var("AGENTKIT_SANDBOX_PRIVATE_NAME").expect(
        "AGENTKIT_SANDBOX_PRIVATE_NAME must name a host /etc/hosts maps to a private address",
    );
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let ws = created.fresh("net3");
        let m = backend
            .ensure_machine(&spec(&ws, &[(&name, 443)]))
            .await
            .unwrap();
        let url = format!("https://{name}/");
        let done = run(&backend, &m, "op-private", &curl(&url)).await;
        assert!(!done.succeeded(), "{done:?}");
        let found = denials(&backend, &m).await;
        assert!(
            found.iter().any(|d| d.host == name && d.port == 443),
            "the refused address is drained: {found:?}"
        );
    })
    .await;
}

/// The default grants are enough for pip.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn pip_downloads_through_the_package_grants() {
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let ws = created.fresh("pip");
        let grants = [("pypi.org", 443), ("files.pythonhosted.org", 443)];
        let m = backend.ensure_machine(&spec(&ws, &grants)).await.unwrap();
        let command = format!(
            "python3 -m pip download --proxy {SHIM} --no-deps --no-cache-dir -d /workspace/dl six"
        );
        let done = run(&backend, &m, "op-pip", &command).await;
        assert!(done.succeeded(), "{done:?}");
        let listed = run(&backend, &m, "op-ls", "ls /workspace/dl").await;
        assert!(listed.output.contains("six-"), "{listed:?}");
    })
    .await;
}

/// NET-6: revoking a grant closes an open tunnel within 5 s.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn net6_a_revoke_closes_an_open_tunnel_within_five_seconds() {
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let ws = created.fresh("net6");
        let m = backend
            .ensure_machine(&spec(&ws, &[("pypi.org", 443)]))
            .await
            .unwrap();
        let slow = format!(
            "curl -sS --limit-rate 20k -o /dev/null --max-time 110 --proxy {SHIM} https://pypi.org/simple/"
        );
        let op = exec(&m, "op-slow", &slow, 120_000);
        backend.dispatch(&m, &op).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let seen = backend.observe(&m, &op.id, 0).await.unwrap();
        assert_eq!(seen.state, OperationState::Running, "the download runs");
        let revoked = Instant::now();
        backend
            .apply_network_policy(&m, &NetworkPolicy::default())
            .await
            .unwrap();
        let done = finish(&backend, &m, &op.id).await;
        assert!(!done.succeeded(), "{done:?}");
        assert!(
            revoked.elapsed() < Duration::from_secs(6),
            "the tunnel closed {:?} after the revoke",
            revoked.elapsed()
        );
    })
    .await;
}

/// NET-7: the guest sees its own two sockets only; no launcher, runtime
/// or other workspace socket.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn net7_the_guest_reaches_no_other_socket() {
    let launcher = real::launcher_socket();
    with_cleanup(real::backend(|_| {}), |backend, created| async move {
        let other = created.fresh("net7b");
        backend.ensure_machine(&spec(&other, &[])).await.unwrap();
        let ws = created.fresh("net7a");
        let m = backend.ensure_machine(&spec(&ws, &[])).await.unwrap();
        let own = run(
            &backend,
            &m,
            "op-own",
            "test -S /run/agentkit/control.sock && test -S /run/agentkit/egress.sock",
        )
        .await;
        assert!(own.succeeded(), "{own:?}");
        // Every unix socket file the guest can see, in every mount.
        let all = "find / \\( -path /proc -o -path /sys -o -path /dev \\) -prune -o -type s -print 2>/dev/null; true";
        let found = run(&backend, &m, "op-find", all).await;
        let own_sockets = ["/run/agentkit/control.sock", "/run/agentkit/egress.sock"];
        for socket in found.output.lines() {
            assert!(own_sockets.contains(&socket), "the guest sees {socket}");
        }
        let absent = format!(
            "test ! -e '{}' && test ! -e /var/run/docker.sock && test ! -e /run/podman/podman.sock",
            launcher.display()
        );
        let done = run(&backend, &m, "op-absent", &absent).await;
        assert!(done.succeeded(), "{done:?}");
    })
    .await;
}

/// NET-8: one tunnel past the limit is refused, and a limit refusal is
/// not drained as a request for a new host.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "real: needs AGENTKIT_SANDBOX_LAUNCHER_SOCKET, AGENTKIT_SANDBOX_SOCKET_DIR, AGENTKIT_SANDBOX_IMAGE_DIGEST"]
async fn net8_the_tunnel_limit_holds() {
    let backend = real::backend(|config| config.egress.max_tunnels = 2);
    with_cleanup(backend, |backend, created| async move {
        let ws = created.fresh("net8");
        let m = backend
            .ensure_machine(&spec(&ws, &[("pypi.org", 443)]))
            .await
            .unwrap();
        let three = format!(
            "for i in 1 2 3; do \
             (curl -sS --max-time 15 --limit-rate 5k -o /dev/null --proxy {SHIM} https://pypi.org/simple/; \
             echo \"rc=$?\") & sleep 1; done; wait"
        );
        let done = run(&backend, &m, "op-three", &three).await;
        let codes: Vec<&str> = done
            .output
            .lines()
            .filter_map(|line| line.strip_prefix("rc="))
            .collect();
        assert_eq!(codes.len(), 3, "{done:?}");
        // Two tunnels run into their 15 s limit (or finish); the third is refused.
        let refused = codes.iter().filter(|code| !matches!(**code, "0" | "28"));
        assert_eq!(refused.count(), 1, "{done:?}");
        let found = denials(&backend, &m).await;
        assert!(found.is_empty(), "a limit refusal is not drained: {found:?}");
    })
    .await;
}
