use std::net::{Ipv4Addr, SocketAddrV4};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use super::*;

fn loopback() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
}

/// An egress socket that answers every connection with what it read,
/// upper-cased.
fn upper_echo(path: &Path) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0; 64];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let _ = stream.write_all(&buf[..read].to_ascii_uppercase()).await;
            });
        }
    })
}

#[tokio::test]
async fn connections_are_piped_to_the_egress_socket() {
    let dir = tempfile::tempdir().unwrap();
    let egress = dir.path().join("egress.sock");
    let echo = upper_echo(&egress);
    let listener = bind(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shim = tokio::spawn(run_shim(listener, egress, 4));
    let mut client = TcpStream::connect(addr).await.unwrap();
    client.write_all(b"connect example.com:443").await.unwrap();
    let mut answer = vec![0; 23];
    client.read_exact(&mut answer).await.unwrap();
    assert_eq!(answer, b"CONNECT EXAMPLE.COM:443");
    shim.abort();
    echo.abort();
}

#[tokio::test]
async fn without_an_egress_socket_the_client_is_closed() {
    let dir = tempfile::tempdir().unwrap();
    let listener = bind(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shim = tokio::spawn(run_shim(listener, dir.path().join("absent.sock"), 4));
    let mut client = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("the shim closes promptly")
        .unwrap_or(0);
    assert_eq!(read, 0);
    shim.abort();
}

#[tokio::test]
async fn connections_over_the_bound_are_closed_at_accept() {
    let dir = tempfile::tempdir().unwrap();
    let egress = dir.path().join("egress.sock");
    // Accepts but never answers, so the first connection holds its permit.
    let silent = UnixListener::bind(&egress).unwrap();
    let held = tokio::spawn(async move {
        let mut open = Vec::new();
        while let Ok((stream, _)) = silent.accept().await {
            open.push(stream);
        }
    });
    let listener = bind(loopback()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shim = tokio::spawn(run_shim(listener, egress, 1));
    let _first = TcpStream::connect(addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut second = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0; 8];
    let read = tokio::time::timeout(Duration::from_secs(5), second.read(&mut buf))
        .await
        .expect("the shim closes the extra connection")
        .unwrap_or(0);
    assert_eq!(read, 0);
    shim.abort();
    held.abort();
}
